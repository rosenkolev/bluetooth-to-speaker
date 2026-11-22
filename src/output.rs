//! ALSA output, volume and hardware mixers. The ALSA buffer doubles as the jitter buffer.

use alsa::mixer::{MilliBel, Mixer, Selem, SelemId};
use alsa::pcm::{Access, Format, HwParams, PCM};
use alsa::{Direction, Round, ValueOr};
use serde::Serialize;

/// Lowest volume step in dB; volume 0 is mute.
pub const MIN_DB: f32 = -60.0;

pub struct Output {
    pcm: PCM,
    channels: u32,
}

impl Output {
    /// Opens the device for 32-bit samples so software volume keeps full resolution.
    /// `plughw:` devices convert to whatever the card needs.
    pub fn open(device: &str, rate: u32, channels: u32, latency_ms: u32) -> Result<Output, alsa::Error> {
        let pcm = PCM::new(device, Direction::Playback, false)?;
        {
            let hwp = HwParams::any(&pcm)?;
            hwp.set_access(Access::RWInterleaved)?;
            hwp.set_format(Format::S32LE)?;
            hwp.set_channels(channels)?;
            hwp.set_rate(rate, ValueOr::Nearest)?;
            hwp.set_buffer_time_near(latency_ms * 1000, ValueOr::Nearest)?;
            hwp.set_period_time_near(latency_ms * 1000 / 4, ValueOr::Nearest)?;
            pcm.hw_params(&hwp)?;
        }
        {
            let hwp = pcm.hw_params_current()?;
            let buffer = hwp.get_buffer_size()?;
            let period = hwp.get_period_size()?;
            let swp = pcm.sw_params_current()?;
            // Start once the buffer is almost full: this is the jitter buffer prefill.
            swp.set_start_threshold(buffer - period)?;
            swp.set_avail_min(period)?;
            pcm.sw_params(&swp)?;
        }
        Ok(Output { pcm, channels })
    }

    /// Blocking write of interleaved S32 samples. Returns true on underrun.
    pub fn write(&self, samples: &[i32]) -> Result<bool, alsa::Error> {
        let io = self.pcm.io_i32()?;
        let frames = samples.len() / self.channels as usize;
        let mut offset = 0usize;
        let mut underrun = false;
        while offset < frames {
            let start = offset * self.channels as usize;
            match io.writei(&samples[start..]) {
                Ok(n) => offset += n,
                Err(e) => {
                    underrun = true;
                    self.pcm.try_recover(e, true)?;
                }
            }
        }
        Ok(underrun)
    }

    /// Frames queued in the device buffer.
    pub fn delay(&self) -> i64 {
        self.pcm.delay().unwrap_or(0).max(0)
    }

    pub fn drain(&self) {
        let _ = self.pcm.drain();
    }

    pub fn drop_now(&self) {
        let _ = self.pcm.drop();
    }
}

/// Volume 0..100 to dB on a 60 dB perceptual scale. 0 = mute (None).
pub fn volume_db(volume: u8) -> Option<f32> {
    if volume == 0 {
        None
    } else {
        Some(MIN_DB * (1.0 - volume.min(100) as f32 / 100.0))
    }
}

/// Convert S16 samples to S32 with a linear gain.
pub fn to_s32(src: &[i16], gain: f32, dst: &mut Vec<i32>) {
    dst.clear();
    if gain >= 0.9999 {
        dst.extend(src.iter().map(|&s| (s as i32) << 16));
    } else {
        let g = gain * 65536.0;
        dst.extend(src.iter().map(|&s| (s as f32 * g) as i32));
    }
}

/// A hardware volume control (ALSA simple mixer element) for one output.
pub struct HwVolume {
    mixer: Mixer,
    control: String,
    min_db: f32,
    max_db: f32,
    has_switch: bool,
}

impl HwVolume {
    /// `want` = "auto" or a control name.
    pub fn open(device: &str, want: &str) -> Result<HwVolume, String> {
        let card = mixer_card(device);
        let mixer = Mixer::new(&card, false).map_err(|e| format!("mixer {card}: {e}"))?;
        let names: Vec<String> =
            if want.eq_ignore_ascii_case("auto") { playback_controls(&mixer) } else { vec![want.to_string()] };
        let preferred = ["Master", "DAC", "PCM", "Speaker", "Headphone", "Digital"];
        let mut ordered: Vec<&String> =
            preferred.iter().filter_map(|p| names.iter().find(|n| n.starts_with(p))).collect();
        let rest: Vec<&String> = names.iter().filter(|n| !ordered.contains(n)).collect();
        ordered.extend(rest);
        for name in ordered {
            let Some(selem) = mixer.find_selem(&SelemId::new(name, 0)) else { continue };
            if !selem.has_playback_volume() {
                continue;
            }
            let (lo, hi) = selem.get_playback_db_range();
            if hi.0 <= lo.0 {
                continue;
            }
            let has_switch = selem.has_playback_switch();
            let control = name.clone();
            return Ok(HwVolume {
                control,
                min_db: lo.0 as f32 / 100.0,
                max_db: hi.0 as f32 / 100.0,
                has_switch,
                mixer,
            });
        }
        Err(format!("no usable volume control on {card}"))
    }

    pub fn control(&self) -> &str {
        &self.control
    }

