//! Small OS helpers: instance locks, systemd notification, service control
//!
//! External programs are only ever started with fixed argument vectors
//! through `std::process::Command`, never through a shell

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::{Command, Stdio};

pub const SERVICE_UNIT: &str = "dell-fan-control.service";

/// Take an exclusive, non blocking lock. The lock lives as long as the file
pub fn try_lock(path: &Path) -> Result<Option<File>, String> {
    let f = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|e| format!("无法打开锁文件 {}：{e}", path.display()))?;
    // SAFETY: valid fd owned by `f`
    let r = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if r == 0 {
        Ok(Some(f))
    } else {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
            Ok(None)
        } else {
            Err(format!("加锁失败 {}：{err}", path.display()))
        }
    }
}

/// Send a notification to systemd if NOTIFY_SOCKET is set
pub fn sd_notify(msg: &str) {
    let Some(addr) = std::env::var_os("NOTIFY_SOCKET") else {
        return;
    };
    let Ok(sock) = std::os::unix::net::UnixDatagram::unbound() else {
        return;
    };
    let bytes = addr.as_encoded_bytes();
    let r = if let Some(name) = bytes.strip_prefix(b"@") {
        use std::os::linux::net::SocketAddrExt;
        std::os::unix::net::SocketAddr::from_abstract_name(name)
            .and_then(|a| sock.send_to_addr(msg.as_bytes(), &a))
    } else {
        sock.send_to(msg.as_bytes(), Path::new(&addr))
    };
    if let Err(e) = r {
        log::debug!("sd_notify 失败：{e}");
    }
}

/// Watchdog interval requested by systemd, if any
pub fn watchdog_enabled() -> bool {
    std::env::var("WATCHDOG_USEC").is_ok_and(|v| v.parse::<u64>().is_ok_and(|n| n > 0))
}

fn systemctl_user(args: &[&str]) -> Result<std::process::Output, String> {
    Command::new("systemctl")
        .arg("--user")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("无法执行 systemctl：{e}"))
}

/// `Some(true)` enabled, `Some(false)` disabled, `None` unit not installed
pub fn autostart_state() -> Option<bool> {
    let out = systemctl_user(&["is-enabled", SERVICE_UNIT]).ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    match s.as_str() {
        "enabled" | "enabled-runtime" => Some(true),
        "disabled" => Some(false),
        _ => None,
    }
}

pub fn set_autostart(on: bool) -> Result<(), String> {
    let out = systemctl_user(&[if on { "enable" } else { "disable" }, SERVICE_UNIT])?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

pub fn start_service() -> Result<(), String> {
    let out = systemctl_user(&["start", SERVICE_UNIT])?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Start the GUI from the background service
///
/// `systemd-run --user` gives the GUI the manager's current environment, so
/// WAYLAND_DISPLAY is right even when the service started before the desktop
/// exported it, and the GUI does not live inside the service's cgroup
pub fn launch_gui() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("找不到程序路径：{e}"))?;
    let status = Command::new("systemd-run")
        .args(["--user", "--collect", "--quiet", "--no-block", "--unit"])
        .arg(format!(
            "dell-fan-control-gui-{}",
            std::process::id() as u64 * 1000 + rand_suffix()
        ))
        .arg("--")
        .arg(&exe)
        .arg("gui")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match status {
        Ok(s) if s.success() => Ok(()),
        _ => {
            // fallback: direct child, detached into its own process group
            use std::os::unix::process::CommandExt;
            let mut cmd = Command::new(&exe);
            cmd.arg("gui")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .process_group(0);
            for (k, v) in manager_environment() {
                cmd.env(k, v);
            }
            let mut child = cmd.spawn().map_err(|e| format!("无法启动界面：{e}"))?;
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            Ok(())
        }
    }
}

fn rand_suffix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 % 1000)
        .unwrap_or(0)
}

/// Display related variables from the systemd user manager
pub fn manager_environment() -> Vec<(String, String)> {
    let Ok(out) = systemctl_user(&["show-environment"]) else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_once('='))
        .filter(|(k, _)| {
            matches!(
                *k,
                "WAYLAND_DISPLAY"
                    | "DISPLAY"
                    | "XDG_CURRENT_DESKTOP"
                    | "HYPRLAND_INSTANCE_SIGNATURE"
                    | "XDG_SESSION_TYPE"
            )
        })
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// AC adapter state from /sys/class/power_supply
pub fn on_ac_power() -> Option<bool> {
    let entries = std::fs::read_dir("/sys/class/power_supply").ok()?;
    let mut seen = false;
    for e in entries.flatten() {
        let p = e.path();
        let kind = std::fs::read_to_string(p.join("type")).unwrap_or_default();
        if kind.trim() == "Mains" {
            seen = true;
            if std::fs::read_to_string(p.join("online")).is_ok_and(|v| v.trim() == "1") {
                return Some(true);
            }
        }
    }
    seen.then_some(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_lock_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("a.lock");
        let first = try_lock(&p).unwrap();
        assert!(first.is_some());
        assert!(try_lock(&p).unwrap().is_none());
        drop(first);
        assert!(try_lock(&p).unwrap().is_some());
    }

    #[test]
    fn lock_refuses_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("t");
        std::fs::write(&target, b"").unwrap();
        let link = tmp.path().join("l");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(try_lock(&link).is_err());
    }
}
