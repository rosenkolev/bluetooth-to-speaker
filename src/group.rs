//! Output group: one source played on several sound cards at once.
//!
//! Each card gets its own writer thread and ring buffer. Cards run on separate clocks,
//! so every writer keeps its queue (ring + ALSA buffer) near the target by dropping or
//! repeating a single frame now and then. That keeps all outputs in sync with the source
//! without resampling. Per-output delay lines outputs up with TVs that add latency.

use crate::app::{App, OutputStatus};
use crate::config::OutputCfg;
use crate::output::{to_s32, volume_db, HwVolume, Output, MIN_DB};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

struct Ring {
    buf: Mutex<VecDeque<i16>>,
    /// Signals data available (writer) and space available (producer).
    cv: Condvar,
    stop: AtomicBool,
    /// Stop without playing what is buffered.
    abort: AtomicBool,
    /// Writer is not playing (device missing); producer must not wait for it.
    dead: AtomicBool,
}

struct Writer {
    ring: Arc<Ring>,
    handle: Option<JoinHandle<()>>,
}

pub struct OutputGroup {
    writers: Vec<Writer>,
    pub rate: u32,
    pub channels: u32,
    /// Max samples per ring before the producer waits (network) or old data is dropped (Bluetooth).
    high_water: usize,
}

impl OutputGroup {
    pub fn open(app: &Arc<App>, outputs: &[OutputCfg], rate: u32, channels: u32, latency_ms: u32) -> OutputGroup {
        let enabled: Vec<OutputCfg> = outputs.iter().filter(|o| o.enabled).cloned().collect();
        let max_delay = enabled.iter().map(|o| o.delay_ms).max().unwrap_or(0) as usize;
        let high_water = (rate as usize * channels as usize * (latency_ms as usize + max_delay)) / 1000;
        let writers = enabled
            .into_iter()
            .enumerate()
            .map(|(idx, o)| {
                let ring = Arc::new(Ring {
                    buf: Mutex::new(VecDeque::with_capacity(high_water * 2)),
                    cv: Condvar::new(),
                    stop: AtomicBool::new(false),
                    abort: AtomicBool::new(false),
                    dead: AtomicBool::new(false),
                });
                let r = ring.clone();
                let app = app.clone();
                let handle = std::thread::Builder::new()
                    .name(format!("out-{idx}"))
                    .spawn(move || {
                        crate::bluez::raise_priority();
                        writer(app, o, r, rate, channels, latency_ms)
                    })
                    .ok();
                Writer { ring, handle }
            })
            .collect();
        OutputGroup { writers, rate, channels, high_water }
    }

    pub fn is_empty(&self) -> bool {
        self.writers.is_empty()
    }

    /// Push samples to every output. `block` waits for space (network sources decode faster
    /// than real time); without it the oldest data is dropped (Bluetooth is paced by the phone).
    pub fn write(&self, samples: &[i16], block: bool, stop: &AtomicBool) {
        for w in &self.writers {
            let mut buf = w.ring.buf.lock().unwrap();
            if block {
                while buf.len() > self.high_water
                    && !stop.load(Ordering::Relaxed)
                    && !w.ring.dead.load(Ordering::Relaxed)
                {
                    buf = w.ring.cv.wait_timeout(buf, Duration::from_millis(50)).unwrap().0;
                }
            }
            buf.extend(samples.iter().copied());
            let cap = self.high_water * 2;
            if buf.len() > cap {
                let excess = buf.len() - cap;
                let excess = excess - excess % self.channels as usize;
                buf.drain(..excess);
            }
            drop(buf);
            w.ring.cv.notify_all();
        }
    }

    /// Stop all outputs. `drain` plays what is buffered first (natural end of a stream).
    pub fn close(mut self, drain: bool) {
        if drain {
            let deadline = Instant::now() + Duration::from_secs(3);
            for w in &self.writers {
                let mut buf = w.ring.buf.lock().unwrap();
                while !buf.is_empty() && !w.ring.dead.load(Ordering::Relaxed) && Instant::now() < deadline {
                    buf = w.ring.cv.wait_timeout(buf, Duration::from_millis(50)).unwrap().0;
                }
            }
        }
        for w in &self.writers {
            if !drain {
                w.ring.abort.store(true, Ordering::Relaxed);
                w.ring.buf.lock().unwrap().clear();
            }
            w.ring.stop.store(true, Ordering::Relaxed);
            w.ring.cv.notify_all();
        }
        for w in &mut self.writers {
            if let Some(h) = w.handle.take() {
                let _ = h.join();
            }
        }
    }
}

impl Drop for OutputGroup {
    fn drop(&mut self) {
        for w in &self.writers {
            w.ring.abort.store(true, Ordering::Relaxed);
            w.ring.stop.store(true, Ordering::Relaxed);
            w.ring.cv.notify_all();
        }
    }
}

fn set_output_status(app: &App, device: &str, state: &str) {
    app.update_status(|s| {
        if let Some(o) = s.outputs.iter_mut().find(|o| o.device == device) {
            o.state = state.into();
        } else {
            s.outputs.push(OutputStatus { device: device.into(), state: state.into() });
        }
    });
}

