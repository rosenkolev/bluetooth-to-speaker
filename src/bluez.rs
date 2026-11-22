//! BlueZ over D-Bus: A2DP sink endpoint, pairing agent, pairing window, AVRCP.
//!
//! Uses only a unique bus name (no well-known name), so no host D-Bus policy file is needed.

use crate::app::{App, BtCmd, DeviceInfo, Source};
use crate::player::{run_bt_stream, Lease};
use futures_util::StreamExt;
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use zbus::message::Type as MsgType;
use zbus::{Connection, MatchRule, Message, MessageStream};
use zbus::{ObjectPath, OwnedObjectPath, OwnedVariant, Variant};

const BLUEZ: &str = "org.bluez";
const A2DP_SINK_UUID: &str = "0000110b-0000-1000-8000-00805f9b34fb";
const ENDPOINT_PATH: &str = "/io/bluetooth_to_speaker/sbc_sink";
const AGENT_PATH: &str = "/io/bluetooth_to_speaker/agent";
const IF_ADAPTER: &str = "org.bluez.Adapter1";
const IF_DEVICE: &str = "org.bluez.Device1";
const IF_TRANSPORT: &str = "org.bluez.MediaTransport1";
const IF_PLAYER: &str = "org.bluez.MediaPlayer1";

type ManagedObjects = HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedVariant>>>;

// ---------------------------------------------------------------- endpoint

enum EpEvent {
    SetConfiguration { transport: String, device: String },
    ClearConfiguration { transport: String },
    Release,
}

struct Endpoint {
    tx: mpsc::UnboundedSender<EpEvent>,
    max_bitpool: u8,
}

#[zbus::interface(name = "org.bluez.MediaEndpoint1")]
impl Endpoint {
    async fn set_configuration(&self, transport: OwnedObjectPath, properties: HashMap<String, OwnedVariant>) {
        let device = properties.get("Device").and_then(v_str).unwrap_or_default();
        let _ = self.tx.send(EpEvent::SetConfiguration { transport: transport.to_string(), device });
    }

    async fn select_configuration(&self, capabilities: Vec<u8>) -> zbus::fdo::Result<Vec<u8>> {
        select_sbc(&capabilities, self.max_bitpool)
            .ok_or_else(|| zbus::fdo::Error::InvalidArgs("unsupported SBC capabilities".into()))
    }

    async fn clear_configuration(&self, transport: OwnedObjectPath) {
        let _ = self.tx.send(EpEvent::ClearConfiguration { transport: transport.to_string() });
    }

    async fn release(&self) {
        let _ = self.tx.send(EpEvent::Release);
    }
}

/// SBC capabilities we accept as a sink. The bitpool cap limits radio airtime.
fn sbc_capabilities(max_bitpool: u8) -> Vec<u8> {
    // byte0: 16/32/44.1/48 kHz | mono/dual/stereo/joint
    // byte1: blocks 4/8/12/16 | subbands 4/8 | SNR/loudness
    vec![0xff, 0xff, 2, max_bitpool]
}

fn select_sbc(caps: &[u8], max_bitpool: u8) -> Option<Vec<u8>> {
    if caps.len() < 4 {
        return None;
    }
    let pick = |bits: u8, prefs: &[u8]| prefs.iter().copied().find(|p| bits & p != 0);
    let freq = pick(caps[0] & 0xf0, &[0x20, 0x10, 0x40, 0x80])?; // 44.1k, 48k, 32k, 16k
    let mode = pick(caps[0] & 0x0f, &[0x01, 0x02, 0x04, 0x08])?; // joint, stereo, dual, mono
    let blocks = pick(caps[1] & 0xf0, &[0x10, 0x20, 0x40, 0x80])?; // 16, 12, 8, 4
    let subbands = pick(caps[1] & 0x0c, &[0x04, 0x08])?; // 8, 4
    let alloc = pick(caps[1] & 0x03, &[0x01, 0x02])?; // loudness, SNR
    let min_bp = caps[2].max(2);
    let max_bp = caps[3].min(max_bitpool).max(min_bp);
    Some(vec![freq | mode, blocks | subbands | alloc, min_bp, max_bp])
}

// ---------------------------------------------------------------- agent

