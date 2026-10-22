//! Command line entry point

use clap::{Parser, Subcommand, ValueEnum};
use dell_fan_control::control::Mode;
use dell_fan_control::hw::sysfs::{AlienwareSysfs, read_platform_profile};
use dell_fan_control::hw::{Boost, Fan, FanHardware};
use dell_fan_control::ipc::{self, Request, Response};
use dell_fan_control::{daemon, gui};

#[derive(Parser)]
#[command(
    name = "dell-fan-control",
    version,
    about = "戴尔风扇控制（Dell Fan Control）"
)]
struct Cli {
    /// Use the simulated laptop and its own socket and state files
    #[arg(long, global = true)]
    simulate: bool,
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Open the window (default)
    Gui,
    /// Run the background service (normally started by systemd)
    Daemon {
        #[arg(long)]
        no_tray: bool,
    },
    /// Undo boost and profile changes recorded by this program
    Restore,
    /// Read only hardware discovery, writes nothing
    Probe,
    /// Show the service status
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Switch software control on or off
    Enable { state: OnOff },
    /// Select a mode
    Mode { mode: ModeArg },
    /// Set a custom mode boost value (0-255)
    Custom { fan: FanArg, boost: u8 },
    /// Restore firmware control and stop the service
    Quit,
}

#[derive(Copy, Clone, ValueEnum)]
enum OnOff {
    On,
    Off,
}

#[derive(Copy, Clone, ValueEnum)]
enum ModeArg {
    Balanced,
    Performance,
    Custom,
    Smart,
}

#[derive(Copy, Clone, ValueEnum)]
enum FanArg {
    Cpu,
    Gpu,
}

fn init_log(default: &str) {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default))
        .format_timestamp_secs()
        .init();
}

fn send(req: Request) -> i32 {
    match ipc::request_blocking(&req) {
        Ok(Response::Ok) => 0,
        Ok(Response::Quitting { report }) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&report).unwrap_or_default()
            );
            if report.ok() { 0 } else { 1 }
        }
        Ok(Response::Error { code, message }) => {
            eprintln!("[{code}] {message}");
            1
        }
        Ok(other) => {
            println!("{other:?}");
            0
        }
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

fn main() {
    let cli = Cli::parse();
    ipc::use_simulator(cli.simulate);
    let command = cli.command.unwrap_or(Cmd::Gui);
    if matches!(command, Cmd::Probe | Cmd::Status { .. }) {
        // print-only commands end quietly when piped into head; never done
        // for the service, where a closed client socket must not kill it
        // SAFETY: setting a signal disposition has no memory preconditions
        unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    }
    let code = match command {
        Cmd::Gui => {
            init_log("warn,wgpu=error,iced_wgpu=error,naga=error,cosmic_text=error");
            gui::run(cli.simulate)
        }
        Cmd::Daemon { no_tray } => {
            init_log("info,zbus=warn,ksni=warn,tracing=warn");
            daemon::run(daemon::DaemonOptions {
                simulate: cli.simulate,
                no_tray,
            })
        }
        Cmd::Restore => {
            init_log("info,zbus=warn");
            daemon::restore_command(cli.simulate)
        }
        Cmd::Probe => {
            init_log("warn");
            probe()
        }
        Cmd::Status { json } => match ipc::request_blocking(&Request::Status) {
            Ok(Response::Status(s)) => {
                if json {
                    println!("{}", serde_json::to_string_pretty(&s).unwrap_or_default());
                } else {
                    let c = &s.control;
                    println!("版本 {} · PID {}", s.version, s.pid);
                    println!(
                        "总开关 {} · 模式 {} · 状态 {:?}",
                        if c.enabled { "开" } else { "关" },
                        c.mode.label_zh(),
                        c.state
                    );
                    if let Some(r) = &c.readings {
                        println!("CPU {:?} °C · GPU {:?} °C", r.cpu_temp_c, r.gpu_temp_c);
                    }
                    for f in &c.fans {
                        println!(
                            "{:?}: rpm {:?} 读回 boost {:?} 写入 {:?} 目标 {:?} 响应 {:?}",
                            f.fan, f.rpm, f.readback, f.written, f.target, f.response
                        );
                    }
                    println!(
                        "平台档位 {:?} · ppd {:?}",
                        c.platform_profile, c.ppd_profile
                    );
                    println!("覆盖记录 {:?}", c.marker);
                }
                0
            }
            Ok(other) => {
                eprintln!("{other:?}");
                1
            }
            Err(e) => {
                eprintln!("{e}");
                1
            }
        },
        Cmd::Enable { state } => send(Request::SetEnabled {
            enabled: matches!(state, OnOff::On),
        }),
        Cmd::Mode { mode } => send(Request::SetMode {
            mode: match mode {
                ModeArg::Balanced => Mode::Balanced,
                ModeArg::Performance => Mode::Performance,
                ModeArg::Custom => Mode::Custom,
                ModeArg::Smart => Mode::Smart,
            },
        }),
        Cmd::Custom { fan, boost } => send(Request::SetCustom {
            fan: match fan {
                FanArg::Cpu => Fan::Cpu,
                FanArg::Gpu => Fan::Gpu,
            },
            boost: Boost::new(boost),
        }),
        Cmd::Quit => send(Request::Quit),
    };
    std::process::exit(code);
}

fn probe() -> i32 {
    let p = read_platform_profile();
    println!(
        "platform_profile: {:?} 可选 {:?} 由 alienware-wmi 提供 {}",
        p.current, p.choices, p.provided_by_alienware
    );
    match AlienwareSysfs::discover() {
        Ok(mut hw) => {
            println!("设备：{}", hw.describe());
            println!("写入权限：{:?}", hw.write_access());
            match hw.read() {
                Ok(r) => println!("{}", serde_json::to_string_pretty(&r).unwrap_or_default()),
                Err(e) => println!("读取失败：{e}"),
            }
            0
        }
        Err(e) => {
            println!("{e}");
            1
        }
    }
}
