//! Setup checks. Each problem names the cause and the exact fix, and shows in the web UI,
//! the log and Home Assistant (MQTT). The app never changes the host itself: it only reads
//! (as a normal user, through D-Bus) and tells the user what to run.

use crate::app::{App, Source};
use crate::privilege::{Privilege, RootReason};
use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use zbus::{Connection, OwnedVariant, Variant};

const DBUS_SOCKET: &str = "/run/dbus/system_bus_socket";
const HELP: &str = "https://github.com/rosenkolev/bluetooth-to-speaker/blob/main/docs/troubleshooting.md";
/// Other programs that receive Bluetooth audio, as system services.
/// PipeWire and PulseAudio run in the user's session, out of sight: `bluez` sees them when
/// a phone's stream goes to them.
const OTHER_SINK_UNITS: [&str; 3] = ["bluealsa.service", "bluez-alsa.service", "bluealsa-aplay.service"];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    /// Something does not work.
    Error,
    /// Works, but not as it should.
    Warning,
    /// Good to know.
    Info,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Problem {
    pub id: String,
    pub level: Level,
    pub title: String,
    pub detail: String,
    /// Commands or settings that fix it, one per line.
    pub fix: Vec<String>,
    /// Link to the troubleshooting section.
    pub help: String,
}

impl Problem {
    pub fn new(id: &str, level: Level, title: impl Into<String>, detail: impl Into<String>) -> Problem {
        Problem {
            id: id.into(),
            level,
            title: title.into(),
            detail: detail.into(),
            fix: Vec::new(),
            help: String::new(),
        }
    }

    pub fn fix(mut self, lines: &[&str]) -> Problem {
        self.fix = lines.iter().map(|s| s.to_string()).collect();
        self
    }

    pub fn fix_owned(mut self, lines: Vec<String>) -> Problem {
        self.fix = lines;
        self
    }

    pub fn help(mut self, anchor: &str) -> Problem {
        self.help = format!("{HELP}#{anchor}");
        self
    }
}

/// A phone's audio goes to another program on the host. `id`: "other_sink" (found by the
/// checks) or "other_sink_stream" (seen when a phone connected).
pub fn other_sink(id: &str, device: &str, who: &str) -> Problem {
    Problem::new(
        id,
        Level::Error,
        "Another program takes the phone's audio",
        format!(
            "{device}: the sound goes to {who} on the host, not to this app. \
             Only one program can be the Bluetooth speaker."
        ),
    )
    .fix(&[
        "# bluez-alsa:",
        "sudo systemctl disable --now bluealsa bluealsa-aplay",
        "# PipeWire (desktop): keep headsets working, stop being a speaker:",
        "mkdir -p ~/.config/wireplumber/wireplumber.conf.d",
        "echo 'monitor.bluez.properties = { bluez5.roles = [ a2dp_source hfp_ag hsp_ag bap_source ] }' > ~/.config/wireplumber/wireplumber.conf.d/90-no-speaker.conf",
        "systemctl --user restart wireplumber",
        "# PulseAudio: remove its Bluetooth module:",
        "sudo apt remove pulseaudio-module-bluetooth",
    ])
    .help("another-program-takes-the-phones-audio")
}

/// Runs the checks at start, every minute, and when asked (web UI "Check again").
pub async fn run(app: Arc<App>) {
    tokio::time::sleep(Duration::from_secs(3)).await; // let BlueZ and MQTT start first
    privilege_problems(&app);
    let mut conn: Option<Connection> = None;
    // Opening an idle output can click on some sound cards: test every output only at
    // start and when asked; the timer re-tests only outputs that have a problem.
    let mut full = true;
    loop {
        // A connection from before a D-Bus restart answers nothing: open a new one.
        if let Some(c) = &conn {
            if !bus_alive(c).await {
                log::info!("D-Bus connection lost; reconnecting");
                conn = None;
            }
        }
        if conn.is_none() && Path::new(DBUS_SOCKET).exists() {
            conn = Connection::system().await.ok();
        }
        check_dbus(&app, conn.as_ref()).await;
        if let Some(c) = &conn {
            check_bluez(&app, c).await;
            check_other_sinks(&app, c).await;
            check_firewall(&app, c).await;
        }
        {
            let app = app.clone();
            let _ = tokio::task::spawn_blocking(move || {
                check_data_dir(&app);
                check_sound(&app, full);
            })
            .await;
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(60)) => full = false,
            _ = app.check_now.notified() => {
                full = true;
                // Seen when a phone connected; the next connection shows it again if it is still true.
                app.clear_problem("other_sink_stream");
            }
        }
    }
}

