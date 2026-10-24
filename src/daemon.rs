//! Background service
//!
//! Threads:
//! - control thread: owns the [`Controller`], ticks it, answers commands,
//!   pings the systemd watchdog. If this thread stops, the watchdog stops
//!   too and systemd kills the service and runs the restore
//! - tokio runtime: IPC server, tray, signal handling
//!
//! Exit paths and what restores the fans:
//! - quit from GUI or tray, SIGTERM, SIGINT: the control thread restores,
//!   then the process exits 0
//! - panic, SIGKILL, hang: systemd runs `ExecStopPost=dell-fan-control
//!   restore`, which undoes whatever the marker file records
//! - next start: leftovers in the marker are restored before anything else

use crate::config::{SettingsStore, default_config_dir};
use crate::control::{Controller, Extras};
use crate::hw::cpu::CpuLoad;
use crate::hw::gpu::GpuReader;
use crate::hw::mock::{SharedLaptop, SimulatedLaptop};
use crate::hw::sysfs::{self, AlienwareSysfs};
use crate::hw::{FanHardware, Unavailable};
use crate::ipc::{self, DaemonStatus, Request, Response};
use crate::profile::{FakeProfile, PpdBackend, ProfileBackend};
use crate::state::{MarkerStore, default_state_dir};
use crate::sys;
use crate::tray::{FanTray, TraySummary};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

pub enum Command {
    Request(Request, Option<tokio::sync::oneshot::Sender<Response>>),
    Shutdown,
}

#[derive(Clone, Debug, Default)]
pub struct DaemonOptions {
    pub simulate: bool,
    pub no_tray: bool,
}

type Shared = Arc<Mutex<Option<DaemonStatus>>>;

/// Handle to the simulated laptop for fault injection; never set for real hardware
static SIM_HANDLE: std::sync::OnceLock<SharedLaptop> = std::sync::OnceLock::new();

fn build_backends(opts: &DaemonOptions) -> (Box<dyn FanHardware>, Box<dyn ProfileBackend>) {
    if opts.simulate {
        let laptop = SimulatedLaptop {
            cpu_heat_w: 38.0,
            ..SimulatedLaptop::realtime()
        };
        let shared = SharedLaptop::new(laptop);
        let _ = SIM_HANDLE.set(shared.clone());
        return (Box::new(shared), Box::new(FakeProfile::default()));
    }
    let hw: Box<dyn FanHardware> = match AlienwareSysfs::discover() {
        Ok(h) => {
            log::info!("硬件：{}", h.describe());
            Box::new(h)
        }
        Err(e) => {
            log::error!("没有可用的风扇硬件：{e}");
            Box::new(Unavailable {
                reason: e.to_string(),
            })
        }
    };
    (hw, Box::new(PpdBackend::new()))
}

/// Directory for the marker file; the simulator uses its own so it can never
/// touch the record of the real hardware
pub fn state_dir(simulate: bool) -> std::path::PathBuf {
    let d = default_state_dir();
    if simulate { d.join("simulate") } else { d }
}

fn config_dir(simulate: bool) -> std::path::PathBuf {
    let d = default_config_dir();
    if simulate { d.join("simulate") } else { d }
}

pub fn run(opts: DaemonOptions) -> i32 {
    if !opts.simulate {
        sysfs::allow_real_writes();
    }
    let dir = match ipc::runtime_dir() {
        Ok(d) => d,
        Err(e) => {
            log::error!("{e}");
            return 1;
        }
    };
    if let Err(e) = ipc::ensure_private_dir(&dir) {
        log::error!("{e}");
        return 1;
    }
    let lock_name = if opts.simulate {
        "daemon-simulate.lock"
    } else {
        "daemon.lock"
    };
    let _lock = match sys::try_lock(&dir.join(lock_name)) {
        Ok(Some(l)) => l,
        Ok(None) => {
            log::warn!("后台服务已经在运行，本次启动退出");
            return 0;
        }
        Err(e) => {
            log::error!("{e}");
            return 1;
        }
    };

    let store = SettingsStore::new(&config_dir(opts.simulate));
    let (settings, config_error) = store.load();
    if let Some(e) = &config_error {
        log::error!("{e}");
    }
    let (hw, profile) = build_backends(&opts);
    let marker = MarkerStore::open(&state_dir(opts.simulate));
    let mut controller = Controller::new(
        hw,
        profile,
        marker,
        settings.enabled,
        settings.mode,
        settings.custom,
        settings.smart.clone(),
    );
    if let Some(r) = controller.recover_leftovers() {
        log::warn!("启动时恢复上次遗留的覆盖：{r:?}");
    }

    let shared: Shared = Arc::new(Mutex::new(None));
    let (tx, rx) = mpsc::channel::<Command>();
    let tray_summary = Arc::new(Mutex::new(TraySummary::default()));

    let control = {
        let shared = shared.clone();
        let tray_summary = tray_summary.clone();
        let config_error = config_error.clone();
        let simulate = opts.simulate;
        std::thread::Builder::new()
            .name("control".into())
            .spawn(move || {
                control_loop(
                    controller,
                    store,
                    settings,
                    rx,
                    shared,
                    tray_summary,
                    config_error,
                    simulate,
                )
            })
            .expect("spawn control thread")
    };

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            log::error!("无法创建运行时：{e}");
            let _ = tx.send(Command::Shutdown);
            let _ = control.join();
            return 1;
        }
    };

    let socket = if opts.simulate {
        dir.join("daemon-simulate.sock")
    } else {
        dir.join("daemon.sock")
    };
    let code = rt.block_on(async_main(opts, socket, tx, shared, tray_summary, control));
    sys::sd_notify("STOPPING=1");
    code
}

