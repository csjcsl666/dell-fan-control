//! iced user interface
//!
//! The GUI holds no control logic. It polls the background service once a
//! second and sends typed requests; closing the window only ends this
//! process. A second launch hands over to the running window through a
//! private socket and exits

pub mod style;

use crate::control::smart::{FanDriver, OutputPlan, Phase, SensorState, SourceDecision};
use crate::control::{ControlState, FanStatus, Mode, Response as FanResponse};
use crate::hw::{Boost, Fan, WriteAccess};
use crate::ipc::{self, DaemonStatus, Request, Response};
use crate::state::write_json_atomic;
use iced::futures::SinkExt;
use iced::widget::{
    Column, Row, button, column, container, row, scrollable, slider, space, text, toggler,
};
use iced::{Alignment, Color, Element, Font, Length, Subscription, Task, Theme, window};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use style::{Appearance, Banner};

pub const APP_ID: &str = "dell-fan-control";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GuiPrefs {
    pub appearance: Appearance,
    pub show_details: bool,
}

fn prefs_path() -> std::path::PathBuf {
    crate::config::default_config_dir().join("gui.json")
}

fn load_prefs() -> GuiPrefs {
    std::fs::read(prefs_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// First installed CJK family from a list of common choices, so Chinese text
/// uses a real Chinese face instead of whatever fallback comes first
fn pick_cjk_font() -> Option<&'static str> {
    const CANDIDATES: [&str; 6] = [
        "Noto Sans CJK SC",
        "Source Han Sans SC",
        "Sarasa UI SC",
        "Noto Sans SC",
        "WenQuanYi Micro Hei",
        "Microsoft YaHei",
    ];
    for c in CANDIDATES {
        let out = std::process::Command::new("fc-list")
            .arg(format!("{c}:style=Regular"))
            .arg("family")
            .output()
            .ok()?;
        if !out.stdout.is_empty() {
            return Some(c);
        }
    }
    None
}

pub fn run(simulate: bool) -> i32 {
    ipc::use_simulator(simulate);
    // single instance: if a window already exists, ask it to come forward
    let dir = match ipc::runtime_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    if let Err(e) = ipc::ensure_private_dir(&dir) {
        eprintln!("{e}");
        return 1;
    }
    let lock = match crate::sys::try_lock(&dir.join(ipc::gui_lock_name(simulate))) {
        Ok(Some(l)) => l,
        Ok(None) => {
            if let Ok(mut s) =
                std::os::unix::net::UnixStream::connect(dir.join(ipc::gui_socket_name(simulate)))
            {
                use std::io::Write;
                let _ = s.write_all(b"show\n");
                println!("界面已经打开，已请求切换到该窗口");
            }
            return 0;
        }
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    // leaked on purpose: the lock must live as long as the process
    std::mem::forget(lock);

    let prefs = load_prefs();
    let transparent = prefs.appearance.wants_transparent_surface();
    let font = pick_cjk_font();
    let mut settings = iced::Settings {
        id: Some(APP_ID.into()),
        ..Default::default()
    };
    if let Some(f) = font {
        settings.default_font = Font::with_name(f);
    }
    let mut win = window::Settings {
        size: iced::Size::new(760.0, 860.0),
        min_size: Some(iced::Size::new(560.0, 520.0)),
        transparent,
        icon: window::icon::from_rgba(crate::icon::rgba(128), 128, 128).ok(),
        exit_on_close_request: true,
        ..Default::default()
    };
    win.platform_specific.application_id = APP_ID.into();

    let result = iced::application(
        move || App::new(prefs.clone(), transparent, simulate),
        App::update,
        App::view,
    )
    .title(App::title)
    .subscription(App::subscription)
    .theme(App::theme)
    .style(App::style)
    .settings(settings)
    .window(win)
    .run();
    let _ = std::fs::remove_file(dir.join(ipc::gui_socket_name(simulate)));
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("界面运行失败：{e}");
            1
        }
    }
}

#[derive(Debug, Clone)]
pub enum Message {
    Poll,
    Status(Result<Box<DaemonStatus>, String>),
    Done(Result<(), String>),
    SetEnabled(bool),
    SetMode(Mode),
    SliderMoved(Fan, u8),
    SliderReleased(Fan),
    Autostart(Option<bool>),
    SetAutostart(bool),
    StartService,
    Quit,
    QuitDone(Result<String, String>),
    Appearance(Appearance),
    SystemTheme(iced::theme::Mode),
    ToggleDetails,
    Raise,
    ExitNow,
}