#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.bluez.Error")]
enum AgentError {
    #[zbus(error)]
    ZBus(zbus::Error),
    Rejected(String),
}

struct Agent {
    app: Arc<App>,
}

impl Agent {
    fn allow_pairing(&self, device: &ObjectPath<'_>) -> Result<(), AgentError> {
        if self.app.pairing_open() {
            log::info!("pairing accepted: {}", addr_of(device.as_str()));
            Ok(())
        } else {
            log::info!("pairing rejected (window closed): {}", addr_of(device.as_str()));
            Err(AgentError::Rejected("pairing window closed".into()))
        }
    }
}

#[zbus::interface(name = "org.bluez.Agent1")]
impl Agent {
    async fn release(&self) {}

    async fn request_pin_code(&self, device: ObjectPath<'_>) -> Result<String, AgentError> {
        self.allow_pairing(&device)?;
        Ok("0000".into())
    }

    async fn display_pin_code(&self, device: ObjectPath<'_>, _pincode: String) -> Result<(), AgentError> {
        self.allow_pairing(&device)
    }

    async fn request_passkey(&self, device: ObjectPath<'_>) -> Result<u32, AgentError> {
        self.allow_pairing(&device)?;
        Ok(0)
    }

    async fn display_passkey(&self, _device: ObjectPath<'_>, _passkey: u32, _entered: u16) {}

    async fn request_confirmation(&self, device: ObjectPath<'_>, _passkey: u32) -> Result<(), AgentError> {
        self.allow_pairing(&device)
    }

    async fn request_authorization(&self, device: ObjectPath<'_>) -> Result<(), AgentError> {
        self.allow_pairing(&device)
    }

    async fn authorize_service(
        &self,
        device: ObjectPath<'_>,
        _uuid: String,
        #[zbus(connection)] conn: &Connection,
    ) -> Result<(), AgentError> {
        let paired =
            get_prop(conn, device.as_str(), IF_DEVICE, "Paired").await.ok().and_then(|v| v_bool(&v)).unwrap_or(false);
        if paired || self.app.pairing_open() {
            Ok(())
        } else {
            Err(AgentError::Rejected("unknown device".into()))
        }
    }

    async fn cancel(&self) {}
}

// ---------------------------------------------------------------- task

pub async fn run(app: Arc<App>, mut rx: mpsc::UnboundedReceiver<BtCmd>) {
    let mut first = true;
    loop {
        match session(&app, &mut rx, first).await {
            Ok(()) => log::warn!("bluez went away, reconnecting"),
            Err(e) => log::warn!("bluez session error: {e}"),
        }
        first = false;
        app.update_status(|s| {
            s.bluez_ok = false;
            s.adapter_ok = false;
        });
        app.output.stop_source(Source::Bluetooth);
        // Drain commands that make no sense without BlueZ.
        while rx.try_recv().is_ok() {}
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

struct Transport {
    device: String,
    stop: Option<Arc<AtomicBool>>,
}

/// Max phones connected at once in multipoint mode.
const MAX_CONNECTED: usize = 2;
/// After a switch, the old device may not take the speaker back for this long.
const SWITCH_GUARD: Duration = Duration::from_secs(5);

struct Session<'a> {
    app: &'a Arc<App>,
    conn: Connection,
    adapter: String,
    transports: HashMap<String, Transport>,
    pairing_was_open: bool,
    /// Connected device addresses, oldest first.
    connected: Vec<String>,
    switched_away: HashMap<String, std::time::Instant>,
    /// Addresses of paired devices.
    known: std::collections::HashSet<String>,
}

async fn session(app: &Arc<App>, rx: &mut mpsc::UnboundedReceiver<BtCmd>, first: bool) -> zbus::Result<()> {
    let cfg = app.cfg();
    let conn = Connection::system().await?;
    let adapter = format!("/org/bluez/{}", cfg.adapter);

    let rule = MatchRule::builder().msg_type(MsgType::Signal).sender(BLUEZ).build()?;
    let mut bluez_signals = MessageStream::for_match_rule(rule, &conn, NonZeroUsize::new(256)).await?;
    let rule = MatchRule::builder()
        .msg_type(MsgType::Signal)
        .sender("org.freedesktop.DBus")
        .member("NameOwnerChanged")
        .add_arg(BLUEZ)
        .build()?;
    let mut owner_signals = MessageStream::for_match_rule(rule, &conn, NonZeroUsize::new(16)).await?;

    let objects = managed_objects(&conn).await?;
    if !objects.iter().any(|(p, i)| p.as_str() == adapter && i.contains_key(IF_ADAPTER)) {
        app.update_status(|s| {
            s.bluez_ok = true;
            s.adapter_ok = false;
            s.warnings = vec![format!("Bluetooth adapter {} not found", cfg.adapter)];
        });
        return Err(zbus::Error::Failure(format!("adapter {adapter} not found")));
    }

    // Adapter: powered, hidden, named.
    let powered = get_prop(&conn, &adapter, IF_ADAPTER, "Powered").await.ok().and_then(|v| v_bool(&v));
    if powered == Some(false) {
        set_prop(&conn, &adapter, IF_ADAPTER, "Powered", Variant::Bool(true)).await?;
    }
    set_prop(&conn, &adapter, IF_ADAPTER, "Alias", Variant::from(cfg.name.as_str())).await?;
    set_prop(&conn, &adapter, IF_ADAPTER, "Discoverable", Variant::Bool(false)).await?;
    set_prop(&conn, &adapter, IF_ADAPTER, "Pairable", Variant::Bool(false)).await?;
    let address =
        get_prop(&conn, &adapter, IF_ADAPTER, "Address").await.ok().and_then(|v| v_str(&v)).unwrap_or_default();

    // A2DP sink endpoint.
    let (ep_tx, mut ep_rx) = mpsc::unbounded_channel();
    conn.object_server().at(ENDPOINT_PATH, Endpoint { tx: ep_tx, max_bitpool: cfg.max_bitpool }).await?;
    let mut props: HashMap<&str, Variant> = HashMap::new();
    props.insert("UUID", Variant::from(A2DP_SINK_UUID));
    props.insert("Codec", Variant::U8(0)); // SBC
    props.insert("Capabilities", Variant::from(sbc_capabilities(cfg.max_bitpool)));
    call(&conn, &adapter, "org.bluez.Media1", "RegisterEndpoint", &(ObjectPath::try_from(ENDPOINT_PATH)?, props))
        .await?;

    // Pairing agent.
    conn.object_server().at(AGENT_PATH, Agent { app: app.clone() }).await?;
    let agent_path = ObjectPath::try_from(AGENT_PATH)?;
    call(&conn, "/org/bluez", "org.bluez.AgentManager1", "RegisterAgent", &(agent_path.clone(), "NoInputNoOutput"))
        .await?;
    if let Err(e) = call(&conn, "/org/bluez", "org.bluez.AgentManager1", "RequestDefaultAgent", &(agent_path,)).await {
        log::warn!("could not become default agent: {e}");
    }

    log::info!("bluez ready: adapter {adapter} ({address}), name \"{}\"", cfg.name);
    app.update_status(|s| {
        s.bluez_ok = true;
        s.adapter_ok = true;
        s.adapter_address = address;
        s.warnings.clear();
    });

    let mut sess = Session {
        app,
        conn: conn.clone(),
        adapter,
        transports: HashMap::new(),
        pairing_was_open: false,
        connected: Vec::new(),
        switched_away: HashMap::new(),
        known: Default::default(),
    };
    sess.refresh_devices().await;
    sess.connected =
        app.status.lock().unwrap().devices.iter().filter(|d| d.connected).map(|d| d.address.clone()).collect();

    if first && cfg.reconnect_on_start {
        if let Ok(addr) = std::fs::read_to_string(app.data_dir.join("last_device")) {
            let path = sess.device_path(addr.trim());
            let conn = conn.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(2)).await;
                if let Err(e) = call(&conn, &path, IF_DEVICE, "Connect", &()).await {
                    log::info!("reconnect to last phone failed: {e}");
                }
            });
        }
    }

    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            msg = bluez_signals.next() => match msg {
                Some(Ok(msg)) => sess.on_signal(&msg).await,
                Some(Err(_)) => {}
                // The D-Bus connection is gone: start a new session.
                None => return Err(zbus::Error::Failure("D-Bus connection closed".into())),
            },
            msg = owner_signals.next() => match msg {
                Some(Ok(msg)) => {
                    if let Ok((_, _, new)) = msg.body().deserialize::<(String, String, String)>() {
                        if new.is_empty() { return Ok(()); }
                    }
                }
                Some(Err(_)) => {}
                None => return Err(zbus::Error::Failure("D-Bus connection closed".into())),
            },
            Some(ev) = ep_rx.recv() => sess.on_endpoint(ev).await,
            Some(cmd) = rx.recv() => sess.on_cmd(cmd).await,
            _ = tick.tick() => sess.on_tick().await,
        }
    }
}

