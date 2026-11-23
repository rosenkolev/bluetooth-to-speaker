//! Audio playback: one owner of the output at a time ("last source wins").

use crate::app::{App, BtCmd, Source, Stream};
use crate::group::OutputGroup;
use crate::sbc;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct Owner {
    id: u64,
    source: Source,
    /// Bluetooth device address for Bluetooth leases.
    device: Option<String>,
    stop: Arc<AtomicBool>,
}

/// Who owns the outputs. A new source stops the old one; the old source's output writers
/// see its stop flag and free the sound cards, and the new writers retry opening them
/// for a moment. No lock is held while playing, so a stuck source cannot block the next.
#[derive(Default)]
pub struct OutputArbiter {
    owner: Mutex<Option<Owner>>,
    next_id: AtomicU64,
}

pub struct Lease {
    id: u64,
    pub stop: Arc<AtomicBool>,
}

impl Lease {
    pub fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }
}

pub struct Preempted {
    pub source: Source,
    pub device: Option<String>,
}

impl OutputArbiter {
    /// Take the output. Stops the current owner and reports who it was.
    pub fn claim(&self, source: Source, device: Option<String>) -> (Lease, Option<Preempted>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let stop = Arc::new(AtomicBool::new(false));
        let mut owner = self.owner.lock().unwrap();
        let prev = owner.take().map(|o| {
            o.stop.store(true, Ordering::Relaxed);
            Preempted { source: o.source, device: o.device }
        });
        *owner = Some(Owner { id, source, device, stop: stop.clone() });
        (Lease { id, stop }, prev)
    }

    pub fn release(&self, lease: &Lease) {
        let mut owner = self.owner.lock().unwrap();
        if owner.as_ref().map(|o| o.id) == Some(lease.id) {
            *owner = None;
        }
    }

    pub fn current(&self) -> Source {
        self.owner.lock().unwrap().as_ref().map(|o| o.source).unwrap_or(Source::Idle)
    }

    /// Stop whoever owns the output (used by DLNA Stop).
    pub fn stop_source(&self, source: Source) {
        let owner = self.owner.lock().unwrap();
        if let Some(o) = owner.as_ref().filter(|o| o.source == source) {
            o.stop.store(true, Ordering::Relaxed);
        }
    }
}

/// Bluetooth A2DP stream: RTP/SBC packets from the transport fd to ALSA.
pub fn run_bt_stream(app: Arc<App>, fd: OwnedFd, read_mtu: usize, lease: Lease) {
    app.playback_starting();
    let mut dec = sbc::Decoder::new();
    let mut out: Option<OutputGroup> = None;
    let mut out_gen = 0u64;
    let mut packet = vec![0u8; read_mtu.max(1024)];
    let mut frame = [0u8; sbc::MAX_FRAME_PCM_BYTES];
    let mut pcm: Vec<i16> = Vec::with_capacity(4096);
    let raw = fd.as_raw_fd();
    let mut preempted = true;

    log::info!("bluetooth stream started (mtu {read_mtu})");
    loop {
        if lease.stopped() {
            break;
        }
        let mut pfd = libc::pollfd { fd: raw, events: libc::POLLIN, revents: 0 };
        let r = unsafe { libc::poll(&mut pfd, 1, 500) };
        if r == 0 {
            continue;
        }
        if r < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if pfd.revents & (libc::POLLHUP | libc::POLLERR) != 0 && pfd.revents & libc::POLLIN == 0 {
            preempted = false;
            break;
        }
        let n = unsafe { libc::read(raw, packet.as_mut_ptr() as *mut libc::c_void, packet.len()) };
        if n <= 0 {
            let err = std::io::Error::last_os_error();
            if n < 0 && matches!(err.kind(), std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock) {
                continue;
            }
            preempted = false;
            break;
        }
        app.stats.packets.fetch_add(1, Ordering::Relaxed);
        let Some(payload) = rtp_payload(&packet[..n as usize]) else {
            app.stats.decode_errors.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        // A2DP SBC media payload header: F|S|L|RFA|frames(4)
        if payload.is_empty() || payload[0] & 0x80 != 0 {
            continue; // fragmented frames are not used by phones
        }
        let mut data = &payload[1..];
        pcm.clear();
        while !data.is_empty() {
            match dec.decode_frame(data, &mut frame) {
                Some((used, written)) => {
                    pcm.extend(frame[..written].chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])));
                    data = &data[used..];
                }
                None => {
                    app.stats.decode_errors.fetch_add(1, Ordering::Relaxed);
                    break;
                }
            }
        }
        if pcm.is_empty() {
            continue;
        }
        let (rate, ch) = (dec.sample_rate(), dec.channels());
        let gen = app.outputs_gen.load(Ordering::Relaxed);
        if out.as_ref().map(|o| o.rate != rate || o.channels != ch || out_gen != gen).unwrap_or(true) {
            if let Some(old) = out.take() {
                old.close(false);
            }
            let cfg = app.cfg();
            log::info!("bluetooth stream: SBC {rate} Hz, {ch} ch, bitpool {}", dec.bitpool());
            let bitpool = dec.bitpool();
            app.update_status(|s| {
                s.stream = Some(Stream { codec: "SBC".into(), sample_rate: rate, channels: ch, bitpool })
            });
            out = Some(OutputGroup::open(&app, &cfg.outputs, rate, ch, cfg.latency_ms, &lease.stop));
            out_gen = gen;
        }
        if let Some(o) = &out {
            o.write(&pcm, false, &lease.stop);
        }
    }
    // Keep the outputs open for what comes next. Cut off: the writers fade out on their own;
    // the phone stopped: play what is buffered first.
    if let Some(o) = out {
        if !preempted {
            o.drain_rings();
        }
        app.park_outputs(o);
    }
    app.output.release(&lease);
    app.playback_stopped();
    app.update_status(|s| s.stream = None);
    log::info!("bluetooth stream ended");
}