async fn bus_alive(conn: &Connection) -> bool {
    dbus_call(conn, "org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus.Peer", "Ping", &())
        .await
        .is_ok()
}

fn privilege_problems(app: &App) {
    let compose = "cap_add: [CHOWN, SETUID, SETGID, SYS_NICE]";
    match &app.privilege {
        Privilege::Root { reason } => {
            let (detail, fix): (String, Vec<String>) = match reason {
                RootReason::Asked => ("RUN_AS_ROOT is set.".into(), vec!["# remove RUN_AS_ROOT from the environment".into()]),
                RootReason::NoCapabilities => (
                    "The container may not switch to a normal user: it lacks the CHOWN, SETUID and SETGID capabilities."
                        .into(),
                    vec![format!("# docker compose: {compose}")],
                ),
                RootReason::DataFolder(e) => (
                    format!("The data folder cannot be given to the app's user: {e}."),
                    vec!["sudo chown -R 1000:1000 <data folder on the host>".into()],
                ),
                RootReason::SoundDevice(node) => (
                    format!("A normal user cannot open {node}."),
                    vec!["# check the device's group: ls -l /dev/snd".into()],
                ),
                RootReason::BluezPolicy => (
                    "The host's BlueZ allows only root (its D-Bus policy). To run as a normal user, allow user 1000:"
                        .into(),
                    vec![
                        "echo '<busconfig><policy user=\"1000\"><allow send_destination=\"org.bluez\"/></policy></busconfig>' | sudo tee /etc/dbus-1/system.d/bluetooth-to-speaker.conf".into(),
                        "sudo systemctl reload dbus".into(),
                        "# then restart the app".into(),
                    ],
                ),
                RootReason::LowPort(p) => {
                    (format!("HTTP_PORT {p} is below 1024."), vec!["# use HTTP_PORT 1024 or higher".into()])
                }
            };
            let level = if matches!(reason, RootReason::Asked) { Level::Info } else { Level::Warning };
            app.set_problem(
                Problem::new(
                    "root",
                    level,
                    "Runs as root",
                    format!("Everything works, but the app runs as root on the host's D-Bus. {detail}"),
                )
                .fix_owned(fix)
                .help("runs-as-root"),
            );
        }
        Privilege::User { uid, gid, realtime } => {
            let rt = if *realtime { ", real-time audio allowed" } else { "" };
            log::info!("running as user {uid}:{gid}{rt}");
        }
        Privilege::Started { uid } => log::info!("running as user {uid}"),
    }
    if !crate::privilege::realtime_available() {
        app.set_problem(
            Problem::new(
                "realtime",
                Level::Info,
                "No real-time priority for audio",
                "Sound may stutter when the host is busy. Allow it with the SYS_NICE capability.",
            )
            .fix(&["# docker compose: cap_add: [SYS_NICE]"])
            .help("stutter"),
        );
    }
}

async fn check_dbus(app: &App, conn: Option<&Connection>) {
    if !Path::new(DBUS_SOCKET).exists() {
        app.set_problem(
            Problem::new(
                "dbus",
                Level::Error,
                "No access to the host's Bluetooth service",
                "The host's D-Bus socket is not in the container, so the app cannot reach BlueZ.",
            )
            .fix(&["# docker compose, under volumes:", "- /run/dbus:/run/dbus:ro"])
            .help("bluez-not-reachable-or-no-adapter"),
        );
    } else if conn.is_none() {
        app.set_problem(Problem::new(
            "dbus",
            Level::Error,
            "Cannot connect to the host's D-Bus",
            "The socket /run/dbus/system_bus_socket is there, but the connection failed. See the log.",
        ));
    } else {
        app.clear_problem("dbus");
    }
}