#[allow(clippy::too_many_arguments)]
fn control_loop(
    mut c: Controller,
    store: SettingsStore,
    mut settings: crate::config::Settings,
    rx: mpsc::Receiver<Command>,
    shared: Shared,
    tray: Arc<Mutex<TraySummary>>,
    config_error: Option<String>,
    simulate: bool,
) {
    let start = Instant::now();
    let mut cpu = CpuLoad::default();
    let mut gpu = GpuReader::new();
    let mut gpu_at = Instant::now() - Duration::from_secs(10);
    let mut gpu_last = None;
    let pid = std::process::id();
    let mut last_client = Instant::now() - Duration::from_secs(60);

    let publish = |c: &mut Controller,
                   cpu: &mut CpuLoad,
                   gpu: &mut GpuReader,
                   gpu_last: &mut Option<_>,
                   gpu_at: &mut Instant| {
        // NVML is cheap but not free; sample it every two seconds at most
        if !simulate && gpu_at.elapsed() >= Duration::from_secs(2) {
            *gpu_last = gpu.sample();
            *gpu_at = Instant::now();
        }
        let extras = Extras {
            cpu_load_pct: cpu.sample(),
            gpu: gpu_last.clone(),
            on_ac: sys::on_ac_power(),
            platform_profile: (!simulate)
                .then(|| sysfs::read_platform_profile().current)
                .flatten(),
        };
        let status = c.tick(start.elapsed().as_secs_f64(), extras).clone();
        if let Ok(mut t) = tray.lock() {
            *t = TraySummary::from_status(&status);
        }
        if let Ok(mut s) = shared.lock() {
            *s = Some(DaemonStatus {
                version: env!("CARGO_PKG_VERSION").into(),
                pid,
                control: status,
                config_error: config_error.clone(),
                tray: None,
            });
        }
        sys::sd_notify("WATCHDOG=1");
    };

    publish(&mut c, &mut cpu, &mut gpu, &mut gpu_last, &mut gpu_at);
    sys::sd_notify("READY=1");

    loop {
        let fast = c.enabled() || last_client.elapsed() < Duration::from_secs(5);
        let interval = if fast {
            Duration::from_secs(1)
        } else {
            Duration::from_secs(3)
        };
        match rx.recv_timeout(interval) {
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) | Ok(Command::Shutdown) => {
                let r = c.shutdown();
                log::info!("后台服务停止，恢复结果：{r:?}");
                return;
            }
            Ok(Command::Request(req, reply)) => {
                last_client = Instant::now();
                let mut save = false;
                let resp = match req {
                    Request::Ping => Response::Pong {
                        version: env!("CARGO_PKG_VERSION").into(),
                        pid,
                    },
                    Request::Status => match shared.lock().ok().and_then(|s| s.clone()) {
                        Some(s) => Response::Status(Box::new(s)),
                        None => Response::error("NOT_READY", "状态尚未就绪"),
                    },
                    Request::SetEnabled { enabled } => {
                        c.set_enabled(enabled);
                        settings.enabled = enabled;
                        save = true;
                        Response::Ok
                    }
                    Request::SetMode { mode } => {
                        c.set_mode(mode);
                        settings.mode = mode;
                        save = true;
                        Response::Ok
                    }
                    Request::Simulate { fault, on } => match SIM_HANDLE.get() {
                        None => Response::error("NOT_SIMULATED", "故障注入只在模拟模式下可用"),
                        Some(h) => {
                            use crate::ipc::SimFault;
                            let mut m = h.lock();
                            match fault {
                                SimFault::BoostIgnored => m.faults.boost_ignored = on,
                                SimFault::FirmwareResetsBoost => {
                                    m.faults.firmware_resets_boost = on
                                }
                                SimFault::SensorsMissing => m.faults.sensors_missing = on,
                                SimFault::WriteError => m.faults.write_error = on,
                                SimFault::EventFlood => {}
                            }
                            drop(m);
                            if fault == SimFault::EventFlood {
                                for i in 1..=100 {
                                    c.log("info", format!("模拟事件 {i}"));
                                }
                            }
                            c.log(
                                "warn",
                                format!("模拟故障 {fault:?} {}", if on { "开启" } else { "关闭" }),
                            );
                            Response::Ok
                        }
                    },
                    Request::SetCustom { boost, .. } if boost > crate::hw::Boost::VERIFIED_MAX => {
                        Response::error(
                            "BOOST_ABOVE_VERIFIED",
                            format!(
                                "boost {} 超过已验证的上限 {}",
                                boost.get(),
                                crate::hw::MAX_VERIFIED_BOOST
                            ),
                        )
                    }
                    Request::SetCustom { fan, boost } => {
                        c.set_custom(fan, boost);
                        settings.custom[fan.index()] = boost;
                        save = true;
                        Response::Ok
                    }
                    Request::Quit => {
                        c.log("info", "收到彻底退出请求，正在恢复固件控制");
                        let report = c.shutdown();
                        if let Some(r) = reply {
                            let _ = r.send(Response::Quitting { report });
                        }
                        notify_gui_quit(simulate);
                        log::info!("彻底退出");
                        return;
                    }
                };
                if save && let Err(e) = store.save(&settings) {
                    c.log("error", format!("保存配置失败：{e}"));
                }
                let is_status = matches!(resp, Response::Status(_) | Response::Pong { .. });
                if !is_status {
                    // apply right away so the caller sees the effect
                    publish(&mut c, &mut cpu, &mut gpu, &mut gpu_last, &mut gpu_at);
                }
                if let Some(r) = reply {
                    let _ = r.send(resp);
                }
                continue;
            }
        }
        publish(&mut c, &mut cpu, &mut gpu, &mut gpu_last, &mut gpu_at);
    }
}

