//! Output group: one source played on several sound cards at once.
//!
//! Each card gets its own writer thread and ring buffer. Cards run on separate clocks,
//! so every writer keeps its queue (ring + ALSA buffer) near the target by dropping or
//! repeating a single frame now and then. That keeps all outputs in sync with the source
//! without resampling. Per-output delay lines outputs up with TVs that add latency.
//!
//! Protecting the speakers from clicks and pops:
//! - A card is never left to run dry: with no data, the writer plays silence, so gaps and
//!   pauses do not end in an underrun (a click, and the card stops and restarts).
//! - Between songs the outputs stay open, playing silence, for `LINGER` (the group is
//!   "parked" in `App`); the next stream takes them over. Opening and closing a card
//!   switches its amplifier on and off, which pops on many boards.
//! - Audio fades in after silence, and fades out when a source is cut off.

use crate::app::{App, OutputStatus};
use crate::config::OutputCfg;
use crate::output::{to_s32, volume_db, HwVolume, Output, MIN_DB};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// An output with nothing to play stays open this long (silence), then closes.
pub const LINGER: Duration = Duration::from_secs(10);
/// Fade-in after silence, fade-out when a source is cut off.
const FADE_IN_MS: usize = 20;
const FADE_OUT_MS: usize = 30;
/// A stopped source hands its outputs over (parks them) at once. If it does not (stuck in
/// a network read), the writers close the cards after this, so the next source gets them.
const ORPHAN_GRACE: Duration = Duration::from_secs(1);

/// Who feeds a ring. Changed when a parked group is taken over by the next source.
struct Ctl {
    source_stop: Arc<AtomicBool>,
    /// Kept open between streams; no source.
    parked: bool,
    /// The writer has finished; the group cannot be reused.
    gone: bool,
}

struct Ring {
    buf: Mutex<VecDeque<i16>>,
    /// Signals data available (writer) and space available (producer).
    cv: Condvar,
    stop: AtomicBool,
    /// Stop without playing what is buffered.
    abort: AtomicBool,
    /// Writer is not playing (device missing); producer must not wait for it.
    dead: AtomicBool,
    ctl: Mutex<Ctl>,
}

impl Ring {
    fn aborted(&self) -> bool {
        self.abort.load(Ordering::Relaxed)
    }

    /// (source stopped, parked)
    fn source(&self) -> (bool, bool) {
        let c = self.ctl.lock().unwrap();
        (c.source_stop.load(Ordering::Relaxed), c.parked)
    }
}

struct Writer {
    ring: Arc<Ring>,
    handle: Option<JoinHandle<()>>,
}

pub struct OutputGroup {
    writers: Vec<Writer>,
    pub rate: u32,
    pub channels: u32,
    latency_ms: u32,
    /// `App::outputs_gen` the group was opened with.
    gen: u64,
    /// Max samples per ring before the producer waits (network) or old data is dropped (Bluetooth).
    high_water: usize,
}

impl OutputGroup {
    /// Open the enabled outputs, or take over the parked ones when they match.
    pub fn open(
        app: &Arc<App>,
        outputs: &[OutputCfg],
        rate: u32,
        channels: u32,
        latency_ms: u32,
        source_stop: &Arc<AtomicBool>,
    ) -> OutputGroup {
        let gen = app.outputs_gen.load(Ordering::Relaxed);
        if let Some(g) = app.take_parked(rate, channels, gen, latency_ms) {
            if g.adopt(source_stop) {
                return g;
            }
            g.close(false);
        }
        // Different format or outputs: free the cards for the new group.
        app.evict_parked();

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
                    ctl: Mutex::new(Ctl { source_stop: source_stop.clone(), parked: false, gone: false }),
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
        OutputGroup { writers, rate, channels, latency_ms, gen, high_water }
    }

    pub fn is_empty(&self) -> bool {
        self.writers.is_empty()
    }

    /// Can a stream with this format play on the group as it is?
    pub fn compatible(&self, rate: u32, channels: u32, gen: u64, latency_ms: u32) -> bool {
        self.rate == rate && self.channels == channels && self.gen == gen && self.latency_ms == latency_ms
    }