async fn check_bluez(app: &App, conn: &Connection) {
    if has_owner(conn, "org.bluez").await {
        app.clear_problem("bluez");
        return;
    }
    let os = os_name(conn).await;
    let install = install_command(&os);
    let systemd = has_owner(conn, "org.freedesktop.systemd1").await;
    let state = if systemd { unit_state(conn, "bluetooth.service").await } else { None };
    let p = match state {
        Some((Some(file_state), _)) if file_state == "masked" => Problem::new(
            "bluez",
            Level::Error,
            "Bluetooth service is switched off",
            "BlueZ is installed, but its service is masked (blocked from starting).",
        )
        .fix(&["sudo systemctl unmask bluetooth", "sudo systemctl enable --now bluetooth"]),
        Some((file_state, active)) if file_state.is_some() => Problem::new(
            "bluez",
            Level::Error,
            "Bluetooth service is not running",
            format!("BlueZ is installed, but bluetoothd is {active}."),
        )
        .fix(&["sudo systemctl enable --now bluetooth"]),
        _ if systemd => Problem::new(
            "bluez",
            Level::Error,
            "Bluetooth service (BlueZ) is not installed",
            format!(
                "The host{} has no BlueZ, the Linux Bluetooth service.",
                if os.is_empty() { String::new() } else { format!(" ({os})") }
            ),
        )
        .fix_owned(vec![install, "sudo systemctl enable --now bluetooth".into()]),
        _ => Problem::new(
            "bluez",
            Level::Error,
            "Bluetooth service (BlueZ) is not running",
            "bluetoothd does not answer on the host's D-Bus. Install BlueZ and start it.",
        )
        .fix_owned(vec![install]),
    };
    app.set_problem(p.help("bluez-not-reachable-or-no-adapter"));
}

/// Other programs that are Bluetooth speakers too (system services and bus names).
async fn check_other_sinks(app: &App, conn: &Connection) {
    let mut found: Vec<String> = Vec::new();
    if let Ok(names) =
        dbus_call(conn, "org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus", "ListNames", &()).await
    {
        if let Ok(names) = names.body().deserialize::<Vec<String>>() {
            if names.iter().any(|n| n.starts_with("org.bluealsa")) {
                found.push("bluez-alsa".into());
            }
        }
    }
    if found.is_empty() && has_owner(conn, "org.freedesktop.systemd1").await {
        for unit in OTHER_SINK_UNITS {
            if let Some((_, active)) = unit_state(conn, unit).await {
                if active == "active" {
                    found.push(unit.trim_end_matches(".service").into());
                }
            }
        }
    }
    if found.is_empty() {
        app.clear_problem("other_sink");
    } else {
        app.set_problem(other_sink("other_sink", "Phones", &found.join(", ")));
    }
}

