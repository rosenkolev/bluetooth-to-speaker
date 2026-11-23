use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Bluetooth name shown on phones.
    pub name: String,
    /// Bluetooth adapter (hci0, hci1, ...).
    pub adapter: String,
    /// Sound outputs. All enabled outputs play at the same time.
    pub outputs: Vec<OutputCfg>,
    /// Output buffer (jitter buffer) in milliseconds.
    pub latency_ms: u32,
    /// Master volume 0..100.
    pub volume: u8,
    /// Highest volume a source may set (0..100).
    pub max_volume: u8,
    /// Seconds the speaker stays visible and pairable after "Pair".
    pub pairing_window_secs: u64,
    /// SBC max bitpool offered to phones. 35 = Wi-Fi friendly, 53 = max quality.
    pub max_bitpool: u8,
    /// Keep two phones connected; the one that starts playing last wins.
    /// Off: a newly connected phone disconnects the previous one.
    pub multipoint: bool,
    /// Reconnect the last phone at start-up.
    pub reconnect_on_start: bool,
    /// Resume the phone after Home Assistant / DLNA playback ends.
    pub resume_bluetooth: bool,
    /// Reset output volumes to their defaults when playback starts after 5+ minutes of silence.
    pub restore_default_volumes: bool,
    /// Web UI / API port.
    pub http_port: u16,
    pub mqtt: MqttConfig,
    pub dlna: DlnaConfig,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct OutputCfg {
    /// ALSA device, e.g. "plughw:CARD=rockchipes8316,DEV=0".
    pub device: String,
    /// Display name.
    pub name: String,
    pub enabled: bool,
    /// Extra delay to line up with slower outputs (TVs add 50–150 ms).
    pub delay_ms: u32,
    /// Current output volume 0..100, on top of the master volume.
    pub volume: u8,
    /// Volume set at start-up and, if enabled, when playback starts after a long pause.
    pub default_volume: u8,
    /// How volume is applied: "" = software (works everywhere), "auto" = the card's
    /// hardware mixer if one is found, or the name of an ALSA mixer control.
    pub mixer: String,
}