pub struct App {
    status: Option<DaemonStatus>,
    connection_error: Option<String>,
    action_error: Option<String>,
    notice: Option<String>,
    sliders: [u8; 2],
    dragging: [bool; 2],
    autostart: Option<bool>,
    prefs: GuiPrefs,
    transparent_surface: bool,
    dark: bool,
    simulate: bool,
    tried_start: bool,
    quitting: bool,
}

impl App {
    fn new(prefs: GuiPrefs, transparent_surface: bool, simulate: bool) -> (Self, Task<Message>) {
        let app = App {
            status: None,
            connection_error: None,
            action_error: None,
            notice: None,
            sliders: [0, 0],
            dragging: [false, false],
            autostart: None,
            prefs,
            transparent_surface,
            dark: true,
            simulate,
            tried_start: false,
            quitting: false,
        };
        let tasks = Task::batch([
            Task::done(Message::Poll),
            Task::perform(
                async {
                    tokio::task::spawn_blocking(crate::sys::autostart_state)
                        .await
                        .ok()
                        .flatten()
                },
                Message::Autostart,
            ),
            iced::system::theme().map(Message::SystemTheme),
        ]);
        (app, tasks)
    }

    fn title(&self) -> String {
        if self.simulate {
            "戴尔风扇控制（模拟）".into()
        } else {
            "戴尔风扇控制（Dell Fan Control）".into()
        }
    }

    fn theme(&self) -> Theme {
        style::theme(self.dark)
    }

    fn style(&self, theme: &Theme) -> iced::theme::Style {
        let p = theme.palette();
        iced::theme::Style {
            background_color: Color {
                a: style::window_alpha(self.transparent_surface, self.prefs.appearance),
                ..p.background
            },
            text_color: p.text,
        }
    }

    fn save_prefs(&self) {
        if let Err(e) = write_json_atomic(&prefs_path(), &self.prefs) {
            log::warn!("保存界面设置失败：{e}");
        }
    }

