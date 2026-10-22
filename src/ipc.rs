//! Local IPC between the GUI, the CLI and the background service
//!
//! - Unix socket in `$XDG_RUNTIME_DIR/dell-fan-control/`, directory 0700,
//!   socket 0600, never in /tmp
//! - every connection is checked with SO_PEERCRED and must come from the same
//!   uid as the service
//! - one JSON request per connection, one JSON response, size limited
//! - requests are typed enums with range checked values; there is no way to
//!   pass a path, a command string or anything that reaches a shell

use crate::control::{ControlStatus, Mode, RestoreReport};
use crate::hw::{Boost, Fan};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const MAX_MESSAGE: usize = 256 * 1024;
pub const IO_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Ping,
    Status,
    SetEnabled {
        enabled: bool,
    },
    SetMode {
        mode: Mode,
    },
    SetCustom {
        fan: Fan,
        boost: Boost,
    },
    /// Restore firmware control and stop the service
    Quit,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub version: String,
    pub pid: u32,
    pub control: ControlStatus,
    pub config_error: Option<String>,
    pub tray: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Response {
    Pong { version: String, pid: u32 },
    Status(Box<DaemonStatus>),
    Ok,
    Quitting { report: RestoreReport },
    Error { code: String, message: String },
}

impl Response {
    pub fn error(code: &str, message: impl Into<String>) -> Self {
        Response::Error {
            code: code.into(),
            message: message.into(),
        }
    }
}

pub fn runtime_dir() -> Result<PathBuf, String> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|d| !d.is_empty())
        .ok_or("XDG_RUNTIME_DIR 未设置，拒绝在共享目录中创建控制 socket")?;
    Ok(PathBuf::from(base).join("dell-fan-control"))
}

/// Create the private runtime directory, or verify an existing one
pub fn ensure_private_dir(dir: &Path) -> Result<(), String> {
    match std::fs::create_dir(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("无法创建 {}：{e}", dir.display())),
    }
    // lstat first: a symlink or a foreign directory is refused before any
    // permission change could follow it somewhere else
    let meta = std::fs::symlink_metadata(dir).map_err(|e| e.to_string())?;
    // SAFETY: getuid has no preconditions
    let uid = unsafe { libc::getuid() };
    if !meta.file_type().is_dir() || meta.uid() != uid {
        return Err(format!("{} 不是当前用户自己的目录", dir.display()));
    }
    if meta.mode() & 0o077 != 0 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("无法设置 {} 权限：{e}", dir.display()))?;
    }
    Ok(())
}

static SIMULATE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn gui_socket_name(simulate: bool) -> &'static str {
    if simulate {
        "gui-simulate.sock"
    } else {
        "gui.sock"
    }
}

pub fn gui_lock_name(simulate: bool) -> &'static str {
    if simulate {
        "gui-simulate.lock"
    } else {
        "gui.lock"
    }
}

/// Point clients at the simulator's socket instead of the real service
pub fn use_simulator(on: bool) {
    SIMULATE.store(on, std::sync::atomic::Ordering::SeqCst);
}

pub fn daemon_socket_path() -> Result<PathBuf, String> {
    let name = if SIMULATE.load(std::sync::atomic::Ordering::SeqCst) {
        "daemon-simulate.sock"
    } else {
        "daemon.sock"
    };
    Ok(runtime_dir()?.join(name))
}

/// Blocking client used by the CLI and the tray
pub fn request_blocking(req: &Request) -> Result<Response, String> {
    let path = daemon_socket_path()?;
    let mut stream = std::os::unix::net::UnixStream::connect(&path)
        .map_err(|e| format!("无法连接后台服务（{}）：{e}", path.display()))?;
    stream.set_read_timeout(Some(IO_TIMEOUT)).ok();
    stream.set_write_timeout(Some(IO_TIMEOUT)).ok();
    let mut line = serde_json::to_vec(req).map_err(|e| e.to_string())?;
    line.push(b'\n');
    stream
        .write_all(&line)
        .map_err(|e| format!("发送失败：{e}"))?;
    let mut reader = BufReader::new(stream.take_limited());
    let mut buf = String::new();
    reader
        .read_line(&mut buf)
        .map_err(|e| format!("读取响应失败：{e}"))?;
    serde_json::from_str(&buf).map_err(|e| format!("响应格式错误：{e}"))
}

