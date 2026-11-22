use crate::config::Config;
use serde::Serialize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, watch};

/// Commands for the Bluetooth task.
#[derive(Debug)]
pub enum BtCmd {
    OpenPairing,
    ClosePairing,
    /// Master volume changed (0..100) by UI / MQTT / DLNA; sync to phone.
    VolumeChanged(u8),
    Connect(String),
    Disconnect(String),
    Remove(String),
    /// play | pause | next | previous on the active phone.
    Media(String),
    /// Bluetooth name changed.
    Rename(String),
    /// Network playback took the output from this device.
    Preempted(String),
    /// Network playback ended; resume this device.
    Resume(String),
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct DeviceInfo {
    pub address: String,
    pub name: String,
    pub connected: bool,
    pub trusted: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Track {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub status: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Stream {
    pub codec: String,
    pub sample_rate: u32,
    pub channels: u32,
    pub bitpool: u8,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Status {
    pub bluez_ok: bool,
    pub adapter_ok: bool,
    pub adapter_address: String,
    pub devices: Vec<DeviceInfo>,
    /// Address of the phone that streams now.
    pub active_device: Option<String>,
    pub track: Track,
    pub stream: Option<Stream>,
    /// DLNA / Home Assistant playback.
    pub network: Option<NetworkPlayback>,
    /// Live state of each output while a stream plays.
    pub outputs: Vec<OutputStatus>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct OutputStatus {
    pub device: String,
    /// playing | idle | unavailable: … | error: …
    pub state: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct NetworkPlayback {
    pub uri: String,
    pub title: String,
    pub state: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Idle,
    Bluetooth,
    Network,
}

#[derive(Default, Serialize)]
pub struct Stats {
    pub packets: AtomicU64,
    pub decode_errors: AtomicU64,
    pub underruns: AtomicU64,
}

pub struct App {
    pub data_dir: PathBuf,
    /// Stable id of this installation (MQTT node id, DLNA UDN).
    pub uid: String,
    pub cfg: RwLock<Config>,
    pub status: Mutex<Status>,
    pub volume: AtomicU8,
    /// Unix seconds when the pairing window closes (0 = closed).
    pub pairing_until: AtomicU64,
    pub stats: Stats,
    pub bt_tx: mpsc::UnboundedSender<BtCmd>,
    pub changed: watch::Sender<u64>,
    pub output: crate::player::OutputArbiter,
    /// Bumped when the output list changes; running streams reopen their outputs.
    pub outputs_gen: AtomicU64,
    /// Unix seconds since the output went idle (0 = playing).
    pub idle_since: AtomicU64,
    /// Settings changed and not yet written (see `save_config`).
    config_dirty: AtomicBool,
    save_requested: tokio::sync::Notify,
}

/// Main volume 0..100 to the AVRCP absolute volume 0..127, and back. Rounded so that
/// a round trip returns the same value (no flapping between the phone and the speaker).
pub fn to_avrcp(v: u8) -> u16 {
    ((v.min(100) as u32 * 127 + 50) / 100) as u16
}

pub fn from_avrcp(v: u16) -> u8 {
    ((v.min(127) as u32 * 100 + 63) / 127) as u8
}

impl App {
    pub fn new(data_dir: PathBuf, cfg: Config) -> (Arc<App>, mpsc::UnboundedReceiver<BtCmd>) {
        let (bt_tx, bt_rx) = mpsc::unbounded_channel();
        let (changed, _) = watch::channel(0);
        let uid = load_uid(&data_dir);
        let app = Arc::new(App {
            data_dir,
            uid,
            volume: AtomicU8::new(cfg.volume),
            cfg: RwLock::new(cfg),
            status: Mutex::new(Status::default()),
            pairing_until: AtomicU64::new(0),
            stats: Stats::default(),
            bt_tx,
            changed,
            output: crate::player::OutputArbiter::default(),
            outputs_gen: AtomicU64::new(0),
            idle_since: AtomicU64::new(now()),
            config_dirty: AtomicBool::new(false),
            save_requested: tokio::sync::Notify::new(),
        });
        (app, bt_rx)
    }

    pub fn cfg(&self) -> Config {
        self.cfg.read().unwrap().clone()
    }

    pub fn notify(&self) {
        self.changed.send_modify(|v| *v = v.wrapping_add(1));
    }

    pub fn update_status(&self, f: impl FnOnce(&mut Status)) {
        f(&mut self.status.lock().unwrap());
        self.notify();
    }

    pub fn pairing_secs_left(&self) -> u64 {
        self.pairing_until.load(Ordering::Relaxed).saturating_sub(now())
    }

    pub fn pairing_open(&self) -> bool {
        self.pairing_secs_left() > 0
    }

    pub fn volume(&self) -> u8 {
        self.volume.load(Ordering::Relaxed)
    }

    /// Set master volume. `from_bt` avoids echoing a phone change back to the phone.
    pub fn set_volume(&self, v: u8, from_bt: bool) {
        let max = self.cfg.read().unwrap().max_volume;
        let v = v.min(max);
        if self.volume.swap(v, Ordering::Relaxed) == v {
            return;
        }
        if !from_bt {
            let _ = self.bt_tx.send(BtCmd::VolumeChanged(v));
        }
        {
            let mut cfg = self.cfg.write().unwrap();
            cfg.volume = v;
        }
        self.notify();
        self.save_config();
    }

    /// Current volume of one output (0..100); 100 if unknown.
    pub fn output_volume(&self, device: &str) -> u8 {
        let cfg = self.cfg.read().unwrap();
        cfg.outputs.iter().find(|o| o.device == device).map(|o| o.volume).unwrap_or(100)
    }

    /// Set one output's volume. Takes effect within ~50 ms; no reopen needed.
    pub fn set_output_volume(&self, device: &str, volume: u8) -> bool {
        let found = {
            let mut cfg = self.cfg.write().unwrap();
            match cfg.outputs.iter_mut().find(|o| o.device == device) {
                Some(o) => {
                    o.volume = volume.min(100);
                    true
                }
                None => false,
            }
        };
        if found {
            self.notify();
        }
        found
    }

    /// Put every output back to its default volume.
    pub fn restore_default_volumes(&self) {
        {
            let mut cfg = self.cfg.write().unwrap();
            for o in cfg.outputs.iter_mut() {
                o.volume = o.default_volume;
            }
        }
        self.notify();
    }

    /// Call when a source starts playing. After a long silence, output volumes
    /// go back to their defaults (if enabled), so a loud evening does not carry over.
    pub fn playback_starting(&self) {
        let idle_since = self.idle_since.swap(0, Ordering::Relaxed);
        if idle_since != 0 && now().saturating_sub(idle_since) >= 300 && self.cfg().restore_default_volumes {
            log::info!("playback after a long pause: restoring default output volumes");
            self.restore_default_volumes();
            self.save_config();
        }
    }

    /// Call when the output becomes idle.
    pub fn playback_stopped(&self) {
        if self.output.current() == Source::Idle {
            self.idle_since.store(now(), Ordering::Relaxed);
        }
    }

    /// Replace the output list; playing streams switch within a moment.
    pub fn set_outputs(&self, outputs: Vec<crate::config::OutputCfg>) {
        // Volume-only changes apply live; anything else reopens the outputs.
        let strip = |v: &[crate::config::OutputCfg]| -> Vec<crate::config::OutputCfg> {
            v.iter()
                .cloned()
                .map(|mut o| {
                    o.volume = 0;
                    o.default_volume = 0;
                    o.name.clear();
                    o
                })
                .collect()
        };
        let reopen = {
            let mut cfg = self.cfg.write().unwrap();
            if cfg.outputs == outputs {
                return;
            }
            let reopen = strip(&cfg.outputs) != strip(&outputs);
            cfg.outputs = outputs;
            cfg.sanitize();
            reopen
        };
        if reopen {
            self.outputs_gen.fetch_add(1, Ordering::Relaxed);
            self.update_status(|s| s.outputs.clear());
        } else {
            self.notify();
        }
    }

    /// Turn one output on or off.
    pub fn enable_output(&self, device: &str, on: bool) {
        let mut outs = self.cfg().outputs;
        if let Some(o) = outs.iter_mut().find(|o| o.device == device) {
            o.enabled = on;
            self.set_outputs(outs);
            self.save_config();
        }
    }

    /// Ask for the settings to be saved. The saver task writes them a moment later, so a
    /// dragged slider causes one write instead of dozens (SD cards wear out).
    pub fn save_config(&self) {
        self.config_dirty.store(true, Ordering::Relaxed);
        self.save_requested.notify_one();
    }

    /// Write pending settings now (blocking file I/O).
    pub fn flush_config(&self) {
        if self.config_dirty.swap(false, Ordering::Relaxed) {
            let cfg = self.cfg();
            cfg.save(&self.data_dir);
        }
    }

    /// Background task: writes settings at most every 2 seconds.
    pub async fn run_saver(self: Arc<Self>) {
        loop {
            self.save_requested.notified().await;
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let app = self.clone();
            let _ = tokio::task::spawn_blocking(move || app.flush_config()).await;
        }
    }

    /// Full state for the web UI and MQTT.
    pub fn snapshot(&self) -> serde_json::Value {
        let status = self.status.lock().unwrap().clone();
        let cfg = self.cfg();
        serde_json::json!({
            "name": cfg.name,
            "volume": self.volume(),
            "source": self.output.current(),
            "pairing_secs_left": self.pairing_secs_left(),
            "outputs": cfg.outputs.iter().map(|o| serde_json::json!({
                "device": o.device, "name": o.name, "enabled": o.enabled, "volume": o.volume,
            })).collect::<Vec<_>>(),
            "status": status,
            "stats": {
                "packets": self.stats.packets.load(Ordering::Relaxed),
                "decode_errors": self.stats.decode_errors.load(Ordering::Relaxed),
                "underruns": self.stats.underruns.load(Ordering::Relaxed),
            },
        })
    }
}

/// Read or create a random UUID in the data directory.
fn load_uid(data_dir: &std::path::Path) -> String {
    let path = data_dir.join("uid");
    if let Ok(s) = std::fs::read_to_string(&path) {
        let s = s.trim().to_string();
        if s.len() == 36 {
            return s;
        }
    }
    let uid = std::fs::read_to_string("/proc/sys/kernel/random/uuid")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| format!("00000000-0000-4000-8000-{:012x}", now()));
    if let Err(e) = std::fs::write(&path, &uid) {
        log::warn!("cannot save {}: {e}; the device id changes on every start", path.display());
    }
    uid
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avrcp_volume_round_trip() {
        for v in 0..=100u8 {
            assert_eq!(from_avrcp(to_avrcp(v)), v, "volume {v}");
        }
        assert_eq!(to_avrcp(100), 127);
        assert_eq!(from_avrcp(127), 100);
    }
}