    /// All writers still run.
    pub fn alive(&self) -> bool {
        self.writers
            .iter()
            .all(|w| w.handle.as_ref().is_some_and(|h| !h.is_finished()) && !w.ring.ctl.lock().unwrap().gone)
    }

    /// Keep the outputs open without a source (playing silence). False if a writer has
    /// already finished; then the group must be closed instead.
    pub fn park(&self) -> bool {
        let mut ctls: Vec<_> = self.writers.iter().map(|w| w.ring.ctl.lock().unwrap()).collect();
        if ctls.iter().any(|c| c.gone) {
            return false;
        }
        for c in ctls.iter_mut() {
            c.parked = true;
        }
        true
    }

    /// A new source takes over the parked outputs.
    fn adopt(&self, source_stop: &Arc<AtomicBool>) -> bool {
        let mut ctls: Vec<_> = self.writers.iter().map(|w| w.ring.ctl.lock().unwrap()).collect();
        if ctls.iter().any(|c| c.gone) {
            return false;
        }
        for c in ctls.iter_mut() {
            c.source_stop = source_stop.clone();
            c.parked = false;
        }
        true
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

    /// Wait (up to 3 s) until the writers have taken everything buffered (end of a stream).
    pub fn drain_rings(&self) {
        let deadline = Instant::now() + Duration::from_secs(3);
        for w in &self.writers {
            let mut buf = w.ring.buf.lock().unwrap();
            while !buf.is_empty() && !w.ring.dead.load(Ordering::Relaxed) && Instant::now() < deadline {
                buf = w.ring.cv.wait_timeout(buf, Duration::from_millis(50)).unwrap().0;
            }
        }
    }

    /// Stop all outputs. `drain` plays what is buffered first.
    pub fn close(mut self, drain: bool) {
        if drain {
            self.drain_rings();
        }
        for w in &self.writers {
            if !drain {
                w.ring.abort.store(true, Ordering::Relaxed);
                w.ring.buf.lock().unwrap().clear();
            }
            w.ring.stop.store(true, Ordering::Relaxed);
            w.ring.cv.notify_all();
        }
        // Wait for the writers to close their sound cards, but never hang: a driver that
        // blocks in a write must not freeze the source thread. Stuck writers are detached.
        let deadline = Instant::now() + Duration::from_secs(2);
        while self.writers.iter().any(|w| w.handle.as_ref().is_some_and(|h| !h.is_finished()))
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        for w in &mut self.writers {
            if let Some(h) = w.handle.take() {
                if h.is_finished() {
                    let _ = h.join();
                } else {
                    log::warn!("an output did not close in time; leaving it to finish on its own");
                }
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

/// Fade the start of `samples` (interleaved) in over `frames` frames, from `done` frames on.
/// Returns the frames of the fade still to do.
fn fade_in(samples: &mut [i32], ch: usize, frames: usize, done: usize) -> usize {
    let mut i = done;
    for frame in samples.chunks_exact_mut(ch) {
        if i >= frames {
            break;
        }
        let g = i as f32 / frames as f32;
        for s in frame {
            *s = (*s as f32 * g) as i32;
        }
        i += 1;
    }
    i
}

/// Keep `frames` frames of the ring, faded out to zero, and drop the rest.
fn fade_out_ring(ring: &Ring, ch: usize, frames: usize) {
    let mut buf = ring.buf.lock().unwrap();
    let keep = (buf.len() / ch).min(frames);
    buf.truncate(keep * ch);
    for (i, frame) in buf.make_contiguous().chunks_exact_mut(ch).enumerate() {
        let g = 1.0 - (i + 1) as f32 / keep.max(1) as f32;
        for s in frame {
            *s = (*s as f32 * g) as i16;
        }
    }
}

fn writer(app: Arc<App>, cfg: OutputCfg, ring: Arc<Ring>, rate: u32, channels: u32, latency_ms: u32) {
    let ch = channels as usize;
    let period = (rate as usize * latency_ms as usize / 4000).max(64); // frames
                                                                       // Frames queued (ring + ALSA) we aim for: buffer plus this output's delay.
    let target = (rate as i64 * (latency_ms + cfg.delay_ms) as i64) / 1000;
    let tol = (rate as i64 * 10) / 1000; // ±10 ms
    let fade_in_frames = (rate as usize * FADE_IN_MS / 1000).max(1);
    let fade_out_frames = (rate as usize * FADE_OUT_MS / 1000).max(1);
    let silence = vec![0i32; period * ch];
    let mut out: Option<Output> = None;
    let mut retry_at = Instant::now();
    let mut chunk: Vec<i16> = Vec::with_capacity(period * ch + ch);
    let mut avg_queue: f64 = target as f64;
    let mut out32: Vec<i32> = Vec::with_capacity(period * ch + ch);
    let mut gain: f32 = 1.0;
    let mut last_vols: (u8, u8) = (255, 255);
    // Frames of fade-in done; a fade starts whenever audio follows silence.
    let mut faded = 0usize;
    let mut starved_since: Option<Instant> = None;
    let mut orphan_since: Option<Instant> = None;
    // Closed after a long silence; reopens when audio comes.
    let mut idle_closed = false;
    // State last shown in the UI ("playing", "idle"); empty after an error message.
    let mut shown = String::new();
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
    let status = |shown: &mut String, state: &str| {
        if shown != state {
            *shown = state.to_string();
            set_output_status(&app, &cfg.device, state);
        }
    };

    // The previous source may still hold the card for a moment: retry quickly at first.
    let started = Instant::now();
    loop {
        if ring.aborted() {
            break;
        }
        // The source was cut off (another source took over, or Stop): fade out what is
        // buffered. Its thread parks the group at once; if it does not, close the card.
        let (source_stopped, parked) = ring.source();
        if source_stopped && !parked {
            match orphan_since {
                None => {
                    orphan_since = Some(Instant::now());
                    fade_out_ring(&ring, ch, fade_out_frames);
                }
                Some(t) if t.elapsed() > ORPHAN_GRACE => {
                    let mut c = ring.ctl.lock().unwrap();
                    if !c.parked {
                        c.gone = true;
                        break;
                    }
                }
                _ => {}
            }
        } else {
            orphan_since = None;
        }

        // Wait for a period of data, but never let the card run dry.
        let mut buf = ring.buf.lock().unwrap();
        while buf.len() < period * ch && !ring.stop.load(Ordering::Relaxed) && !ring.aborted() {
            match &out {
                None => break,
                Some(o) if o.delay() < period as i64 => break,
                Some(_) => {}
            }
            buf = ring.cv.wait_timeout(buf, Duration::from_millis(10)).unwrap().0;
            if source_stopped != ring.source().0 {
                break; // source changed: handle it first
            }
        }
        if ring.aborted() || (ring.stop.load(Ordering::Relaxed) && (buf.len() < ch || out.is_none())) {
            break;
        }

        // No device: closed after a long silence (reopen when audio comes), or missing
        // (TV off, HDMI unplugged: discard data, retry now and then).
        if out.is_none() {
            if idle_closed && buf.len() < ch {
                drop(buf);
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
            if !idle_closed {
                buf.clear();
            }
            drop(buf);
            ring.cv.notify_all();
            if Instant::now() >= retry_at {
                match Output::open(&cfg.device, rate, channels, latency_ms) {
                    Ok(o) => {
                        log::info!("output {} open: {rate} Hz, {channels} ch", cfg.device);
                        // Delay line: silence ahead of the audio.
                        let delay = vec![0i32; (rate as usize * cfg.delay_ms as usize / 1000) * ch];
                        if !delay.is_empty() {
                            let _ = o.write(&delay);
                        }
                        ring.dead.store(false, Ordering::Relaxed);
                        idle_closed = false;
                        faded = 0;
                        starved_since = None;
                        out = Some(o);
                    }
                    Err(e) if started.elapsed() < Duration::from_secs(2) => {
                        // Usually "device busy" while the previous source closes it, or
                        // outputs kept open for a stream of another format.
                        log::debug!("output {} not ready yet: {e}", cfg.device);
                        if !ring.source().1 {
                            app.evict_parked();
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    Err(e) => {
                        if !ring.dead.swap(true, Ordering::Relaxed) {
                            log::warn!("output {} unavailable: {e}", cfg.device);
                            status(&mut shown, &format!("unavailable: {e}"));
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

        let ring_frames = buf.len() / ch;
        let take = (period * ch).min(buf.len() - buf.len() % ch);
        chunk.clear();
        chunk.extend(buf.drain(..take));
        drop(buf);
        ring.cv.notify_all();
        let o = out.as_ref().unwrap();

        if chunk.is_empty() {
            if o.delay() >= period as i64 {
                continue; // woken early (the source changed); the card has enough
            }
            // Nothing to play and the card is running low: play silence.
            if let Err(e) = o.write(&silence) {
                log::warn!("output {} error: {e}", cfg.device);
                status(&mut shown, &format!("error: {e}"));
                out = None;
                ring.dead.store(true, Ordering::Relaxed);
                retry_at = Instant::now() + Duration::from_secs(3);
                continue;
            }
            faded = 0;
            let since = *starved_since.get_or_insert_with(Instant::now);
            // Idle at once when the source is gone; after a second for a gap in the data.
            if source_stopped || parked || since.elapsed() > Duration::from_secs(1) {
                status(&mut shown, "idle");
            }
            if since.elapsed() > LINGER {
                // A long silence (paused): close the card, so its amplifier can power down.
                log::info!("output {} closed after {}s of silence", cfg.device, LINGER.as_secs());
                o.drop_now();
                out = None;
                idle_closed = true;
                avg_queue = target as f64;
            }
            continue;
        }

        if faded == 0 {
            // Audio after silence: build the queue up to the target again first, so the
            // next gap in the data does not run the card dry.
            let queued = o.delay();
            if queued < target {
                let frames = (target - queued) as usize;
                let zeros = vec![0i32; frames * ch];
                let _ = o.write(&zeros);
            }
            avg_queue = target as f64;
        }
        starved_since = None;
        status(&mut shown, "playing");

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
        if faded < fade_in_frames {
            faded = fade_in(&mut out32, ch, fade_in_frames, faded);
        }
        match o.write(&out32) {
            Ok(true) => {
                app.stats.underruns.fetch_add(1, Ordering::Relaxed);
            }
            Ok(false) => {}
            Err(e) => {
                log::warn!("output {} error: {e}", cfg.device);
                status(&mut shown, &format!("error: {e}"));
                out = None;
                ring.dead.store(true, Ordering::Relaxed);
                retry_at = Instant::now() + Duration::from_secs(3);
            }
        }
    }
    ring.ctl.lock().unwrap().gone = true;
    if let Some(o) = out {
        if ring.aborted() {
            o.drop_now();
        } else {
            o.drain();
        }
    }
    status(&mut shown, "idle");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fades() {
        let mut s = vec![1000i32; 8]; // 4 stereo frames
        assert_eq!(fade_in(&mut s, 2, 2, 0), 2);
        assert_eq!(s, vec![0, 0, 500, 500, 1000, 1000, 1000, 1000]);

        let ring = Ring {
            buf: Mutex::new(VecDeque::from(vec![1000i16; 20])),
            cv: Condvar::new(),
            stop: AtomicBool::new(false),
            abort: AtomicBool::new(false),
            dead: AtomicBool::new(false),
            ctl: Mutex::new(Ctl { source_stop: Arc::new(AtomicBool::new(false)), parked: false, gone: false }),
        };
        fade_out_ring(&ring, 2, 4);
        let buf: Vec<i16> = ring.buf.lock().unwrap().iter().copied().collect();
        assert_eq!(buf, vec![750, 750, 500, 500, 250, 250, 0, 0]);
    }
}