trait TakeLimited {
    fn take_limited(self) -> std::io::Take<std::os::unix::net::UnixStream>;
}

impl TakeLimited for std::os::unix::net::UnixStream {
    fn take_limited(self) -> std::io::Take<std::os::unix::net::UnixStream> {
        std::io::Read::take(self, MAX_MESSAGE as u64)
    }
}

/// Async client for the GUI
pub async fn request(req: Request) -> Result<Response, String> {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
    let path = daemon_socket_path()?;
    let fut = async move {
        let mut stream = tokio::net::UnixStream::connect(&path)
            .await
            .map_err(|e| format!("无法连接后台服务：{e}"))?;
        let mut line = serde_json::to_vec(&req).map_err(|e| e.to_string())?;
        line.push(b'\n');
        stream
            .write_all(&line)
            .await
            .map_err(|e| format!("发送失败：{e}"))?;
        let mut reader = tokio::io::BufReader::new(stream.take(MAX_MESSAGE as u64));
        let mut buf = String::new();
        reader
            .read_line(&mut buf)
            .await
            .map_err(|e| format!("读取响应失败：{e}"))?;
        serde_json::from_str::<Response>(&buf).map_err(|e| format!("响应格式错误：{e}"))
    };
    tokio::time::timeout(IO_TIMEOUT * 2, fut)
        .await
        .map_err(|_| "后台服务响应超时".to_string())?
}

/// Parse one request line, rejecting anything unexpected
pub fn parse_request(line: &str) -> Result<Request, Response> {
    if line.len() > MAX_MESSAGE {
        return Err(Response::error("IPC_TOO_LARGE", "请求过大"));
    }
    let bad = |m: String| Response::error("IPC_BAD_REQUEST", m);
    let raw: serde_json::Value =
        serde_json::from_str(line.trim()).map_err(|e| bad(format!("无法解析请求：{e}")))?;
    let req: Request =
        serde_json::from_value(raw.clone()).map_err(|e| bad(format!("无法解析请求：{e}")))?;
    // serde ignores unknown fields on unit variants of tagged enums, so the
    // key set is compared with the canonical form of the parsed request
    let canonical = serde_json::to_value(&req).map_err(|e| bad(e.to_string()))?;
    let keys = |v: &serde_json::Value| {
        v.as_object()
            .map(|o| o.keys().cloned().collect::<std::collections::BTreeSet<_>>())
    };
    if keys(&raw) != keys(&canonical) {
        return Err(bad("请求包含未知字段".into()));
    }
    Ok(req)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_typed_and_strict() {
        assert_eq!(
            parse_request(r#"{"cmd":"set_custom","fan":"gpu","boost":200}"#).unwrap(),
            Request::SetCustom {
                fan: Fan::Gpu,
                boost: Boost::new(200)
            }
        );
        // out of range boost
        assert!(parse_request(r#"{"cmd":"set_custom","fan":"gpu","boost":256}"#).is_err());
        assert!(parse_request(r#"{"cmd":"set_custom","fan":"gpu","boost":-1}"#).is_err());
        // unknown fan, unknown command, extra fields
        assert!(parse_request(r#"{"cmd":"set_custom","fan":"fan3","boost":1}"#).is_err());
        assert!(parse_request(r#"{"cmd":"exec","line":"rm -rf /"}"#).is_err());
        assert!(parse_request(r#"{"cmd":"ping","path":"/etc/shadow"}"#).is_err());
        assert!(parse_request(r#"{"cmd":"set_mode","mode":"turbo"}"#).is_err());
        assert_eq!(
            parse_request(r#"{"cmd":"set_mode","mode":"smart"}"#).unwrap(),
            Request::SetMode { mode: Mode::Smart }
        );
    }

    #[test]
    fn private_dir_is_created_with_0700() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("x");
        ensure_private_dir(&d).unwrap();
        let mode = std::fs::metadata(&d).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn a_shared_directory_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("link");
        std::os::unix::fs::symlink("/tmp", &d).unwrap();
        assert!(ensure_private_dir(&d).is_err());
    }
}