    fn send(req: Request) -> Task<Message> {
        Task::perform(
            async move {
                match ipc::request(req).await? {
                    Response::Ok => Ok(()),
                    Response::Error { code, message } => Err(format!("[{code}] {message}")),
                    other => Err(format!("意外响应：{other:?}")),
                }
            },
            Message::Done,
        )
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Poll => {
                return Task::perform(
                    async {
                        match ipc::request(Request::Status).await? {
                            Response::Status(s) => Ok(s),
                            Response::Error { code, message } => Err(format!("[{code}] {message}")),
                            other => Err(format!("意外响应：{other:?}")),
                        }
                    },
                    Message::Status,
                );
            }
            Message::Status(Ok(s)) => {
                self.connection_error = None;
                for fan in Fan::ALL {
                    if !self.dragging[fan.index()] {
                        self.sliders[fan.index()] = s.control.custom[fan.index()].get();
                    }
                }
                self.status = Some(*s);
            }
            Message::Status(Err(e)) => {
                if self.quitting {
                    return iced::exit();
                }
                self.status = None;
                self.connection_error = Some(e);
                if !self.tried_start && !self.simulate {
                    self.tried_start = true;
                    return Task::perform(
                        async {
                            tokio::task::spawn_blocking(crate::sys::start_service)
                                .await
                                .unwrap_or_else(|e| Err(e.to_string()))
                        },
                        Message::Done,
                    );
                }
            }
            Message::Done(r) => {
                self.action_error = r.err();
                return Task::done(Message::Poll);
            }
            Message::SetEnabled(on) => return Self::send(Request::SetEnabled { enabled: on }),
            Message::SetMode(m) => return Self::send(Request::SetMode { mode: m }),
            Message::SliderMoved(fan, v) => {
                self.dragging[fan.index()] = true;
                self.sliders[fan.index()] = v;
            }
            Message::SliderReleased(fan) => {
                self.dragging[fan.index()] = false;
                return Self::send(Request::SetCustom {
                    fan,
                    boost: Boost::new(self.sliders[fan.index()]),
                });
            }
            Message::Autostart(a) => self.autostart = a,
            Message::SetAutostart(on) => {
                return Task::perform(
                    async move {
                        tokio::task::spawn_blocking(move || {
                            crate::sys::set_autostart(on)?;
                            Ok::<_, String>(crate::sys::autostart_state())
                        })
                        .await
                        .map_err(|e| e.to_string())?
                    },
                    |r| match r {
                        Ok(a) => Message::Autostart(a),
                        Err(e) => Message::Done(Err(format!("设置登录自启失败：{e}"))),
                    },
                );
            }
            Message::StartService => {
                return Task::perform(
                    async {
                        tokio::task::spawn_blocking(crate::sys::start_service)
                            .await
                            .unwrap_or_else(|e| Err(e.to_string()))
                    },
                    Message::Done,
                );
            }
            Message::Quit => {
                self.quitting = true;
                return Task::perform(
                    async {
                        match ipc::request(Request::Quit).await? {
                            Response::Quitting { report } => Ok(if report.ok() {
                                "已恢复固件控制".to_string()
                            } else {
                                format!("恢复未完全成功：{}", report.errors.join("；"))
                            }),
                            Response::Error { code, message } => Err(format!("[{code}] {message}")),
                            other => Err(format!("意外响应：{other:?}")),
                        }
                    },
                    Message::QuitDone,
                );
            }
            Message::QuitDone(Ok(_)) => return iced::exit(),
            Message::QuitDone(Err(e)) => {
                self.quitting = false;
                self.action_error = Some(format!("退出失败：{e}"));
            }
            Message::Appearance(a) => {
                self.prefs.appearance = a;
                self.save_prefs();
                let needs = a.wants_transparent_surface();
                self.notice = (needs != self.transparent_surface)
                    .then(|| "窗口透明度的底层设置将在下次打开界面时生效".to_string());
            }
            Message::SystemTheme(m) => self.dark = style::is_dark(m),
            Message::ToggleDetails => {
                self.prefs.show_details = !self.prefs.show_details;
                self.save_prefs();
            }
            Message::ExitNow => return iced::exit(),
            Message::Raise => {
                return window::latest().and_then(window::gain_focus);
            }
        }
        Task::none()
    }

    fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            iced::time::every(Duration::from_secs(1)).map(|_| Message::Poll),
            iced::system::theme_changes().map(Message::SystemTheme),
            Subscription::run_with(self.simulate, |sim| raise_listener(*sim)),
        ])
    }

    fn alpha(&self) -> f32 {
        style::window_alpha(self.transparent_surface, self.prefs.appearance)
    }

    fn card<'a>(&self, content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
        let a = self.alpha();
        container(content)
            .padding(16)
            .width(Length::Fill)
            .style(move |t| style::card(t, a))
            .into()
    }

    fn view(&self) -> Element<'_, Message> {
        let mut page = Column::new().spacing(14).padding(20).width(Length::Fill);
        page = page.push(self.header());
        for b in self.banners() {
            page = page.push(b);
        }
        if let Some(s) = &self.status {
            page = page.push(self.metrics(s));
            page = page.push(self.modes(s));
            page = page.push(self.mode_panel(s));
            if self.prefs.show_details {
                page = page.push(self.events(s));
            }
        }
        page = page.push(self.settings());
        scrollable(page).height(Length::Fill).into()
    }

    fn header(&self) -> Element<'_, Message> {
        let enabled = self.status.as_ref().is_some_and(|s| s.control.enabled);
        let title = column![
            text("戴尔风扇控制").size(26),
            text(if self.simulate {
                "Dell Fan Control · 模拟数据，不会控制真实风扇"
            } else {
                "Dell Fan Control · alienware-wmi"
            })
            .size(13)
            .style(|t: &Theme| text::Style {
                color: Some(style::muted(t))
            }),
        ]
        .spacing(2);
        let switch = toggler(enabled)
            .label("启用风扇控制")
            .text_size(17)
            .size(26)
            .on_toggle_maybe(self.status.is_some().then_some(Message::SetEnabled));
        row![title, space().width(Length::Fill), switch]
            .align_y(Alignment::Center)
            .into()
    }

    fn banners(&self) -> Vec<Element<'_, Message>> {
        let mut out = Vec::new();
        let banner = |kind: Banner,
                      msg: String,
                      action: Option<(&'static str, Message)>|
         -> Element<'_, Message> {
            let mut r = Row::new()
                .spacing(12)
                .align_y(Alignment::Center)
                .push(text(msg).size(14).width(Length::Fill));
            if let Some((label, m)) = action {
                r = r.push(button(text(label).size(13)).on_press(m));
            }
            container(r)
                .padding(12)
                .width(Length::Fill)
                .style(move |t| style::banner(t, kind))
                .into()
        };
        if let Some(e) = &self.connection_error {
            out.push(banner(
                Banner::Error,
                format!("后台服务未连接：{e}"),
                (!self.simulate).then_some(("启动后台服务", Message::StartService)),
            ));
        }
        if let Some(e) = &self.action_error {
            out.push(banner(Banner::Error, e.clone(), None));
        }
        if let Some(n) = &self.notice {
            out.push(banner(Banner::Info, n.clone(), None));
        }
        if let Some(s) = &self.status {
            let c = &s.control;
            let (kind, msg) = match &c.state {
                ControlState::Off => (
                    Banner::Info,
                    "软件控制已关闭，风扇完全由 Dell 固件控制".to_string(),
                ),
                ControlState::Restoring => (Banner::Warn, "正在撤销软件设置并交还固件控制".into()),
                ControlState::Firmware => (
                    Banner::Ok,
                    format!(
                        "{}模式：风扇由固件策略控制，本软件不写入 boost",
                        c.mode.label_zh()
                    ),
                ),
                ControlState::Controlling => {
                    (Banner::Ok, format!("{}模式：软件控制中", c.mode.label_zh()))
                }
                ControlState::Fallback(r) => (Banner::Warn, format!("已临时交还固件：{r}")),
                ControlState::NoPermission(r) => (Banner::Error, r.clone()),
                ControlState::Fault(r) => (
                    Banner::Error,
                    format!("控制已停止：{r}。请关闭后重新打开总开关"),
                ),
            };
            out.push(banner(kind, msg, None));
            if let Some(e) = &c.read_error {
                out.push(banner(Banner::Error, format!("读取传感器失败：{e}"), None));
            }
            if c.guard.engaged {
                out.push(banner(
                    Banner::Error,
                    format!(
                        "临界温度保护已触发：{}，两个风扇使用已验证的最高 boost",
                        c.guard.reason.clone().unwrap_or_default()
                    ),
                    None,
                ));
            }
            if let Some(e) = &s.config_error {
                out.push(banner(Banner::Warn, e.clone(), None));
            }
            if c.write_access == WriteAccess::PermissionDenied && c.state != ControlState::Off {
                out.push(banner(
                    Banner::Warn,
                    "当前用户不能写入 fan*_boost，均衡与性能模式可用，自定义与智能模式需要先安装 udev 规则".into(),
                    None,
                ));
            }
            for f in &c.fans {
                if let Some(b) = &f.blocked {
                    out.push(banner(Banner::Warn, format!("{}：{b}", fan_name(f)), None));
                }
                if f.response == FanResponse::NoResponse {
                    out.push(banner(
                        Banner::Warn,
                        format!("{}：boost 已写入但转速没有明显变化", fan_name(f)),
                        None,
                    ));
                }
            }
        }
        out
    }

    fn metrics(&self, s: &DaemonStatus) -> Element<'_, Message> {
        let c = &s.control;
        let r = c.readings.as_ref();
        let tile = |label: &'static str,
                    temp: Option<f32>,
                    fan: &FanStatus,
                    extra: String|
         -> Element<'_, Message> {
            let temp_txt = temp.map(|t| format!("{t:.0}")).unwrap_or("--".into());
            let rpm = fan.rpm.map(|v| v.to_string()).unwrap_or("--".into());
            let max = fan.max_rpm.map(|v| format!(" / {v}")).unwrap_or_default();
            let boost = fan
                .readback
                .map(|b| format!("Boost {} / 255", b.get()))
                .unwrap_or("Boost --".into());
            self.card(
                column![
                    text(label).size(14).style(|t: &Theme| text::Style {
                        color: Some(style::muted(t))
                    }),
                    row![text(temp_txt).size(44), text("°C").size(18)]
                        .align_y(Alignment::End)
                        .spacing(4),
                    text(format!("风扇 {rpm}{max} RPM")).size(15),
                    text(boost).size(13).style(|t: &Theme| text::Style {
                        color: Some(style::muted(t))
                    }),
                    text(extra).size(13).style(|t: &Theme| text::Style {
                        color: Some(style::muted(t))
                    }),
                ]
                .spacing(4),
            )
        };
        let cpu_extra = format!(
            "负载 {}",
            c.cpu_load_pct
                .map(|l| format!("{l:.0}%"))
                .unwrap_or("--".into())
        );
        let gpu_extra = match &c.gpu {
            Some(g) => format!(
                "功耗 {} · 负载 {}{}",
                g.power_w
                    .map(|w| format!("{w:.0} W"))
                    .unwrap_or("--".into()),
                g.util_pct
                    .map(|u| format!("{u:.0}%"))
                    .unwrap_or("--".into()),
                g.pstate
                    .as_ref()
                    .map(|p| format!(" · {p}"))
                    .unwrap_or_default()
            ),
            None => "功耗不可用（NVML）".into(),
        };
        let profile = format!(
            "平台档位 {} · 电源管理 {} · {}",
            c.platform_profile.clone().unwrap_or("--".into()),
            c.ppd_profile.clone().unwrap_or("--".into()),
            match c.on_ac {
                Some(true) => "交流电源",
                Some(false) => "电池",
                None => "电源未知",
            }
        );
        column![
            row![
                tile("CPU", r.and_then(|r| r.cpu_temp_c), &c.fans[0], cpu_extra),
                tile("GPU", r.and_then(|r| r.gpu_temp_c), &c.fans[1], gpu_extra),
            ]
            .spacing(14),
            text(profile).size(13).style(|t: &Theme| text::Style {
                color: Some(style::muted(t))
            }),
        ]
        .spacing(8)
        .into()
    }

    fn modes(&self, s: &DaemonStatus) -> Element<'_, Message> {
        let c = &s.control;
        let mut r = Row::new().spacing(8);
        for m in Mode::ALL {
            let selected = c.mode == m;
            r = r.push(
                button(container(text(m.label_zh()).size(15)).center_x(Length::Fill))
                    .padding([10, 8])
                    .width(Length::Fill)
                    .style(move |t, st| style::segment(t, st, selected))
                    .on_press(Message::SetMode(m)),
            );
        }
        self.card(column![text("散热模式").size(16), r].spacing(10))
    }

    fn mode_panel(&self, s: &DaemonStatus) -> Element<'_, Message> {
        let c = &s.control;
        let muted = |t: &Theme| text::Style {
            color: Some(style::muted(t)),
        };
        let body: Element<'_, Message> = match c.mode {
            Mode::Balanced => column![
                text("均衡").size(16),
                text("通过 power-profiles-daemon 选择 balanced 档位，风扇完全由 Dell 固件的均衡策略控制，本软件不写入 boost。").size(14).style(muted),
            ]
            .spacing(6)
            .into(),
            Mode::Performance => column![
                text("性能").size(16),
                text("选择 performance 档位。在 G 系列笔记本上内核会同时开启 G-Mode，它会改变风扇策略，也会放宽 CPU 与 GPU 的功耗限制，不只是提高风扇转速。").size(14).style(muted),
                text(format!(
                    "当前平台档位：{}{}",
                    c.platform_profile.clone().unwrap_or("--".into()),
                    if c.platform_profile.as_deref() == Some("performance") { "（G-Mode 已由内核开启）" } else { "" }
                ))
                .size(14),
            ]
            .spacing(6)
            .into(),
            Mode::Custom => {
                let mut col = column![
                    text("自定义").size(16),
                    text("Boost 是在固件基准转速之上附加的提升量，0 表示不附加，交给固件。本机实测 160 已让两个风扇超过驱动报告的最高转速，软件只开放 0 到 160。").size(13).style(muted),
                ]
                .spacing(10);
                for fan in Fan::ALL {
                    let f = &c.fans[fan.index()];
                    let v = self.sliders[fan.index()];
                    col = col.push(
                        column![
                            row![
                                text(fan.label_zh()).size(15),
                                space().width(Length::Fill),
                                text(format!(
                                    "Boost {v} / 160 · 读回 {} · {} RPM",
                                    f.readback.map(|b| b.get().to_string()).unwrap_or("--".into()),
                                    f.rpm.map(|r| r.to_string()).unwrap_or("--".into())
                                ))
                                .size(13),
                            ],
                            slider(0u8..=crate::hw::MAX_VERIFIED_BOOST, v, move |x| Message::SliderMoved(fan, x))
                                .on_release(Message::SliderReleased(fan)),
                        ]
                        .spacing(6),
                    );
                }
                col.into()
            }
            Mode::Smart => self.smart_panel(s),
        };
        self.card(body)
    }

    fn smart_panel(&self, s: &DaemonStatus) -> Element<'_, Message> {
        let c = &s.control;
        let muted = |t: &Theme| text::Style {
            color: Some(style::muted(t)),
        };
        let mut col = column![
            row![
                text("智能").size(16),
                space().width(Length::Fill),
                button(
                    text(if self.prefs.show_details {
                        "收起算法细节"
                    } else {
                        "显示算法细节"
                    })
                    .size(13)
                )
                .on_press(Message::ToggleDetails),
            ]
            .align_y(Alignment::Center),
            text("双时间尺度 EMA、升降双阈值、降速保持与非对称步长，临界温度保护独立于曲线。")
                .size(13)
                .style(muted),
        ]
        .spacing(8);
        let Some(d) = &c.smart else {
            return col.push(text("启用风扇控制后开始计算").size(14)).into();
        };
        for (name, sd, fan) in [("CPU", &d.cpu, &c.fans[0]), ("GPU", &d.gpu, &c.fans[1])] {
            let fan_line = format!(
                "{name} 风扇：目标 {} · 写入 {} · 读回 {} · {} RPM · {}{}",
                fan.target
                    .map(|b| b.get().to_string())
                    .unwrap_or("--".into()),
                fan.written
                    .map(|b| b.get().to_string())
                    .unwrap_or("--".into()),
                fan.readback
                    .map(|b| b.get().to_string())
                    .unwrap_or("--".into()),
                fan.rpm.map(|b| b.to_string()).unwrap_or("--".into()),
                plan_label(fan.plan.as_ref()),
                d.fans
                    .as_ref()
                    .map(|f| driver_label(f[if name == "CPU" { 0 } else { 1 }].driver))
                    .unwrap_or_default(),
            );
            col = col.push(text(format!("{name}：{}", source_summary(sd))).size(14));
            col = col.push(text(fan_line).size(13).style(muted));
            if self.prefs.show_details {
                col = col.push(
                    text(source_details(sd))
                        .size(12)
                        .font(Font::MONOSPACE)
                        .style(muted),
                );
            }
        }
        if let Some(f) = &d.fallback {
            col = col.push(text(format!("回退：{f}")).size(13));
        }
        col.into()
    }

    fn events(&self, s: &DaemonStatus) -> Element<'_, Message> {
        let mut col = column![text("最近事件").size(16)].spacing(4);
        for e in s.control.events.iter().rev().take(10) {
            col = col.push(text(format!("{} {}", fmt_time(e.unix_s), e.text)).size(12));
        }
        self.card(col)
    }

    fn settings(&self) -> Element<'_, Message> {
        let mut looks = Row::new().spacing(8);
        for a in Appearance::ALL {
            let selected = self.prefs.appearance == a;
            looks = looks.push(
                button(container(text(a.label()).size(14)).center_x(Length::Fill))
                    .padding([8, 6])
                    .width(Length::Fill)
                    .style(move |t, st| style::segment(t, st, selected))
                    .on_press(Message::Appearance(a)),
            );
        }
        let autostart = match self.autostart {
            Some(on) => toggler(on)
                .label("登录时自动启动后台服务")
                .text_size(14)
                .on_toggle(Message::SetAutostart),
            None => toggler(false)
                .label("登录时自动启动（服务未安装）")
                .text_size(14),
        };
        let quit = button(text("彻底退出（撤销 boost 并恢复固件控制）").size(14))
            .padding([10, 14])
            .style(style::danger_button)
            .on_press_maybe((self.status.is_some() && !self.quitting).then_some(Message::Quit));
        self.card(
            column![
                text("设置").size(16),
                autostart,
                text("外观").size(14),
                looks,
                row![
                    text("关闭窗口不会停止后台控制")
                        .size(13)
                        .style(|t: &Theme| text::Style {
                            color: Some(style::muted(t))
                        }),
                    space().width(Length::Fill),
                    quit
                ]
                .align_y(Alignment::Center),
            ]
            .spacing(10),
        )
    }
}