impl Session<'_> {
    fn device_path(&self, addr: &str) -> String {
        format!("{}/dev_{}", self.adapter, addr.replace(':', "_"))
    }

    async fn refresh_devices(&mut self) {
        let Ok(objects) = managed_objects(&self.conn).await else { return };
        let prefix = format!("{}/", self.adapter);
        let mut devices: Vec<DeviceInfo> = objects
            .iter()
            .filter(|(p, _)| p.as_str().starts_with(&prefix))
            .filter_map(|(_, ifs)| ifs.get(IF_DEVICE))
            .filter(|d| d.get("Paired").and_then(v_bool).unwrap_or(false))
            .map(|d| DeviceInfo {
                address: d.get("Address").and_then(v_str).unwrap_or_default(),
                name: d.get("Alias").and_then(v_str).unwrap_or_default(),
                connected: d.get("Connected").and_then(v_bool).unwrap_or(false),
                trusted: d.get("Trusted").and_then(v_bool).unwrap_or(false),
            })
            .collect();
        devices.sort_by(|a, b| b.connected.cmp(&a.connected).then(a.name.cmp(&b.name)));
        self.known = devices.iter().map(|d| d.address.clone()).collect();
        self.app.update_status(|s| s.devices = devices);
    }

    /// Signals about a device that is not ours (BLE sensors seen by Home Assistant's scanner
    /// change RSSI many times a second) are skipped before their body is decoded.
    fn is_ours(&self, path: &str) -> bool {
        if !path.contains("/dev_") {
            return true; // adapter, BlueZ root
        }
        let addr = addr_of(path);
        self.app.pairing_open() || self.known.contains(&addr) || self.transports.values().any(|t| t.device == path)
    }

    async fn on_signal(&mut self, msg: &Message) {
        let hdr = msg.header();
        let (Some(member), Some(path)) = (hdr.member(), hdr.path()) else { return };
        let path = path.as_str().to_string();
        match member.as_str() {
            "PropertiesChanged" if !self.is_ours(&path) => {}
            "PropertiesChanged" => {
                let Ok((iface, changed, _)) =
                    msg.body().deserialize::<(String, HashMap<String, OwnedVariant>, Vec<String>)>()
                else {
                    return;
                };
                match iface.as_str() {
                    IF_TRANSPORT => {
                        if let Some(state) = changed.get("State").and_then(v_str) {
                            self.on_transport_state(&path, &state).await;
                        }
                        if let Some(v) = changed.get("Volume").and_then(v_u16) {
                            self.app.set_volume(crate::app::from_avrcp(v), true);
                        }
                    }
                    IF_DEVICE => {
                        if changed.get("Paired").and_then(v_bool) == Some(true) {
                            log::info!("paired: {}", addr_of(&path));
                            let _ = set_prop(&self.conn, &path, IF_DEVICE, "Trusted", Variant::Bool(true)).await;
                            self.close_pairing().await; // one new phone per window
                        }
                        match changed.get("Connected").and_then(v_bool) {
                            Some(true) => self.on_device_connected(addr_of(&path)).await,
                            Some(false) => {
                                let addr = addr_of(&path);
                                self.connected.retain(|a| a != &addr);
                                let was_active =
                                    self.app.status.lock().unwrap().active_device.as_deref() == Some(addr.as_str());
                                if was_active && self.app.output.current() != Source::Bluetooth {
                                    self.app.update_status(|s| {
                                        s.active_device = None;
                                        s.track = Default::default();
                                    });
                                }
                            }
                            None => {}
                        }
                        if ["Connected", "Paired", "Trusted", "Alias"].iter().any(|k| changed.contains_key(*k)) {
                            self.refresh_devices().await;
                        }
                    }
                    IF_PLAYER => {
                        let mut track = self.app.status.lock().unwrap().track.clone();
                        if let Some(st) = changed.get("Status").and_then(v_str) {
                            track.status = st;
                        }
                        if let Some(t) = changed.get("Track") {
                            let t = v_dict(t);
                            track.title = t.get("Title").cloned().unwrap_or_default();
                            track.artist = t.get("Artist").cloned().unwrap_or_default();
                            track.album = t.get("Album").cloned().unwrap_or_default();
                        }
                        self.app.update_status(|s| s.track = track);
                    }
                    IF_ADAPTER => {
                        if changed.get("Powered").and_then(v_bool) == Some(false) {
                            self.app.update_status(|s| s.warnings = vec!["Bluetooth adapter powered off".into()]);
                        } else if changed.get("Powered").and_then(v_bool) == Some(true) {
                            self.app.update_status(|s| s.warnings.clear());
                        }
                    }
                    _ => {}
                }
            }
            // Only paired devices matter; BLE devices come and go all the time.
            "InterfacesAdded" => {
                type Added = (OwnedObjectPath, HashMap<String, HashMap<String, OwnedVariant>>);
                let Ok((obj, ifaces)) = msg.body().deserialize::<Added>() else { return };
                let paired = ifaces.get(IF_DEVICE).and_then(|d| d.get("Paired")).and_then(v_bool).unwrap_or(false);
                if paired || self.known.contains(&addr_of(obj.as_str())) {
                    self.refresh_devices().await;
                }
            }
            "InterfacesRemoved" => {
                let Ok((obj, _)) = msg.body().deserialize::<(OwnedObjectPath, Vec<String>)>() else { return };
                if self.known.contains(&addr_of(obj.as_str())) {
                    self.refresh_devices().await;
                }
            }
            _ => {}
        }
    }

    async fn on_endpoint(&mut self, ev: EpEvent) {
        match ev {
            EpEvent::SetConfiguration { transport, device } => {
                log::info!("transport configured: {transport}");
                self.transports.insert(transport.clone(), Transport { device, stop: None });
                // The state may already be pending.
                if let Ok(v) = get_prop(&self.conn, &transport, IF_TRANSPORT, "State").await {
                    if let Some(state) = v_str(&v) {
                        self.on_transport_state(&transport, &state).await;
                    }
                }
                // Tell the phone our volume.
                let vol = crate::app::to_avrcp(self.app.volume());
                let _ = set_prop(&self.conn, &transport, IF_TRANSPORT, "Volume", Variant::U16(vol)).await;
            }
            EpEvent::ClearConfiguration { transport } => {
                if let Some(t) = self.transports.remove(&transport) {
                    if let Some(stop) = t.stop {
                        stop.store(true, Ordering::Relaxed);
                    }
                }
            }
            EpEvent::Release => {
                for t in self.transports.values() {
                    if let Some(stop) = &t.stop {
                        stop.store(true, Ordering::Relaxed);
                    }
                }
                self.transports.clear();
            }
        }
    }

    async fn on_transport_state(&mut self, path: &str, state: &str) {
        let Some(t) = self.transports.get(path) else { return };
        let running = t.stop.as_ref().map(|s| !s.load(Ordering::Relaxed)).unwrap_or(false);
        let addr = addr_of(&t.device);
        match state {
            "pending" | "active" if !running => {
                // A device we just switched away from may not take the speaker straight back
                // (phones that ignore AVRCP pause would otherwise ping-pong).
                if let Some(t0) = self.switched_away.get(&addr) {
                    if t0.elapsed() < SWITCH_GUARD {
                        log::info!("{addr} wants to stream again right after a switch; ignored");
                        return;
                    }
                }
                let method = if state == "pending" { "TryAcquire" } else { "Acquire" };
                let reply = call(&self.conn, path, IF_TRANSPORT, method, &()).await;
                let reply = match reply {
                    Ok(r) => r,
                    Err(e) => {
                        log::warn!("{method} failed on {path}: {e}");
                        return;
                    }
                };
                let Ok((fd, read_mtu, _write_mtu)) = reply.body().deserialize::<(zbus::OwnedFd, u16, u16)>() else {
                    log::warn!("bad Acquire reply");
                    return;
                };
                let (lease, prev) = self.app.output.claim(Source::Bluetooth, Some(addr.clone()));
                if let Some(t) = self.transports.get_mut(path) {
                    t.stop = Some(lease.stop.clone());
                }
                if let Some(p) = prev {
                    match (p.source, p.device) {
                        (Source::Bluetooth, Some(old)) if old != addr => {
                            log::info!("switching from {old} to {addr}");
                            self.switched_away.insert(old.clone(), std::time::Instant::now());
                            self.switched_away.remove(&addr);
                            self.preempt(&old).await;
                        }
                        (src, _) => log::info!("{addr} takes output from {src:?}"),
                    }
                }
                let _ = std::fs::write(self.app.data_dir.join("last_device"), &addr);
                self.app.update_status(|s| s.active_device = Some(addr));
                let app = self.app.clone();
                let fd: std::os::fd::OwnedFd = fd.into();
                spawn_stream(app, fd, read_mtu as usize, lease);
            }
            "idle" => {
                let stop = self.transports.get_mut(path).and_then(|t| t.stop.take());
                if let Some(stop) = stop {
                    stop.store(true, Ordering::Relaxed);
                    let _ = call(&self.conn, path, IF_TRANSPORT, "Release", &()).await;
                }
            }
            _ => {}
        }
    }

    /// Another source took the speaker from this phone: pause it and free its stream.
    async fn preempt(&mut self, addr: &str) {
        self.avrcp(addr, "pause").await;
        let paths: Vec<String> =
            self.transports.iter().filter(|(_, t)| addr_of(&t.device) == addr).map(|(p, _)| p.clone()).collect();
        for p in paths {
            if let Some(t) = self.transports.get_mut(&p) {
                if let Some(stop) = t.stop.take() {
                    stop.store(true, Ordering::Relaxed);
                }
            }
            let _ = call(&self.conn, &p, IF_TRANSPORT, "Release", &()).await;
        }
        if !self.app.cfg().multipoint {
            self.device_call(addr, IF_DEVICE, "Disconnect");
        }
    }

    /// A paired device connected (for example, picked in a tablet's Bluetooth menu).
    async fn on_device_connected(&mut self, addr: String) {
        self.connected.retain(|a| a != &addr);
        self.connected.push(addr.clone());
        let active = self.app.status.lock().unwrap().active_device.clone();
        let multipoint = self.app.cfg().multipoint;
        // Single-device mode: the new device replaces the others.
        // Multipoint: keep at most two; drop the oldest idle one.
        let keep = if multipoint { MAX_CONNECTED } else { 1 };
        while self.connected.len() > keep {
            let Some(pos) =
                self.connected.iter().position(|a| a != &addr && (!multipoint || Some(a) != active.as_ref()))
            else {
                break;
            };
            let old = self.connected.remove(pos);
            log::info!("{addr} connected; disconnecting {old}");
            if Some(&old) == active.as_ref() {
                self.preempt(&old).await;
            }
            self.device_call(&old, IF_DEVICE, "Disconnect");
        }
    }

    async fn on_cmd(&mut self, cmd: BtCmd) {
        match cmd {
            BtCmd::OpenPairing => self.open_pairing().await,
            BtCmd::ClosePairing => self.close_pairing().await,
            BtCmd::VolumeChanged(v) => {
                let vol = crate::app::to_avrcp(v);
                for path in self.transports.keys() {
                    let _ = set_prop(&self.conn, path, IF_TRANSPORT, "Volume", Variant::U16(vol)).await;
                }
            }
            BtCmd::Connect(addr) => self.device_call(&addr, IF_DEVICE, "Connect"),
            BtCmd::Disconnect(addr) => self.device_call(&addr, IF_DEVICE, "Disconnect"),
            BtCmd::Remove(addr) => {
                let dev = ObjectPath::try_from(self.device_path(&addr)).map(|p| p.into_owned());
                if let Ok(dev) = dev {
                    if let Err(e) = call(&self.conn, &self.adapter, IF_ADAPTER, "RemoveDevice", &(dev,)).await {
                        log::warn!("remove {addr}: {e}");
                    }
                }
                self.refresh_devices().await;
            }
            BtCmd::Media(action) => {
                let addr = self.app.status.lock().unwrap().active_device.clone();
                if let Some(addr) = addr {
                    self.avrcp(&addr, &action).await;
                }
            }
            BtCmd::Rename(name) => {
                let _ = set_prop(&self.conn, &self.adapter, IF_ADAPTER, "Alias", Variant::from(name.as_str())).await;
            }
            BtCmd::Preempted(addr) => {
                // Home Assistant took the speaker. Pause the phone but keep it connected,
                // so it can resume when Home Assistant is done.
                self.avrcp(&addr, "pause").await;
                let paths: Vec<String> = self
                    .transports
                    .iter()
                    .filter(|(_, t)| addr_of(&t.device) == addr)
                    .map(|(p, _)| p.clone())
                    .collect();
                for p in paths {
                    if let Some(t) = self.transports.get_mut(&p) {
                        t.stop = None;
                    }
                    let _ = call(&self.conn, &p, IF_TRANSPORT, "Release", &()).await;
                }
            }
            BtCmd::Resume(addr) => self.avrcp(&addr, "play").await,
        }
    }

    fn device_call(&self, addr: &str, iface: &'static str, method: &'static str) {
        let conn = self.conn.clone();
        let path = self.device_path(addr);
        let addr = addr.to_string();
        // Connect / Disconnect can take seconds; do not block the loop.
        tokio::spawn(async move {
            if let Err(e) = call(&conn, &path, iface, method, &()).await {
                log::warn!("{method} {addr}: {e}");
            }
        });
    }

    async fn avrcp(&self, addr: &str, action: &str) {
        let method = match action {
            "play" => "Play",
            "pause" => "Pause",
            "next" => "Next",
            "previous" => "Previous",
            "stop" => "Stop",
            _ => return,
        };
        let dev = self.device_path(addr);
        let player = match get_prop(&self.conn, &dev, "org.bluez.MediaControl1", "Player").await {
            Ok(v) => v_str(&v),
            Err(_) => None,
        };
        let player = match player {
            Some(p) => p,
            None => match managed_objects(&self.conn).await {
                Ok(objs) => {
                    match objs.keys().find(|p| p.as_str().starts_with(&dev) && objs[*p].contains_key(IF_PLAYER)) {
                        Some(p) => p.to_string(),
                        None => return,
                    }
                }
                Err(_) => return,
            },
        };
        if let Err(e) = call(&self.conn, &player, IF_PLAYER, method, &()).await {
            log::warn!("AVRCP {method} on {addr}: {e}");
        }
    }

    async fn open_pairing(&mut self) {
        let secs = self.app.cfg().pairing_window_secs;
        let _ = set_prop(&self.conn, &self.adapter, IF_ADAPTER, "PairableTimeout", Variant::U32(0)).await;
        let _ = set_prop(&self.conn, &self.adapter, IF_ADAPTER, "DiscoverableTimeout", Variant::U32(secs as u32)).await;
        let _ = set_prop(&self.conn, &self.adapter, IF_ADAPTER, "Pairable", Variant::Bool(true)).await;
        let _ = set_prop(&self.conn, &self.adapter, IF_ADAPTER, "Discoverable", Variant::Bool(true)).await;
        self.app.pairing_until.store(crate::app::now() + secs, Ordering::Relaxed);
        self.pairing_was_open = true;
        log::info!("pairing window open for {secs}s");
        self.app.notify();
    }

    async fn close_pairing(&mut self) {
        self.app.pairing_until.store(0, Ordering::Relaxed);
        if self.pairing_was_open {
            let _ = set_prop(&self.conn, &self.adapter, IF_ADAPTER, "Discoverable", Variant::Bool(false)).await;
            let _ = set_prop(&self.conn, &self.adapter, IF_ADAPTER, "Pairable", Variant::Bool(false)).await;
            self.pairing_was_open = false;
            log::info!("pairing window closed");
        }
        self.app.notify();
    }

    async fn on_tick(&mut self) {
        if self.pairing_was_open {
            if self.app.pairing_open() {
                self.app.notify(); // countdown in the UI
            } else {
                self.close_pairing().await;
            }
        }
    }
}

