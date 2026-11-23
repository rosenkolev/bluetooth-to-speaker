//! Run as a normal user. The container starts as root only to hand the data folder and the
//! sound devices to that user; then it drops root before any thread starts.
//!
//! Why: the app talks to the host's system D-Bus, and D-Bus trusts uid 0 as the host's
//! root. As a normal user the app can reach BlueZ, and nothing else that needs root.
//!
//! The drop happens only when it is safe: the user can then still write the data folder,
//! open the sound devices and use BlueZ. Otherwise the app stays root and reports why
//! (see `checks`), so it always works.

use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::Path;

const CAP_SYS_NICE: u32 = 23;
const DBUS_SOCKET: &str = "/run/dbus/system_bus_socket";

/// Result of the start-up privilege drop.
#[derive(Clone, Debug)]
pub enum Privilege {
    /// Runs as this user. `realtime`: CAP_SYS_NICE kept for the audio threads.
    User { uid: u32, gid: u32, realtime: bool },
    /// Started as a normal user (Docker `user:`); nothing to drop.
    Started { uid: u32 },
    /// Still root, and why.
    Root { reason: RootReason },
}

#[derive(Clone, Debug)]
pub enum RootReason {
    /// RUN_AS_ROOT=1 is set.
    Asked,
    /// Missing CHOWN, SETUID or SETGID capability (cap_drop: ALL without adding them).
    NoCapabilities,
    /// The data folder cannot be handed to the user.
    DataFolder(String),
    /// A sound device the user could not open.
    SoundDevice(String),
    /// The host's BlueZ D-Bus policy allows only root.
    BluezPolicy,
    /// HTTP port below 1024.
    LowPort(u16),
}

/// Drop root if it is safe. Must run before any thread starts.
pub fn drop_root(data_dir: &Path, http_port: u16) -> Privilege {
    let euid = unsafe { libc::geteuid() };
    if euid != 0 {
        return Privilege::Started { uid: euid };
    }
    if std::env::var("RUN_AS_ROOT").is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true")) {
        return Privilege::Root { reason: RootReason::Asked };
    }
    if http_port < 1024 {
        return Privilege::Root { reason: RootReason::LowPort(http_port) };
    }
    let (uid, gid) = target_user(data_dir);

    // Groups of the sound devices, so the user can open them.
    let mut groups: Vec<u32> = Vec::new();
    for node in sound_nodes() {
        let Ok(meta) = std::fs::metadata(&node) else { continue };
        if meta.gid() != 0 && !groups.contains(&meta.gid()) {
            groups.push(meta.gid());
        }
    }
    let mut all_groups = groups.clone();
    all_groups.push(gid);
    for node in sound_nodes() {
        if let Ok(meta) = std::fs::metadata(&node) {
            if !can_rw(&meta, uid, &all_groups) {
                return Privilege::Root { reason: RootReason::SoundDevice(node) };
            }
        }
    }
    if let Ok(meta) = std::fs::metadata(DBUS_SOCKET) {
        if !can_rw(&meta, uid, &all_groups) {
            return Privilege::Root { reason: RootReason::BluezPolicy };
        }
    }

    // A trial run in a child process: can the user reach BlueZ? Some distributions allow
    // only root and the "bluetooth" group (whose number differs on every host).
    if Path::new(DBUS_SOCKET).exists() && !child_can_use_bluez(uid, gid, &groups) {
        return Privilege::Root { reason: RootReason::BluezPolicy };
    }

    if let Err(e) = chown_tree(data_dir, uid, gid) {
        // Already the user's folder (Docker Apps, PUID): fine. Otherwise stay root.
        let owned = std::fs::metadata(data_dir).map(|m| m.uid() == uid).unwrap_or(false);
        if !owned {
            return if e.raw_os_error() == Some(libc::EPERM) {
                Privilege::Root { reason: RootReason::NoCapabilities }
            } else {
                Privilege::Root { reason: RootReason::DataFolder(format!("{}: {e}", data_dir.display())) }
            };
        }
    }

    match switch_user(uid, gid, &groups) {
        Ok(realtime) => Privilege::User { uid, gid, realtime },
        Err(_) => Privilege::Root { reason: RootReason::NoCapabilities },
    }
}

/// PUID/PGID, else the owner of an existing data folder, else 1000.
fn target_user(data_dir: &Path) -> (u32, u32) {
    let env = |k: &str| std::env::var(k).ok().and_then(|v| v.trim().parse::<u32>().ok()).filter(|&v| v != 0);
    let owner = std::fs::metadata(data_dir).ok().filter(|m| m.uid() != 0).map(|m| (m.uid(), m.gid()));
    let uid = env("PUID").or(owner.map(|o| o.0)).unwrap_or(1000);
    let gid = env("PGID").or(owner.map(|o| o.1).filter(|&g| g != 0)).unwrap_or(uid);
    (uid, gid)
}

fn sound_nodes() -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir("/dev/snd")
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_char_device()).unwrap_or(false))
        .map(|e| e.path().display().to_string())
        .collect();
    out.sort();
    out
}

/// Read and write permission for this user, by the mode bits.
fn can_rw(meta: &std::fs::Metadata, uid: u32, groups: &[u32]) -> bool {
    let mode = meta.permissions().mode();
    let bits = if meta.uid() == uid {
        mode >> 6
    } else if groups.contains(&meta.gid()) {
        mode >> 3
    } else {
        mode
    };
    bits & 0o6 == 0o6
}