fn writer(app: Arc<App>, cfg: OutputCfg, ring: Arc<Ring>, rate: u32, channels: u32, latency_ms: u32) {
    let ch = channels as usize;
    let period = (rate as usize * latency_ms as usize / 4000).max(64); // frames
                                                                       // Frames queued (ring + ALSA) we aim for: buffer plus this output's delay.
    let target = (rate as i64 * (latency_ms + cfg.delay_ms) as i64) / 1000;
    let tol = (rate as i64 * 10) / 1000; // ±10 ms
    let mut out: Option<Output> = None;
    let mut retry_at = Instant::now();
    let mut chunk: Vec<i16> = Vec::with_capacity(period * ch + ch);
    let mut avg_queue: f64 = target as f64;
    let mut out32: Vec<i32> = Vec::with_capacity(period * ch + ch);
    let mut gain: f32 = 1.0;
    let mut last_vols: (u8, u8) = (255, 255);
    // Hardware volume control, if configured; falls back to software.
    let hw = if cfg.mixer.is_empty() {
        None
    } else {
        match HwVolume::open(&cfg.device, &cfg.mixer) {
            Ok(h) => {
                log::info!("output {}: hardware volume via \"{}\"", cfg.device, h.control());
                Some(h)
            }
            Err(e) => {
                log::warn!("output {}: {e}; using software volume", cfg.device);
                None
            }
        }
    };

    loop {
        // Wait for a period of data (or stop).
        let mut buf = ring.buf.lock().unwrap();
        while buf.len() < period * ch && !ring.stop.load(Ordering::Relaxed) {
            buf = ring.cv.wait_timeout(buf, Duration::from_millis(20)).unwrap().0;
            if out.is_none() {
                break;
            }
        }
        if ring.stop.load(Ordering::Relaxed) && (buf.is_empty() || out.is_none()) {
            break;
        }
        let ring_frames = buf.len() / ch;

        // Device missing (TV off, HDMI unplugged): discard data, retry now and then.
        if out.is_none() {
            buf.clear();
            drop(buf);
            ring.cv.notify_all();
            if Instant::now() >= retry_at {
                match Output::open(&cfg.device, rate, channels, latency_ms) {
                    Ok(o) => {
                        log::info!("output {} open: {rate} Hz, {channels} ch", cfg.device);
                        // Delay line: silence ahead of the audio.
                        let silence = vec![0i32; (rate as usize * cfg.delay_ms as usize / 1000) * ch];
                        if !silence.is_empty() {
                            let _ = o.write(&silence);
                        }
                        ring.dead.store(false, Ordering::Relaxed);
                        set_output_status(&app, &cfg.device, "playing");
                        out = Some(o);
                    }
                    Err(e) => {
                        if !ring.dead.swap(true, Ordering::Relaxed) {
                            log::warn!("output {} unavailable: {e}", cfg.device);
                            set_output_status(&app, &cfg.device, &format!("unavailable: {e}"));
                        }
                        retry_at = Instant::now() + Duration::from_secs(3);
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
            } else {
                std::thread::sleep(Duration::from_millis(100));
            }
            continue;
        }

        let take = (period * ch).min(buf.len() - buf.len() % ch);
        chunk.clear();
        chunk.extend(buf.drain(..take));
        drop(buf);
        ring.cv.notify_all();
        if chunk.is_empty() {
            continue;
        }

        let o = out.as_ref().unwrap();
        // Drift correction: keep total queued frames near target.
        let queued = ring_frames as i64 + o.delay();
        avg_queue = avg_queue * 0.95 + queued as f64 * 0.05;
        if avg_queue > (target + tol) as f64 && chunk.len() > 2 * ch {
            chunk.truncate(chunk.len() - ch); // drop one frame
        } else if avg_queue < (target - tol) as f64 && ring_frames > 0 {
            let last: Vec<i16> = chunk[chunk.len() - ch..].to_vec();
            chunk.extend(last); // repeat one frame
        }

        // Volume: master × this output. Hardware mixer first, the rest in software.
        let vols = (app.volume(), app.output_volume(&cfg.device));
        if vols != last_vols {
            last_vols = vols;
            let db = match (volume_db(vols.0), volume_db(vols.1)) {
                (Some(a), Some(b)) => Some((a + b).max(2.0 * MIN_DB)),
                _ => None,
            };
            gain = match (&hw, db) {
                (Some(hw), db) => {
                    let rest = hw.set(db);
                    if db.is_none() {
                        0.0
                    } else {
                        10f32.powf(rest / 20.0)
                    }
                }
                (None, Some(db)) => 10f32.powf(db / 20.0),
                (None, None) => 0.0,
            };
        }
        to_s32(&chunk, gain, &mut out32);
        match o.write(&out32) {
            Ok(true) => {
                app.stats.underruns.fetch_add(1, Ordering::Relaxed);
            }
            Ok(false) => {}
            Err(e) => {
                log::warn!("output {} error: {e}", cfg.device);
                set_output_status(&app, &cfg.device, &format!("error: {e}"));
                out = None;
                ring.dead.store(true, Ordering::Relaxed);
                retry_at = Instant::now() + Duration::from_secs(3);
            }
        }
    }
    if let Some(o) = out {
        if ring.abort.load(Ordering::Relaxed) {
            o.drop_now();
        } else {
            o.drain();
        }
    }
    set_output_status(&app, &cfg.device, "idle");
}