fn spawn_stream(app: Arc<App>, fd: std::os::fd::OwnedFd, mtu: usize, lease: Lease) {
    let res = std::thread::Builder::new().name("bt-audio".into()).spawn(move || {
        raise_priority();
        run_bt_stream(app, fd, mtu, lease)
    });
    if let Err(e) = res {
        log::error!("cannot start audio thread: {e}");
    }
}

/// Real-time priority for the audio thread when CAP_SYS_NICE is granted; otherwise nice -10 or nothing.
pub fn raise_priority() {
    unsafe {
        // Zeroed: musl's sched_param has extra fields.
        let mut param: libc::sched_param = std::mem::zeroed();
        param.sched_priority = 20;
        if libc::pthread_setschedparam(libc::pthread_self(), libc::SCHED_FIFO, &param) != 0 {
            libc::setpriority(libc::PRIO_PROCESS, 0, -10);
        }
    }
}

// ---------------------------------------------------------------- D-Bus helpers

/// BlueZ can stall (adapter reset, heavy scanning). No call may block the session for long.
const DBUS_TIMEOUT: Duration = Duration::from_secs(10);

async fn with_timeout<T>(f: impl std::future::Future<Output = zbus::Result<T>>) -> zbus::Result<T> {
    tokio::time::timeout(DBUS_TIMEOUT, f)
        .await
        .unwrap_or_else(|_| Err(zbus::Error::Failure("BlueZ did not answer in time".into())))
}