/// firewalld: are 8780/tcp and 1900/udp open in the zone of the LAN interface?
async fn check_firewall(app: &App, conn: &Connection) {
    const FW: &str = "org.fedoraproject.FirewallD1";
    const PATH: &str = "/org/fedoraproject/FirewallD1";
    if !has_owner(conn, FW).await {
        app.clear_problem("firewall");
        return;
    }
    let http = app.cfg().http_port;
    let Some(iface) = crate::net::local_ip().and_then(crate::net::interface_of) else { return };
    let zone =
        match dbus_call(conn, FW, PATH, "org.fedoraproject.FirewallD1.zone", "getZoneOfInterface", &(iface.as_str(),))
            .await
        {
            Ok(r) => r.body().deserialize::<String>().unwrap_or_default(),
            Err(e) => {
                log::debug!("firewalld: {e}");
                return;
            }
        };
    let zone = if zone.is_empty() {
        match dbus_call(conn, FW, PATH, FW, "getDefaultZone", &()).await {
            Ok(r) => r.body().deserialize::<String>().unwrap_or_default(),
            Err(_) => return,
        }
    } else {
        zone
    };
    let target =
        match dbus_call(conn, FW, PATH, "org.fedoraproject.FirewallD1.zone", "getZoneSettings2", &(zone.as_str(),))
            .await
        {
            Ok(r) => r
                .body()
                .deserialize::<HashMap<String, OwnedVariant>>()
                .ok()
                .and_then(|m| {
                    m.get("target").and_then(|v| match &**v {
                        Variant::Str(s) => Some(s.to_string()),
                        _ => None,
                    })
                })
                .unwrap_or_default(),
            Err(_) => String::new(),
        };
    if target == "ACCEPT" || zone == "trusted" {
        app.clear_problem("firewall");
        return;
    }
    let ports: Vec<Vec<String>> =
        match dbus_call(conn, FW, PATH, "org.fedoraproject.FirewallD1.zone", "getPorts", &(zone.as_str(),)).await {
            Ok(r) => r.body().deserialize().unwrap_or_default(),
            Err(e) => {
                log::debug!("firewalld getPorts: {e}");
                return;
            }
        };
    let services: Vec<String> =
        match dbus_call(conn, FW, PATH, "org.fedoraproject.FirewallD1.zone", "getServices", &(zone.as_str(),)).await {
            Ok(r) => r.body().deserialize().unwrap_or_default(),
            Err(_) => Vec::new(),
        };
    let open = |port: u16, proto: &str| ports.iter().any(|p| p.len() == 2 && p[1] == proto && port_in(&p[0], port));
    let mut missing = Vec::new();
    if !open(http, "tcp") {
        missing.push(format!("{http}/tcp"));
    }
    if !open(1900, "udp") && !services.iter().any(|s| s == "ssdp") {
        missing.push("1900/udp".to_string());
    }
    if missing.is_empty() {
        app.clear_problem("firewall");
        return;
    }
    let add: String = missing.iter().map(|m| format!(" --add-port={m}")).collect();
    app.set_problem(
        Problem::new(
            "firewall",
            Level::Warning,
            "The firewall blocks other devices",
            format!(
                "firewalld zone \"{zone}\" ({iface}) does not open {}. The web UI opens only on this host, and Home \
                 Assistant may not find or reach the speaker.",
                missing.join(" and ")
            ),
        )
        .fix_owned(vec![
            format!("sudo firewall-cmd --permanent --zone={zone}{add}"),
            "sudo firewall-cmd --reload".into(),
        ])
        .help("the-web-ui-does-not-open-from-other-devices"),
    );
}

/// "8780" or "8000-9000" contains `port`.
fn port_in(spec: &str, port: u16) -> bool {
    match spec.split_once('-') {
        Some((a, b)) => match (a.trim().parse::<u16>(), b.trim().parse::<u16>()) {
            (Ok(a), Ok(b)) => (a..=b).contains(&port),
            _ => false,
        },
        None => spec.trim().parse::<u16>() == Ok(port),
    }
}