/// Strip the RTP header. Returns the media payload.
fn rtp_payload(p: &[u8]) -> Option<&[u8]> {
    if p.len() < 13 || p[0] >> 6 != 2 {
        return None;
    }
    let cc = (p[0] & 0x0f) as usize;
    let mut off = 12 + cc * 4;
    if p[0] & 0x10 != 0 {
        let ext = p.get(off + 2..off + 4)?;
        off += 4 + 4 * u16::from_be_bytes([ext[0], ext[1]]) as usize;
    }
    let mut end = p.len();
    if p[0] & 0x20 != 0 {
        end = end.checked_sub(*p.last()? as usize)?;
    }
    p.get(off..end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_rtp_header() {
        let mut p = vec![0x80, 0x60, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0];
        p.extend([0x05, 0x9c, 0xbd]);
        assert_eq!(rtp_payload(&p), Some(&[0x05, 0x9c, 0xbd][..]));
        assert_eq!(rtp_payload(&[0x40; 20]), None); // wrong RTP version
    }

    #[test]
    fn last_source_wins() {
        let arb = OutputArbiter::default();
        let (bt, prev) = arb.claim(Source::Bluetooth, Some("AA".into()));
        assert!(prev.is_none());
        let (net, prev) = arb.claim(Source::Network, None);
        assert!(bt.stopped());
        let prev = prev.unwrap();
        assert_eq!(prev.source, Source::Bluetooth);
        assert_eq!(prev.device.as_deref(), Some("AA"));
        arb.release(&bt); // stale lease must not clear the new owner
        assert_eq!(arb.current(), Source::Network);
        arb.release(&net);
        assert_eq!(arb.current(), Source::Idle);
    }
}

/// Shared state of the network (DLNA) player.
#[derive(Default)]
pub struct NetState {
    pub paused: AtomicBool,
    /// Milliseconds played.
    pub position_ms: AtomicU64,
    pub duration_ms: AtomicU64,
    pub playing: AtomicBool,
    /// Phone paused by network playback, resumed when it ends.
    pub resume: Mutex<Option<String>>,
}

/// Network playback (DLNA / Home Assistant / Music Assistant): HTTP URL → decode → ALSA.
pub fn run_network(app: Arc<App>, uri: String, net: Arc<NetState>, lease: Lease) {
    app.playback_starting();
    net.playing.store(true, Ordering::Relaxed);
    net.position_ms.store(0, Ordering::Relaxed);
    net.duration_ms.store(0, Ordering::Relaxed);
    if let Err(e) = play_url(&app, &uri, &net, &lease) {
        log::warn!("network playback failed: {e}");
    }
    app.output.release(&lease);
    app.playback_stopped();
    if app.output.current() == Source::Network {
        return; // replaced by the next URI; that stream owns the state now
    }
    net.playing.store(false, Ordering::Relaxed);
    net.paused.store(false, Ordering::Relaxed);
    app.update_status(|s| {
        s.stream = None;
        if let Some(n) = s.network.as_mut() {
            n.state = "STOPPED".into();
        }
    });
    // Give the phone back its music after a short pause, unless a controller queues the
    // next track meanwhile or the phone itself took the output.
    std::thread::sleep(Duration::from_secs(3));
    match app.output.current() {
        Source::Idle => {
            if let Some(dev) = net.resume.lock().unwrap().take() {
                let _ = app.bt_tx.send(BtCmd::Resume(dev));
            }
        }
        Source::Bluetooth => {
            net.resume.lock().unwrap().take();
        }
        Source::Network => {}
    }
}

fn play_url(app: &Arc<App>, uri: &str, net: &NetState, lease: &Lease) -> Result<(), String> {
    use symphonia::core::audio::SampleBuffer;
    use symphonia::core::codecs::DecoderOptions;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions, ReadOnlySource};
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;

    let resp = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(5))
        .timeout_read(Duration::from_secs(30))
        .build()
        .get(uri)
        .call()
        .map_err(|e| e.to_string())?;
    let mut hint = Hint::new();
    let ctype = resp.content_type().to_lowercase();
    let ext = match ctype.as_str() {
        "audio/mpeg" | "audio/mp3" => Some("mp3"),
        "audio/flac" | "audio/x-flac" => Some("flac"),
        "audio/aac" | "audio/aacp" => Some("aac"),
        "audio/mp4" | "audio/m4a" | "audio/x-m4a" => Some("m4a"),
        "audio/ogg" | "application/ogg" => Some("ogg"),
        "audio/wav" | "audio/x-wav" | "audio/wave" => Some("wav"),
        _ => None,
    };
    if let Some(e) = ext {
        hint.with_extension(e);
    } else if let Some(e) = uri.split('?').next().and_then(|p| p.rsplit('.').next()) {
        if e.len() <= 4 {
            hint.with_extension(e);
        }
    }
    let reader = resp.into_reader();
    let mss = MediaSourceStream::new(
        Box::new(ReadOnlySource::new(reader)),
        MediaSourceStreamOptions { buffer_len: 256 * 1024 },
    );
    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .map_err(|e| format!("unsupported stream: {e}"))?;
    let mut format = probed.format;
    let track = format.default_track().ok_or("no audio track")?.clone();
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| e.to_string())?;
    if let (Some(frames), Some(rate)) = (track.codec_params.n_frames, track.codec_params.sample_rate) {
        net.duration_ms.store(frames * 1000 / rate as u64, Ordering::Relaxed);
    }

    let codec_name = symphonia::default::get_codecs()
        .get_codec(track.codec_params.codec)
        .map(|c| c.short_name.to_uppercase())
        .unwrap_or_else(|| "?".into());
    let mut out: Option<OutputGroup> = None;
    let mut out_gen = 0u64;
    let mut sbuf: Option<SampleBuffer<i16>> = None;
    let mut sbuf_spec = None;
    let mut frames_played: u64 = 0;
    let result = loop {
        if lease.stopped() {
            break Ok(());
        }
        while net.paused.load(Ordering::Relaxed) && !lease.stopped() {
            std::thread::sleep(Duration::from_millis(100));
        }
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(symphonia::core::errors::Error::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                break Ok(());
            }
            Err(e) => break Err(e.to_string()),
        };
        if packet.track_id() != track.id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(symphonia::core::errors::Error::DecodeError(_)) => continue,
            Err(e) => break Err(e.to_string()),
        };
        let spec = *decoded.spec();
        let ch = spec.channels.count() as u32;
        if ch == 0 || spec.rate == 0 {
            continue;
        }
        // New buffer when the packet is bigger or the format changed (a mismatch panics).
        if sbuf_spec != Some(spec) || sbuf.as_ref().is_none_or(|b| b.capacity() < decoded.capacity() * ch as usize) {
            sbuf = Some(SampleBuffer::new(decoded.capacity() as u64, spec));
            sbuf_spec = Some(spec);
        }
        let buf = sbuf.as_mut().unwrap();
        buf.copy_interleaved_ref(decoded);
        let samples = buf.samples();
        let gen = app.outputs_gen.load(Ordering::Relaxed);
        if out.as_ref().map(|o| o.rate != spec.rate || o.channels != ch || out_gen != gen).unwrap_or(true) {
            if let Some(old) = out.take() {
                old.close(false);
            }
            let cfg = app.cfg();
            app.update_status(|s| {
                s.stream = Some(Stream { codec: codec_name.clone(), sample_rate: spec.rate, channels: ch, bitpool: 0 })
            });
            out = Some(OutputGroup::open(app, &cfg.outputs, spec.rate, ch, cfg.latency_ms, &lease.stop));
            out_gen = gen;
        }
        let o = out.as_ref().unwrap();
        if o.is_empty() {
            // No output enabled: keep real-time pace so position and state stay sensible.
            std::thread::sleep(Duration::from_millis((samples.len() / ch as usize) as u64 * 1000 / spec.rate as u64));
        } else {
            o.write(samples, true, &lease.stop);
        }
        frames_played += (samples.len() / ch as usize) as u64;
        net.position_ms.store(frames_played * 1000 / spec.rate as u64, Ordering::Relaxed);
    };
    // Keep the outputs open for the next song. Stopped or replaced: the writers fade out on
    // their own; the end of the song: play what is buffered first.
    if let Some(o) = out {
        if !lease.stopped() {
            o.drain_rings();
        }
        app.park_outputs(o);
    }
    result
}