async fn call<B>(conn: &Connection, path: &str, iface: &str, method: &str, body: &B) -> zbus::Result<Message>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    with_timeout(conn.call_method(Some(BLUEZ), path, Some(iface), method, body)).await
}

async fn get_prop(conn: &Connection, path: &str, iface: &str, name: &str) -> zbus::Result<OwnedVariant> {
    let reply = call(conn, path, "org.freedesktop.DBus.Properties", "Get", &(iface, name)).await?;
    reply.body().deserialize::<OwnedVariant>()
}

async fn set_prop(conn: &Connection, path: &str, iface: &str, name: &str, value: Variant<'_>) -> zbus::Result<()> {
    call(conn, path, "org.freedesktop.DBus.Properties", "Set", &(iface, name, value)).await?;
    Ok(())
}

async fn managed_objects(conn: &Connection) -> zbus::Result<ManagedObjects> {
    let reply = call(conn, "/", "org.freedesktop.DBus.ObjectManager", "GetManagedObjects", &()).await?;
    reply.body().deserialize::<ManagedObjects>()
}

fn v_bool(v: &OwnedVariant) -> Option<bool> {
    match &**v {
        Variant::Bool(b) => Some(*b),
        _ => None,
    }
}

fn v_u16(v: &OwnedVariant) -> Option<u16> {
    match &**v {
        Variant::U16(x) => Some(*x),
        _ => None,
    }
}