fn check_data_dir(app: &App) {
    let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
    if let Some(e) = crate::config::UNREADABLE.lock().unwrap().clone() {
        app.set_problem(
            Problem::new(
                "data",
                Level::Error,
                "Settings cannot be read",
                format!(
                    "config.json in the data folder belongs to another user ({e}). The app uses default settings and \
                     does not save, so your settings stay safe. Give the folder to the app's user, then restart the app."
                ),
            )
            .fix_owned(vec![format!("sudo chown -R {uid}:{gid} <data folder on the host>")])
            .help("settings-are-lost-after-a-restart"),
        );
        return;
    }
    if crate::app::TEMPORARY_UID.load(std::sync::atomic::Ordering::Relaxed) {
        app.set_problem(
            Problem::new(
                "data",
                Level::Warning,
                "Device id cannot be read",
                "The uid file in the data folder belongs to another user. Home Assistant may see this speaker as a \
                 new device after each restart.",
            )
            .fix_owned(vec![format!("sudo chown -R {uid}:{gid} <data folder on the host>")])
            .help("settings-are-lost-after-a-restart"),
        );
        return;
    }
    let test = app.data_dir.join(".write-test");
    match std::fs::write(&test, b"ok") {
        Ok(()) => {
            let _ = std::fs::remove_file(&test);
            app.clear_problem("data");
        }
        Err(e) => {
            app.set_problem(
                Problem::new(
                    "data",
                    Level::Error,
                    "Settings cannot be saved",
                    format!(
                        "The data folder ({}) is not writable: {e}. Settings, paired-phone memory and the Home Assistant \
                         device id are lost on restart.",
                        app.data_dir.display()
                    ),
                )
                .fix_owned(vec![format!("sudo chown -R {uid}:{gid} <data folder on the host>")])
                .help("settings-are-lost-after-a-restart"),
            );
        }
    }
}

fn check_sound(app: &App, full: bool) {
    let has_pcm = std::fs::read_dir("/dev/snd")
        .map(|d| d.flatten().any(|e| e.file_name().to_string_lossy().ends_with('p')))
        .unwrap_or(false);
    if !has_pcm {
        app.set_problem(
            Problem::new(
                "sound",
                Level::Error,
                "No sound card",
                if Path::new("/dev/snd").exists() {
                    "The host has no sound output (/dev/snd has no playback device)."
                } else {
                    "The sound devices are not in the container."
                },
            )
            .fix(&["# docker compose:", "devices:", "  - /dev/snd:/dev/snd"])
            .help("no-sound"),
        );
    } else {
        app.clear_problem("sound");
    }

    // Try each enabled output while nothing plays (our own stream, or an output kept open
    // between songs, would hold it).
    if app.output.current() != Source::Idle || app.outputs_parked() {
        return;
    }
    let outputs = app.cfg().outputs;
    let known: Vec<String> = app.problems().into_iter().map(|p| p.id).filter(|id| id.starts_with("output:")).collect();
    let mut seen = Vec::new();
    for o in outputs.iter().filter(|o| o.enabled) {
        let id = format!("output:{}", o.device);
        seen.push(id.clone());
        if !full && !known.contains(&id) {
            continue;
        }
        let res = alsa::pcm::PCM::new(&o.device, alsa::Direction::Playback, true);
        let errno = match res {
            Ok(_) => {
                app.clear_problem(&id);
                continue;
            }
            Err(e) => e.errno(),
        };
        let p = match errno {
            libc::EBUSY => Problem::new(
                &id,
                Level::Error,
                format!("Output \"{}\" is used by another program", o.name),
                format!(
                    "{} is busy. Another program on the host keeps it open, usually PulseAudio, PipeWire or another \
                     player.",
                    o.device
                ),
            )
            .fix(&["sudo fuser -v /dev/snd/*   # shows the program", "# stop that program, or pick another output"]),
            libc::EACCES | libc::EPERM => Problem::new(
                &id,
                Level::Error,
                format!("Output \"{}\": access denied", o.name),
                format!("The app may not open {}.", o.device),
            )
            .fix(&[
                "ls -ln /dev/snd   # the group number of the devices",
                "# docker compose: group_add: [\"<that number>\"]",
            ]),
            _ => Problem::new(
                &id,
                Level::Warning,
                format!("Output \"{}\" is not available", o.name),
                format!(
                    "{} cannot be opened ({}). An HDMI TV may be off, or a USB card unplugged.",
                    o.device,
                    std::io::Error::from_raw_os_error(errno)
                ),
            ),
        };
        app.set_problem(p.help("no-sound"));
    }
    app.retain_problems(|p| !p.id.starts_with("output:") || seen.contains(&p.id));
}

// ---------------------------------------------------------------- host info over D-Bus