impl Default for OutputCfg {
    fn default() -> Self {
        OutputCfg {
            device: "default".into(),
            name: "Default".into(),
            enabled: true,
            delay_ms: 0,
            volume: 100,
            default_volume: 100,
            mixer: String::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct MqttConfig {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub discovery_prefix: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct DlnaConfig {
    /// DLNA media renderer, served on the web UI port; discovery via SSDP needs host networking.
    pub enabled: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            name: "Bluetooth Speaker".into(),
            adapter: "hci0".into(),
            outputs: vec![OutputCfg::default()],
            latency_ms: 200,
            volume: 50,
            max_volume: 100,
            pairing_window_secs: 120,
            max_bitpool: 35,
            multipoint: true,
            reconnect_on_start: true,
            resume_bluetooth: true,
            restore_default_volumes: true,
            http_port: 8780,
            mqtt: MqttConfig::default(),
            dlna: DlnaConfig::default(),
        }
    }
}

impl Default for MqttConfig {
    fn default() -> Self {
        MqttConfig {
            enabled: false,
            host: "localhost".into(),
            port: 1883,
            username: String::new(),
            password: String::new(),
            discovery_prefix: "homeassistant".into(),
        }
    }
}

impl Default for DlnaConfig {
    fn default() -> Self {
        DlnaConfig { enabled: true }
    }
}

/// Set when the settings file exists but could not be read: saving is off, so the
/// defaults in use never replace it. Holds the error.
pub static UNREADABLE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

impl Config {
    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join("config.json")
    }

    /// HTTP port before the settings are loaded (HTTP_PORT, else the settings file).
    /// Reads only; any error gives the default.
    pub fn peek_http_port(data_dir: &Path) -> u16 {
        if let Some(p) = std::env::var("HTTP_PORT").ok().and_then(|v| v.parse().ok()) {
            return p;
        }
        std::fs::read_to_string(Self::path(data_dir))
            .ok()
            .and_then(|s| serde_json::from_str::<Config>(&s).ok())
            .map(|c| c.http_port)
            .unwrap_or_else(|| Config::default().http_port)
    }

    pub fn load(data_dir: &Path) -> Config {
        let path = Self::path(data_dir);
        let first_start = !path.exists();
        let mut cfg: Config = match std::fs::read_to_string(&path) {
            Ok(s) => match serde_json::from_str(&s) {
                Ok(c) => c,
                Err(e) => {
                    // Keep the broken file for the user instead of overwriting it with defaults.
                    // A new name each time: an older broken file is never overwritten.
                    let bad = data_dir.join(format!("config.json.bad-{}", crate::app::now()));
                    log::error!("settings file is invalid ({e}); kept as {} and using defaults", bad.display());
                    let _ = std::fs::rename(&path, bad);
                    Config::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(e) => {
                // Exists but cannot be read (owned by another user): never overwrite it.
                log::error!("cannot read {}: {e}; using defaults and not saving", path.display());
                *UNREADABLE.lock().unwrap() = Some(e.to_string());
                Config::default()
            }
        };
        // Starting values from the environment, handy for compose files. Only on the first
        // start: afterwards the web UI owns these, and a restart must not undo its changes.
        if first_start {
            if let Ok(v) = std::env::var("BT_NAME") {
                cfg.name = v;
            }
            // ALSA devices separated by ';' (device names contain commas), all enabled.
            if let Ok(v) = std::env::var("BT_OUTPUT") {
                cfg.outputs = v
                    .split(';')
                    .map(str::trim)
                    .filter(|d| !d.is_empty())
                    .map(|d| OutputCfg { device: d.into(), name: d.into(), ..Default::default() })
                    .collect();
            }
        }
        // Deployment settings (not in the web UI): the environment always wins.
        if let Ok(v) = std::env::var("BT_ADAPTER") {
            cfg.adapter = v;
        }
        if let Ok(v) = std::env::var("HTTP_PORT") {
            if let Ok(p) = v.parse() {
                cfg.http_port = p;
            }
        }
        if let Ok(v) = std::env::var("MQTT_HOST") {
            cfg.mqtt.enabled = true;
            cfg.mqtt.host = v;
        }
        if let Ok(v) = std::env::var("MQTT_PORT") {
            if let Ok(p) = v.parse() {
                cfg.mqtt.port = p;
            }
        }
        if let Ok(v) = std::env::var("MQTT_USERNAME") {
            cfg.mqtt.username = v;
        }
        if let Ok(v) = std::env::var("MQTT_PASSWORD") {
            cfg.mqtt.password = v;
        }
        cfg.sanitize();
        cfg
    }

    pub fn sanitize(&mut self) {
        self.volume = self.volume.min(100);
        self.max_volume = self.max_volume.clamp(1, 100);
        self.max_bitpool = self.max_bitpool.clamp(2, 53);
        self.latency_ms = self.latency_ms.clamp(40, 1000);
        self.pairing_window_secs = self.pairing_window_secs.clamp(10, 600);
        if self.name.trim().is_empty() {
            self.name = "Bluetooth Speaker".into();
        }
        self.outputs.retain(|o| {
            let ok = safe_alsa_device(o.device.trim());
            if !ok {
                log::warn!("output \"{}\" ignored: not a plain ALSA device name", o.device);
            }
            ok
        });
        let mut seen = std::collections::HashSet::new();
        self.outputs.retain(|o| seen.insert(o.device.clone()));
        self.outputs.truncate(8);
        for o in &mut self.outputs {
            o.volume = o.volume.min(100);
            o.default_volume = o.default_volume.min(100);
            o.device = o.device.trim().to_string();
            o.mixer = o.mixer.trim().to_string();
            if !o.mixer.is_empty() && !safe_alsa_mixer(&o.mixer) {
                log::warn!("mixer \"{}\" ignored: not a plain control name", o.mixer);
                o.mixer.clear();
            }
            o.delay_ms = o.delay_ms.min(2000);
            if o.name.trim().is_empty() {
                o.name = o.device.clone();
            }
        }
        if self.outputs.is_empty() {
            self.outputs.push(OutputCfg::default());
        }
    }

    /// Atomic and durable: write a temporary file, flush it to disk, then rename it,
    /// so a power cut leaves either the old or the new settings, never an empty file.
    pub fn save(&self, data_dir: &Path) {
        use std::io::Write;
        if UNREADABLE.lock().unwrap().is_some() {
            return; // the user's settings are there; defaults must not replace them
        }
        let tmp = data_dir.join("config.json.tmp");
        let Ok(s) = serde_json::to_string_pretty(self) else { return };
        let res = (|| -> std::io::Result<()> {
            // Owner-only: the file holds the MQTT password.
            let mut f = {
                use std::os::unix::fs::OpenOptionsExt;
                std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?
            };
            f.write_all(s.as_bytes())?;
            f.sync_all()?;
            std::fs::rename(&tmp, Self::path(data_dir))?;
            if let Ok(dir) = std::fs::File::open(data_dir) {
                let _ = dir.sync_all();
            }
            Ok(())
        })();
        if let Err(e) = res {
            log::warn!("cannot save settings in {}: {e}", data_dir.display());
        }
    }
}

/// ALSA PCM plugins that may be named. ALSA also accepts inline configuration in a
/// device name, including the `file` plugin with `|command`, which runs a program;
/// only plain `plugin:arguments` names from this list are accepted.
const ALSA_PLUGINS: &[&str] =
    &["default", "sysdefault", "hw", "plughw", "plug", "dmix", "front", "iec958", "hdmi", "null", "pulse", "pipewire"];

/// A plain ALSA device name such as `plughw:CARD=rockchipes8316,DEV=0`.
pub fn safe_alsa_device(s: &str) -> bool {
    let (plugin, args) = s.split_once(':').unwrap_or((s, ""));
    s.len() <= 100
        && ALSA_PLUGINS.contains(&plugin)
        && args.chars().all(|c| c.is_ascii_alphanumeric() || "_-.:=,".contains(c))
}

/// A plain mixer control name such as `DAC` or `Headphone Mixer`.
pub fn safe_alsa_mixer(s: &str) -> bool {
    s.len() <= 64 && s.chars().all(|c| c.is_ascii_alphanumeric() || " _-.".contains(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alsa_names() {
        assert!(safe_alsa_device("default"));
        assert!(safe_alsa_device("plughw:CARD=rockchipes8316,DEV=0"));
        assert!(safe_alsa_device("plug:dmix:CARD=x"));
        assert!(!safe_alsa_device("file:'|sh -c id'"));
        assert!(!safe_alsa_device("file:FILE=/data/x"));
        assert!(!safe_alsa_device("plughw:{type hooks}"));
        assert!(!safe_alsa_device("plughw:CARD=x DEV=0"));
        assert!(safe_alsa_mixer("Headphone Mixer"));
        assert!(!safe_alsa_mixer("DAC'"));
    }

    #[test]
    fn sanitize_drops_unsafe_outputs() {
        let mut c = Config {
            outputs: vec![
                OutputCfg { device: "file:'|touch /tmp/x'".into(), ..Default::default() },
                OutputCfg { device: "hw:1,0".into(), mixer: "x;y".into(), ..Default::default() },
            ],
            ..Default::default()
        };
        c.sanitize();
        assert_eq!(c.outputs.len(), 1);
        assert_eq!(c.outputs[0].device, "hw:1,0");
        assert_eq!(c.outputs[0].mixer, "");
    }
}