async fn async_main(
    opts: DaemonOptions,
    socket: std::path::PathBuf,
    tx: mpsc::Sender<Command>,
    _shared: Shared,
    tray_summary: Arc<Mutex<TraySummary>>,
    control: std::thread::JoinHandle<()>,
) -> i32 {
    // a stale socket from a crashed run: we hold the lock, so it is ours
    let _ = std::fs::remove_file(&socket);
    let listener = match tokio::net::UnixListener::bind(&socket) {
        Ok(l) => l,
        Err(e) => {
            log::error!("无法监听 {}：{e}", socket.display());
            let _ = tx.send(Command::Shutdown);
            let _ = tokio::task::spawn_blocking(move || control.join()).await;
            return 1;
        }
    };
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600));
    }
    log::info!("控制 socket：{}", socket.display());

    // tray, optional: control never depends on it
    let tray_handle = if opts.no_tray {
        None
    } else {
        use ksni::TrayMethods;
        let tray = FanTray {
            summary: tray_summary.lock().map(|s| s.clone()).unwrap_or_default(),
            commands: tx.clone(),
        };
        match tray.assume_sni_available(true).spawn().await {
            Ok(h) => Some(h),
            Err(e) => {
                log::warn!("托盘不可用，继续在后台运行：{e}");
                None
            }
        }
    };
    if let Some(h) = tray_handle.clone() {
        let summary = tray_summary.clone();
        tokio::spawn(async move {
            let mut last = TraySummary::default();
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                let now = summary.lock().map(|s| s.clone()).unwrap_or_default();
                if now != last {
                    last = now.clone();
                    if h.update(move |t| t.summary = now).await.is_none() {
                        break;
                    }
                }
            }
        });
    }

    // the control thread ends on quit or after a panic; watch it
    let (done_tx, mut done_rx) = tokio::sync::oneshot::channel::<bool>();
    tokio::task::spawn_blocking(move || {
        let ok = control.join().is_ok();
        let _ = done_tx.send(ok);
    });

    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
    let mut int = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).ok();
    let mut hup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()).ok();

    // SAFETY: geteuid has no preconditions
    let my_uid = unsafe { libc::geteuid() };
    let code;
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { continue };
                let tx = tx.clone();
                tokio::spawn(async move { handle_client(stream, tx, my_uid).await });
            }
            joined = &mut done_rx => {
                code = match joined {
                    Ok(true) => 0,
                    _ => {
                        log::error!("控制线程异常结束，退出并交给 systemd 恢复");
                        70
                    }
                };
                break;
            }
            _ = async { match term.as_mut() { Some(s) => { s.recv().await; } None => std::future::pending().await } } => {
                log::info!("收到 SIGTERM");
                let _ = tx.send(Command::Shutdown);
            }
            _ = async { match int.as_mut() { Some(s) => { s.recv().await; } None => std::future::pending().await } } => {
                log::info!("收到 SIGINT");
                let _ = tx.send(Command::Shutdown);
            }
            _ = async { match hup.as_mut() { Some(s) => { s.recv().await; } None => std::future::pending().await } } => {
                log::info!("收到 SIGHUP，忽略");
            }
        }
    }
    if let Some(h) = tray_handle {
        h.shutdown().await;
    }
    let _ = std::fs::remove_file(&socket);
    code
}