async fn dbus_call<B>(
    conn: &Connection,
    dest: &str,
    path: &str,
    iface: &str,
    method: &str,
    body: &B,
) -> zbus::Result<zbus::Message>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    tokio::time::timeout(Duration::from_secs(5), conn.call_method(Some(dest), path, Some(iface), method, body))
        .await
        .unwrap_or_else(|_| Err(zbus::Error::Failure("no answer".into())))
}

async fn has_owner(conn: &Connection, name: &str) -> bool {
    match dbus_call(
        conn,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
        "NameHasOwner",
        &(name,),
    )
    .await
    {
        Ok(r) => r.body().deserialize::<bool>().unwrap_or(false),
        Err(_) => false,
    }
}

/// (unit file state, active state) of a systemd unit; None if unknown to systemd.
async fn unit_state(conn: &Connection, unit: &str) -> Option<(Option<String>, String)> {
    const SD: &str = "org.freedesktop.systemd1";
    const PATH: &str = "/org/freedesktop/systemd1";
    let file_state = dbus_call(conn, SD, PATH, "org.freedesktop.systemd1.Manager", "GetUnitFileState", &(unit,))
        .await
        .ok()
        .and_then(|r| r.body().deserialize::<String>().ok());
    let path = dbus_call(conn, SD, PATH, "org.freedesktop.systemd1.Manager", "LoadUnit", &(unit,))
        .await
        .ok()?
        .body()
        .deserialize::<zbus::OwnedObjectPath>()
        .ok()?;
    let reply = dbus_call(
        conn,
        SD,
        path.as_str(),
        "org.freedesktop.DBus.Properties",
        "Get",
        &("org.freedesktop.systemd1.Unit", "ActiveState"),
    )
    .await
    .ok()?;
    let v = reply.body().deserialize::<OwnedVariant>().ok()?;
    let active = match &*v {
        Variant::Str(s) => s.to_string(),
        _ => return None,
    };
    if file_state.is_none() && active == "inactive" {
        // LoadUnit returns a stub for units that do not exist.
        return None;
    }
    Some((file_state, active))
}

async fn os_name(conn: &Connection) -> String {
    let r = dbus_call(
        conn,
        "org.freedesktop.hostname1",
        "/org/freedesktop/hostname1",
        "org.freedesktop.DBus.Properties",
        "Get",
        &("org.freedesktop.hostname1", "OperatingSystemPrettyName"),
    )
    .await;
    r.ok()
        .and_then(|r| r.body().deserialize::<OwnedVariant>().ok())
        .and_then(|v| match &*v {
            Variant::Str(s) => Some(s.to_string()),
            _ => None,
        })
        .unwrap_or_default()
}

/// The install command for BlueZ on this host's distribution.
fn install_command(os: &str) -> String {
    let os = os.to_lowercase();
    let has = |names: &[&str]| names.iter().any(|n| os.contains(n));
    if has(&["debian", "ubuntu", "raspbian", "armbian", "dietpi", "mint", "pop!_os", "kali", "openmediavault"]) {
        "sudo apt install bluez".into()
    } else if has(&["fedora", "red hat", "centos", "rocky", "alma"]) {
        "sudo dnf install bluez".into()
    } else if has(&["arch", "manjaro", "endeavour"]) {
        "sudo pacman -S bluez bluez-utils".into()
    } else if has(&["suse"]) {
        "sudo zypper install bluez".into()
    } else if has(&["alpine"]) {
        "sudo apk add bluez && sudo rc-update add bluetooth && sudo rc-service bluetooth start".into()
    } else {
        "# install BlueZ with your package manager: apt install bluez | dnf install bluez | pacman -S bluez".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_ranges() {
        assert!(port_in("8780", 8780));
        assert!(port_in("8000-9000", 8780));
        assert!(!port_in("8000-8100", 8780));
        assert!(!port_in("x", 8780));
    }

    #[test]
    fn install_commands() {
        assert_eq!(install_command("Debian GNU/Linux 12 (bookworm)"), "sudo apt install bluez");
        assert_eq!(install_command("Fedora Linux 41"), "sudo dnf install bluez");
        assert!(install_command("").starts_with('#'));
    }
}