fn v_str(v: &OwnedVariant) -> Option<String> {
    match &**v {
        Variant::Str(s) => Some(s.to_string()),
        Variant::ObjectPath(p) => Some(p.to_string()),
        _ => None,
    }
}

fn v_dict(v: &OwnedVariant) -> HashMap<String, String> {
    let Ok(owned) = v.try_clone() else { return HashMap::new() };
    let Ok(map) = HashMap::<String, OwnedVariant>::try_from(owned) else { return HashMap::new() };
    map.into_iter().filter_map(|(k, v)| v_str(&v).map(|s| (k, s))).collect()
}

/// "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF" → "AA:BB:CC:DD:EE:FF"
pub fn addr_of(path: &str) -> String {
    path.rsplit('/').find(|s| s.starts_with("dev_")).map(|s| s[4..].replace('_', ":")).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_bitpool_and_prefers_quality() {
        let cfg = select_sbc(&[0xff, 0xff, 2, 53], 35).unwrap();
        assert_eq!(cfg, vec![0x20 | 0x01, 0x10 | 0x04 | 0x01, 2, 35]);
        assert!(select_sbc(&[0x00, 0xff, 2, 53], 35).is_none());
    }

    #[test]
    fn address_from_path() {
        assert_eq!(addr_of("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF/fd0"), "AA:BB:CC:DD:EE:FF");
    }
}