fn fan_name(f: &FanStatus) -> &'static str {
    f.fan.map(|f| f.label_zh()).unwrap_or("风扇")
}

fn plan_label(p: Option<&OutputPlan>) -> String {
    match p {
        None => "不控制".into(),
        Some(OutputPlan::Unchanged) => "保持".into(),
        Some(OutputPlan::BelowMinStep) => "变化小于最小步长".into(),
        Some(OutputPlan::WaitingInterval) => "等待写入间隔".into(),
        Some(OutputPlan::Write(b)) => format!("写入 {}", b.get()),
    }
}

fn driver_label(d: FanDriver) -> &'static str {
    match d {
        FanDriver::Own => " · 来自本热源",
        FanDriver::Coupled => " · 来自另一热源",
        FanDriver::Guard => " · 临界保护",
        FanDriver::NoiseCap => " · 噪声上限",
    }
}

fn phase_label(p: Phase) -> &'static str {
    match p {
        Phase::Rising => "升温，已提高",
        Phase::Holding => "迟滞区间内保持",
        Phase::WaitingToLower => "等待降速",
        Phase::Lowering => "降速",
    }
}

fn source_summary(d: &SourceDecision) -> String {
    let sensor = match d.sensor {
        SensorState::Ok => String::new(),
        SensorState::Holding(n) => format!(" · 传感器无效，沿用上次值（{n}）"),
        SensorState::Lost => " · 传感器失效".into(),
    };
    let wait = if d.phase == Phase::WaitingToLower {
        format!("，剩余 {:.0} 秒", d.hold_remaining_s)
    } else {
        String::new()
    };
    format!(
        "{} → 需求 {}（{}{}）{}",
        d.raw_c.map(|t| format!("{t:.1} °C")).unwrap_or("--".into()),
        d.demand.get(),
        phase_label(d.phase),
        wait,
        sensor
    )
}