async fn handle_client(stream: tokio::net::UnixStream, tx: mpsc::Sender<Command>, my_uid: u32) {
    let peer_ok = stream
        .peer_cred()
        .map(|c| c.uid() == my_uid)
        .unwrap_or(false);
    let (read, mut write) = stream.into_split();
    let respond = |r: Response| {
        let mut v = serde_json::to_vec(&r).unwrap_or_default();
        v.push(b'\n');
        v
    };
    if !peer_ok {
        log::warn!("拒绝来自其他用户的连接");
        let _ = write
            .write_all(&respond(Response::error("IPC_FORBIDDEN", "调用者身份不符")))
            .await;
        return;
    }
    let mut reader = tokio::io::BufReader::new(read.take(ipc::MAX_MESSAGE as u64));
    let mut line = String::new();
    let got = tokio::time::timeout(ipc::IO_TIMEOUT, reader.read_line(&mut line)).await;
    let resp = match got {
        Ok(Ok(n)) if n > 0 => match ipc::parse_request(&line) {
            Ok(req) => {
                let (otx, orx) = tokio::sync::oneshot::channel();
                if tx.send(Command::Request(req, Some(otx))).is_err() {
                    Response::error("DAEMON_STOPPING", "后台服务正在退出")
                } else {
                    match tokio::time::timeout(Duration::from_secs(10), orx).await {
                        Ok(Ok(r)) => r,
                        _ => Response::error("DAEMON_TIMEOUT", "控制线程未响应"),
                    }
                }
            }
            Err(r) => r,
        },
        _ => Response::error("IPC_READ", "没有收到完整请求"),
    };
    let _ = write.write_all(&respond(resp)).await;
}

/// `dell-fan-control restore`: undo whatever the marker records. Used by
/// systemd ExecStopPost and by hand. Refuses while a service holds the lock
pub fn restore_command(simulate: bool) -> i32 {
    if !simulate {
        sysfs::allow_real_writes();
    }
    let marker = MarkerStore::open(&state_dir(simulate));
    if marker.get().is_empty() {
        log::info!("没有需要恢复的覆盖记录");
        return 0;
    }
    // hold the service lock while restoring so a new service cannot start
    // writing in the middle of it
    let _guard = match ipc::runtime_dir() {
        Ok(dir) if dir.exists() => {
            let lock = dir.join(if simulate {
                "daemon-simulate.lock"
            } else {
                "daemon.lock"
            });
            match sys::try_lock(&lock) {
                Ok(None) => {
                    log::error!("后台服务仍在运行，请通过界面或 `dell-fan-control quit` 退出");
                    return 2;
                }
                Ok(Some(l)) => Some(l),
                Err(e) => {
                    log::warn!("{e}");
                    None
                }
            }
        }
        _ => None,
    };
    let (hw, profile) = build_backends(&DaemonOptions {
        simulate,
        no_tray: true,
    });
    let mut c = Controller::new(
        hw,
        profile,
        marker,
        false,
        Default::default(),
        Default::default(),
        Default::default(),
    );
    match c.recover_leftovers() {
        Some(r) if r.ok() => {
            log::info!("恢复完成：{r:?}");
            println!("{}", serde_json::to_string(&r).unwrap_or_default());
            0
        }
        Some(r) => {
            log::error!("恢复未完成：{r:?}");
            println!("{}", serde_json::to_string(&r).unwrap_or_default());
            1
        }
        None => 0,
    }
}

/// Ask an open window to close after a quit from the tray or the CLI
fn notify_gui_quit(simulate: bool) {
    use std::io::Write;
    let Ok(dir) = ipc::runtime_dir() else { return };
    if let Ok(mut s) =
        std::os::unix::net::UnixStream::connect(dir.join(ipc::gui_socket_name(simulate)))
    {
        let _ = s.set_write_timeout(Some(Duration::from_millis(500)));
        let _ = s.write_all(b"quit\n");
    }
}