    /// Set the control to `db` (relative to the control's maximum = 0 dB), or mute.
    /// Returns the part the hardware could not reach, to apply in software.
    pub fn set(&self, db: Option<f32>) -> f32 {
        let _ = self.mixer.handle_events();
        let Some(selem) = self.mixer.find_selem(&SelemId::new(&self.control, 0)) else { return db.unwrap_or(MIN_DB) };
        match db {
            None => {
                if self.has_switch {
                    let _ = selem.set_playback_switch_all(0);
                } else {
                    let _ = selem.set_playback_db_all(MilliBel((self.min_db * 100.0) as i64), Round::Floor);
                }
                0.0
            }
            Some(db) => {
                let target = (self.max_db + db).max(self.min_db);
                let _ = selem.set_playback_db_all(MilliBel((target * 100.0) as i64), Round::Floor);
                if self.has_switch {
                    let _ = selem.set_playback_switch_all(1);
                }
                // Hardware floor reached: the rest is done in software.
                (self.max_db + db - target).min(0.0)
            }
        }
    }
}

/// "plughw:CARD=x,DEV=0" → "hw:CARD=x"; "hw:1,0" → "hw:1"; anything else as is.
fn mixer_card(device: &str) -> String {
    let d = device.strip_prefix("plug").unwrap_or(device);
    if let Some(rest) = d.strip_prefix("hw:") {
        let card = rest.split(',').next().unwrap_or(rest);
        format!("hw:{card}")
    } else {
        device.to_string()
    }
}

fn playback_controls(mixer: &Mixer) -> Vec<String> {
    mixer
        .iter()
        .filter_map(Selem::new)
        .filter(|s| s.has_playback_volume() && !s.has_capture_volume())
        .filter(|s| {
            let (lo, hi) = s.get_playback_db_range();
            hi.0 > lo.0
        })
        .filter_map(|s| s.get_id().get_name().ok().map(str::to_string))
        // Codec tuning controls that are not volumes.
        .filter(|n| !["Capture", "ADC", "ALC", "Ramp", "Enhancement", "Boost", "Mic"].iter().any(|w| n.contains(w)))
        .collect()
}

#[derive(Serialize)]
pub struct OutputDevice {
    pub id: String,
    pub name: String,
    /// Hardware volume controls of the card (empty: software volume only).
    pub mixers: Vec<String>,
}

/// List playback devices. Uses the ALSA API and the /dev/snd file names, because Docker
/// hides /proc/asound inside containers.
pub fn list_devices() -> Vec<OutputDevice> {
    let mixers_of = |device: &str| -> Vec<String> {
        Mixer::new(&mixer_card(device), false).map(|m| playback_controls(&m)).unwrap_or_default()
    };
    let mut out =
        vec![OutputDevice { id: "default".into(), name: "System default".into(), mixers: mixers_of("default") }];
    // Playback PCMs: /dev/snd/pcmC<card>D<device>p
    let mut pcms: Vec<(i32, u32)> = std::fs::read_dir("/dev/snd")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| parse_pcm_name(&e.file_name().to_string_lossy()))
        .collect();
    pcms.sort_unstable();
    for &(card_no, dev) in &pcms {
        let Ok((card_id, card_name)) = alsa::card::Card::new(card_no).get_id_info() else { continue };
        let multi = pcms.iter().filter(|(c, _)| *c == card_no).count() > 1;
        let id = format!("plughw:CARD={card_id},DEV={dev}");
        let name = if multi { format!("{card_name} (device {dev})") } else { card_name };
        out.push(OutputDevice { mixers: mixers_of(&id), id, name });
    }
    out
}

/// "pcmC4D0p" → (4, 0); capture devices and other files → None.
fn parse_pcm_name(name: &str) -> Option<(i32, u32)> {
    let rest = name.strip_prefix("pcmC")?.strip_suffix('p')?;
    let (card, dev) = rest.split_once('D')?;
    Some((card.parse().ok()?, dev.parse().ok()?))
}

trait CardInfo {
    fn get_id_info(&self) -> Result<(String, String), alsa::Error>;
}

impl CardInfo for alsa::card::Card {
    /// (short id such as "rockchipes8316", readable name such as "rockchip-es8316")
    fn get_id_info(&self) -> Result<(String, String), alsa::Error> {
        let ctl = alsa::ctl::Ctl::from_card(self, false)?;
        let info = ctl.card_info()?;
        let id = info.get_id()?.to_string();
        let name = info.get_name().map(str::to_string).unwrap_or_else(|_| id.clone());
        Ok((id, name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_curve() {
        assert_eq!(volume_db(0), None);
        assert_eq!(volume_db(100), Some(0.0));
        assert_eq!(volume_db(50), Some(-30.0));
    }

    #[test]
    fn s32_conversion() {
        let mut v = Vec::new();
        to_s32(&[1, -1, i16::MAX], 1.0, &mut v);
        assert_eq!(v, vec![1 << 16, -(1 << 16), (i16::MAX as i32) << 16]);
        to_s32(&[16384], 0.5, &mut v);
        assert_eq!(v, vec![8192 << 16]);
    }

    #[test]
    fn pcm_file_names() {
        assert_eq!(parse_pcm_name("pcmC4D0p"), Some((4, 0)));
        assert_eq!(parse_pcm_name("pcmC3D0c"), None);
        assert_eq!(parse_pcm_name("controlC0"), None);
    }

    #[test]
    fn mixer_card_names() {
        assert_eq!(mixer_card("plughw:CARD=rockchipes8316,DEV=0"), "hw:CARD=rockchipes8316");
        assert_eq!(mixer_card("hw:1,0"), "hw:1");
        assert_eq!(mixer_card("default"), "default");
    }
}