fn chown_tree(path: &Path, uid: u32, gid: u32) -> std::io::Result<()> {
    std::os::unix::fs::lchown(path, Some(uid), Some(gid))?;
    if path.is_dir() && !path.is_symlink() {
        for entry in std::fs::read_dir(path)? {
            chown_tree(&entry?.path(), uid, gid)?;
        }
    }
    Ok(())
}

/// Become the user; keep CAP_SYS_NICE (real-time audio) when the container has it.
/// Returns whether CAP_SYS_NICE was kept.
fn switch_user(uid: u32, gid: u32, groups: &[u32]) -> std::io::Result<bool> {
    let had_nice = caps().map(|c| c.permitted & (1 << CAP_SYS_NICE) != 0).unwrap_or(false);
    let check = |r: libc::c_int| if r == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) };
    unsafe {
        let gids: Vec<libc::gid_t> = groups.iter().map(|&g| g as libc::gid_t).collect();
        check(libc::setgroups(gids.len() as _, gids.as_ptr()))?;
        check(libc::setresgid(gid, gid, gid))?;
        check(libc::prctl(libc::PR_SET_KEEPCAPS, 1, 0, 0, 0))?;
        check(libc::setresuid(uid, uid, uid))?;
        libc::prctl(libc::PR_SET_KEEPCAPS, 0, 0, 0, 0);
    }
    // Every capability is gone from the effective set; keep only CAP_SYS_NICE.
    let keep = if had_nice { 1u32 << CAP_SYS_NICE } else { 0 };
    let kept = set_caps(keep).is_ok() && had_nice;
    if !kept {
        let _ = set_caps(0);
    }
    Ok(kept)
}

#[repr(C)]
struct CapHeader {
    version: u32,
    pid: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CapData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

const CAP_V3: u32 = 0x2008_0522;

/// Lower 32 capabilities of this process.
fn caps() -> std::io::Result<CapData> {
    let mut hdr = CapHeader { version: CAP_V3, pid: 0 };
    let mut data = [CapData::default(); 2];
    let r = unsafe { libc::syscall(libc::SYS_capget, &mut hdr as *mut CapHeader, data.as_mut_ptr()) };
    if r == 0 {
        Ok(data[0])
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Set effective and permitted to `mask` (lower 32 capabilities); nothing above.
fn set_caps(mask: u32) -> std::io::Result<()> {
    let mut hdr = CapHeader { version: CAP_V3, pid: 0 };
    let data = [CapData { effective: mask, permitted: mask, inheritable: 0 }, CapData::default()];
    let r = unsafe { libc::syscall(libc::SYS_capset, &mut hdr as *mut CapHeader, data.as_ptr()) };
    if r == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Fork, become the user in the child, and ask BlueZ for its objects. False only when the
/// D-Bus policy refuses the user (BlueZ not running is not a policy problem).
fn child_can_use_bluez(uid: u32, gid: u32, groups: &[u32]) -> bool {
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return true; // cannot test; assume the common case (allowed)
    }
    if pid == 0 {
        // Child: one thread, never returns.
        let code = match switch_user(uid, gid, groups) {
            Err(_) => 0, // the parent finds out itself
            Ok(_) => match bluez_probe() {
                Probe::Denied => 1,
                _ => 0,
            },
        };
        unsafe { libc::_exit(code) };
    }
    // Parent: wait up to 5 seconds.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let mut status = 0;
        let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if r == pid {
            return !(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 1);
        }
        if r < 0 || std::time::Instant::now() >= deadline {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
                libc::waitpid(pid, &mut status, 0);
            }
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

enum Probe {
    Ok,
    Denied,
    Other,
}

fn bluez_probe() -> Probe {
    let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return Probe::Other };
    let reply = rt.block_on(async {
        let conn = zbus::Connection::system().await?;
        let call = conn.call_method(
            Some("org.bluez"),
            "/",
            Some("org.freedesktop.DBus.ObjectManager"),
            "GetManagedObjects",
            &(),
        );
        tokio::time::timeout(std::time::Duration::from_secs(3), call)
            .await
            .unwrap_or_else(|_| Err(zbus::Error::Failure("timeout".into())))
    });
    match reply {
        Ok(_) => Probe::Ok,
        Err(zbus::Error::MethodError(name, _, _)) if name.as_str() == "org.freedesktop.DBus.Error.AccessDenied" => {
            Probe::Denied
        }
        Err(_) => Probe::Other,
    }
}

/// Whether the audio threads may use real-time priority.
pub fn realtime_available() -> bool {
    caps().map(|c| c.effective & (1 << CAP_SYS_NICE) != 0).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_bits() {
        let dir = std::env::temp_dir().join(format!("b2s-perm-{}", std::process::id()));
        std::fs::write(&dir, b"").unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o660)).unwrap();
        let meta = std::fs::metadata(&dir).unwrap();
        assert!(can_rw(&meta, meta.uid(), &[]));
        assert!(can_rw(&meta, meta.uid() + 1, &[meta.gid()]));
        assert!(!can_rw(&meta, meta.uid() + 1, &[meta.gid() + 1]));
        std::fs::remove_file(&dir).unwrap();
    }
}