fn source_details(d: &SourceDecision) -> String {
    let f = |v: Option<f32>| v.map(|x| format!("{x:.1}")).unwrap_or("--".into());
    format!(
        "原始 {}  快 {}  慢 {}  斜率 {:+.2}°C/s  超前 {:.1}\n升温判定 {} → {}  降温判定 {} → {}  曲线段 {}",
        f(d.raw_c),
        f(d.fast_c),
        f(d.slow_c),
        d.slope_c_per_s,
        d.lead_c,
        f(d.rise_temp_c),
        d.rise_demand.get(),
        f(d.fall_temp_c),
        d.fall_demand.get(),
        d.curve_segment
    )
}

fn fmt_time(unix: u64) -> String {
    // local time without extra dependencies
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let t = unix as libc::time_t;
    // SAFETY: both pointers are valid for the duration of the call
    unsafe { libc::localtime_r(&t, &mut tm) };
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

/// Listens on the GUI socket so a second launch can bring this window forward
fn raise_listener(simulate: bool) -> impl iced::futures::Stream<Item = Message> {
    iced::stream::channel(
        4,
        async move |mut output: iced::futures::channel::mpsc::Sender<Message>| {
            let Ok(dir) = ipc::runtime_dir() else {
                return;
            };
            let path = dir.join(ipc::gui_socket_name(simulate));
            let _ = std::fs::remove_file(&path);
            let Ok(listener) = tokio::net::UnixListener::bind(&path) else {
                return;
            };
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
            }
            // SAFETY: geteuid has no preconditions
            let uid = unsafe { libc::geteuid() };
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    continue;
                };
                if !stream.peer_cred().is_ok_and(|c| c.uid() == uid) {
                    continue;
                }
                use tokio::io::AsyncBufReadExt;
                let mut line = String::new();
                let mut r = tokio::io::BufReader::new(tokio::io::AsyncReadExt::take(stream, 64));
                let _ = tokio::time::timeout(Duration::from_secs(1), r.read_line(&mut line)).await;
                let msg = if line.trim() == "quit" {
                    Message::ExitNow
                } else {
                    Message::Raise
                };
                let _ = output.send(msg).await;
            }
        },
    )
}
