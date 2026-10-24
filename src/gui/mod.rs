//! iced user interface
//!
//! The GUI holds no control logic. It polls the background service once a
//! second and sends typed requests; closing the window only ends this
//! process. A second launch hands over to the running window through a
//! private socket and exits

pub mod style;

use crate::control::response::ResponseState;
use crate::control::smart::{FanDriver, OutputPlan, Phase, SensorState, SourceDecision};
use crate::control::{AlertLevel, ControlState, ControlStatus, FanStatus, Mode};
use crate::hw::{Boost, Fan, WriteAccess};
use crate::ipc::{self, DaemonStatus, Request, Response};
use crate::state::write_json_atomic;
use iced::font::Weight;
use iced::futures::SinkExt;
use iced::widget::scrollable::{Direction, Scrollbar};
use iced::widget::{
    Column, Row, button, column, container, responsive, row, scrollable, slider, space, text,
    toggler,
};
use iced::{Alignment, Element, Font, Length, Subscription, Task, Theme, window};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use style::{Appearance, Glass, Tone};

pub const APP_ID: &str = "dell-fan-control";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GuiPrefs {
    pub appearance: Appearance,
    pub show_details: bool,
    /// Whether the full event history is expanded
    pub show_events: bool,
    /// Custom and smart panels folded to a one line summary
    pub mode_panel_collapsed: bool,
}

/// The simulator keeps its own preferences so tests never touch the real ones
fn prefs_path(simulate: bool) -> std::path::PathBuf {
    let d = crate::config::default_config_dir();
    if simulate {
        d.join("simulate/gui.json")
    } else {
        d.join("gui.json")
    }
}

fn load_prefs(simulate: bool) -> GuiPrefs {
    std::fs::read(prefs_path(simulate))
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

    let prefs = load_prefs(simulate);
    let transparent = prefs.appearance.wants_transparent_surface();
    let font = pick_cjk_font();
    style::set_family(font);
    let mut settings = iced::Settings {
        id: Some(APP_ID.into()),
        ..Default::default()
    };
    if let Some(f) = font {
        settings.default_font = Font::with_name(f);
    }
    let mut win = window::Settings {
        size: iced::Size::new(760.0, 820.0),
        min_size: Some(iced::Size::new(420.0, 360.0)),
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
    ToggleEvents,
    ToggleModePanel,
    DismissAction,
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
        iced::theme::Style {
            background_color: style::window_background(self.glass()),
            text_color: theme.palette().text,
        }
    }

    fn save_prefs(&self) {
        if let Err(e) = write_json_atomic(&prefs_path(self.simulate), &self.prefs) {
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
            Message::ToggleEvents => {
                self.prefs.show_events = !self.prefs.show_events;
                self.save_prefs();
            }
            Message::DismissAction => self.action_error = None,
            Message::ToggleModePanel => {
                self.prefs.mode_panel_collapsed = !self.prefs.mode_panel_collapsed;
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

    fn glass(&self) -> Glass {
        Glass::new(self.dark, self.transparent_surface, self.prefs.appearance)
    }

    fn card<'a>(&self, content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
        let g = self.glass();
        container(content)
            .padding(18)
            .width(Length::Fill)
            .style(move |_| style::card(g))
            .into()
    }

    fn panel<'a>(&self, content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
        let g = self.glass();
        container(content)
            .padding(16)
            .width(Length::Fill)
            .style(move |_| style::panel(g))
            .into()
    }

    fn view(&self) -> Element<'_, Message> {
        let g = self.glass();
        // the layout follows the real window size, whatever the compositor
        // gives us: tiled, floating, maximised or small
        container(responsive(move |size| self.page(size.width)))
            .width(Length::Fill)
            .height(Length::Fill)
            .style(move |_| style::root(g))
            .into()
    }

    fn page(&self, width: f32) -> Element<'_, Message> {
        let narrow = width < NARROW_WIDTH;
        let pad = if narrow { 14 } else { 22 };
        let mut page = Column::new().spacing(14).padding(pad).width(Length::Fill);
        page = page.push(self.header());
        for n in self.notices() {
            page = page.push(n);
        }
        if let Some(s) = &self.status {
            page = page.push(self.metrics(s, narrow));
            page = page.push(self.modes(s));
            page = page.push(self.mode_panel(s));
            page = page.push(self.events(s));
        }
        page = page.push(self.settings());
        // scrolling stays fully available (wheel, touchpad, keyboard), only the
        // bar is not drawn, so the glass edge on the right stays clean
        scrollable(page)
            .direction(Direction::Vertical(Scrollbar::hidden()))
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn header(&self) -> Element<'_, Message> {
        let enabled = self.status.as_ref().is_some_and(|s| s.control.enabled);
        let title = column![
            text("戴尔风扇控制")
                .size(26)
                .font(style::font(Weight::Semibold)),
            text(if self.simulate {
                "Dell Fan Control · 模拟数据，不会控制真实风扇"
            } else {
                "Dell Fan Control · alienware-wmi"
            })
            .size(13)
            .style(muted),
        ]
        .spacing(2);
        let switch = toggler(enabled)
            .label("启用风扇控制")
            .text_size(16)
            .size(24)
            .style(style::glass_toggler)
            .on_toggle_maybe(self.status.is_some().then_some(Message::SetEnabled));
        row![title, space().width(Length::Fill), switch]
            .align_y(Alignment::Center)
            .into()
    }

    fn notice_bar<'a>(
        &self,
        tone: Tone,
        msg: String,
        action: Option<(&'static str, Message)>,
    ) -> Element<'a, Message> {
        let g = self.glass();
        let mut r = Row::new()
            .spacing(12)
            .align_y(Alignment::Center)
            .push(
                container(space().width(8).height(8))
                    .width(8)
                    .height(8)
                    .style(move |t| style::dot(t, tone)),
            )
            .push(text(msg).size(14).width(Length::Fill));
        if let Some((label, m)) = action {
            r = r.push(
                button(text(label).size(13))
                    .padding([6, 12])
                    .style(style::ghost_button)
                    .on_press(m),
            );
        }
        container(r)
            .padding([10, 14])
            .width(Length::Fill)
            .style(move |t| style::notice(t, g, tone))
            .into()
    }

    /// Conditions that hold right now. History lives in the events card
    fn notices(&self) -> Vec<Element<'_, Message>> {
        let mut out = Vec::new();
        if let Some(e) = &self.connection_error {
            out.push(self.notice_bar(
                Tone::Error,
                format!("后台服务未连接：{e}"),
                (!self.simulate).then_some(("启动后台服务", Message::StartService)),
            ));
        }
        if let Some(e) = &self.action_error {
            out.push(self.notice_bar(
                Tone::Error,
                e.clone(),
                Some(("知道了", Message::DismissAction)),
            ));
        }
        if let Some(n) = &self.notice {
            out.push(self.notice_bar(Tone::Info, n.clone(), None));
        }
        let Some(s) = &self.status else { return out };
        let c = &s.control;
        if c.simulated {
            out.push(self.notice_bar(
                Tone::Info,
                "模拟测试模式：数据、警告与事件都来自模拟笔记本，与真实硬件无关".into(),
                None,
            ));
        }
        let (tone, msg) = match &c.state {
            ControlState::Off => (
                Tone::Neutral,
                "软件控制已关闭，风扇完全由 Dell 固件控制".to_string(),
            ),
            ControlState::Firmware => (
                Tone::Ok,
                format!(
                    "{}模式：风扇由固件策略控制，本软件不写入 boost",
                    c.mode.label_zh()
                ),
            ),
            ControlState::Controlling => {
                (Tone::Ok, format!("{}模式：软件控制中", c.mode.label_zh()))
            }
            // these states come with their own alert below
            _ => (Tone::Neutral, String::new()),
        };
        if !msg.is_empty() {
            out.push(self.notice_bar(tone, msg, None));
        }
        let mut alerts = c.alerts.clone();
        alerts.sort_by_key(|a| std::cmp::Reverse(a.level));
        for a in alerts.into_iter().take(MAX_NOTICES) {
            let tone = match a.level {
                AlertLevel::Error => Tone::Error,
                AlertLevel::Warning => Tone::Warn,
                AlertLevel::Info => Tone::Info,
            };
            out.push(self.notice_bar(tone, a.text, None));
        }
        if c.alerts.len() > MAX_NOTICES {
            out.push(self.notice_bar(
                Tone::Neutral,
                format!(
                    "另有 {} 条提示，见下方最近事件",
                    c.alerts.len() - MAX_NOTICES
                ),
                None,
            ));
        }
        if let Some(e) = &s.config_error {
            out.push(self.notice_bar(Tone::Warn, e.clone(), None));
        }
        if c.write_access == WriteAccess::PermissionDenied && c.state == ControlState::Off {
            out.push(self.notice_bar(
                Tone::Warn,
                "当前用户不能写入 fan*_boost，自定义与智能模式需要先安装 udev 规则".into(),
                None,
            ));
        }
        out
    }

    fn metrics(&self, s: &DaemonStatus, narrow: bool) -> Element<'_, Message> {
        let c = &s.control;
        let r = c.readings.as_ref();
        let tile = |label: &'static str,
                    temp: Option<f32>,
                    fan: &FanStatus,
                    extra: String|
         -> Element<'_, Message> {
            let (chip_tone, chip_text) = fan_condition(c, fan);
            let temp_txt = temp.map(|t| format!("{t:.0}")).unwrap_or("--".into());
            let rpm = fan
                .rpm
                .map(|v| format!("{v} RPM"))
                .unwrap_or("-- RPM".into());
            let boost = fan
                .readback
                .map(|b| format!("Boost {}", b.get()))
                .unwrap_or("Boost --".into());
            let reference = fan
                .max_rpm
                .map(|m| format!(" · 驱动参考上限 {m}"))
                .unwrap_or_default();
            self.card(
                column![
                    row![
                        text(label)
                            .size(14)
                            .font(style::font(Weight::Semibold))
                            .style(muted),
                        space().width(Length::Fill),
                        container(text(chip_text).size(12))
                            .padding([3, 10])
                            .style(move |t| style::chip(t, chip_tone)),
                    ]
                    .align_y(Alignment::Center),
                    row![
                        text(temp_txt).size(48).font(style::font(Weight::Semibold)),
                        text("°C").size(18).style(muted)
                    ]
                    .align_y(Alignment::End)
                    .spacing(4),
                    text(format!("风扇 {rpm}"))
                        .size(16)
                        .font(style::font(Weight::Medium)),
                    text(format!("{boost}{reference}")).size(13).style(muted),
                    text(extra).size(13).style(muted),
                ]
                .spacing(5),
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
        let cpu = tile("CPU", r.and_then(|r| r.cpu_temp_c), &c.fans[0], cpu_extra);
        let gpu = tile("GPU", r.and_then(|r| r.gpu_temp_c), &c.fans[1], gpu_extra);
        // side by side when there is room, stacked in a narrow window
        let tiles: Element<'_, Message> = if narrow {
            column![cpu, gpu].spacing(12).into()
        } else {
            row![cpu, gpu].spacing(14).into()
        };
        column![tiles, text(profile).size(13).style(muted)]
            .spacing(8)
            .into()
    }

    fn modes(&self, s: &DaemonStatus) -> Element<'_, Message> {
        let c = &s.control;
        let mut r = Row::new().spacing(10);
        for m in Mode::ALL {
            let selected = c.mode == m;
            r = r.push(
                button(
                    container(
                        text(m.label_zh())
                            .size(15)
                            .font(style::font(Weight::Medium)),
                    )
                    .center_x(Length::Fill),
                )
                .padding([11, 8])
                .width(Length::Fill)
                .style(move |t, st| style::glass_button(t, st, selected))
                .on_press(Message::SetMode(m)),
            );
        }
        self.panel(
            column![
                text("散热模式")
                    .size(16)
                    .font(style::font(Weight::Semibold)),
                r
            ]
            .spacing(12),
        )
    }

    fn mode_panel(&self, s: &DaemonStatus) -> Element<'_, Message> {
        let c = &s.control;
        let body: Element<'_, Message> = match c.mode {
            Mode::Balanced => column![
                text("均衡").size(16).font(style::font(Weight::Semibold)),
                text("通过 power-profiles-daemon 选择 balanced 档位，风扇完全由 Dell 固件的均衡策略控制，本软件不写入 boost").size(14).style(muted),
            ]
            .spacing(6)
            .into(),
            Mode::Performance => column![
                text("性能").size(16).font(style::font(Weight::Semibold)),
                text("选择 performance 档位，G 系列笔记本上内核会同时开启 G-Mode，它会改变风扇策略，也会放宽 CPU 与 GPU 的功耗限制，不只是提高风扇转速").size(14).style(muted),
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
                    text("自定义").size(16).font(style::font(Weight::Semibold)),
                    text("Boost 是在固件基准转速之上附加的提升量，0 表示不附加，交给固件。本机实测 160 已让两个风扇超过驱动报告的参考上限，软件只开放 0 到 160").size(13).style(muted),
                ]
                .spacing(12);
                for fan in Fan::ALL {
                    let f = &c.fans[fan.index()];
                    let v = self.sliders[fan.index()];
                    col = col.push(
                        column![
                            row![
                                text(fan.label_zh()).size(15).font(style::font(Weight::Medium)),
                                space().width(Length::Fill),
                                text(format!(
                                    "Boost {v} / 160 · 读回 {} · {} RPM",
                                    f.readback.map(|b| b.get().to_string()).unwrap_or("--".into()),
                                    f.rpm.map(|r| r.to_string()).unwrap_or("--".into())
                                ))
                                .size(13)
                                .style(muted),
                            ],
                            slider(0u8..=crate::hw::MAX_VERIFIED_BOOST, v, move |x| Message::SliderMoved(fan, x))
                                .on_release(Message::SliderReleased(fan))
                                .style(style::glass_slider),
                        ]
                        .spacing(8),
                    );
                }
                col.into()
            }
            Mode::Smart => self.smart_panel(s),
        };
        if !matches!(c.mode, Mode::Custom | Mode::Smart) {
            return self.panel(body);
        }
        let collapsed = self.prefs.mode_panel_collapsed;
        let toggle = button(text(if collapsed { "展开" } else { "收起" }).size(13))
            .padding([6, 12])
            .style(style::ghost_button)
            .on_press(Message::ToggleModePanel);
        if collapsed {
            let summary = Fan::ALL
                .iter()
                .map(|f| {
                    let x = &c.fans[f.index()];
                    format!(
                        "{} Boost {} · {} RPM",
                        f.label_zh(),
                        x.readback
                            .map(|b| b.get().to_string())
                            .unwrap_or("--".into()),
                        x.rpm.map(|r| r.to_string()).unwrap_or("--".into())
                    )
                })
                .collect::<Vec<_>>()
                .join("　");
            return self.panel(
                row![
                    text(c.mode.label_zh())
                        .size(16)
                        .font(style::font(Weight::Semibold)),
                    text(summary).size(13).style(muted).width(Length::Fill),
                    toggle
                ]
                .spacing(12)
                .align_y(Alignment::Center),
            );
        }
        self.panel(column![body, row![space().width(Length::Fill), toggle]].spacing(8))
    }

    fn smart_panel(&self, s: &DaemonStatus) -> Element<'_, Message> {
        let c = &s.control;
        let mut col = column![
            row![
                text("智能").size(16).font(style::font(Weight::Semibold)),
                space().width(Length::Fill),
                button(
                    text(if self.prefs.show_details {
                        "收起算法细节"
                    } else {
                        "显示算法细节"
                    })
                    .size(13)
                )
                .padding([6, 12])
                .style(style::ghost_button)
                .on_press(Message::ToggleDetails),
            ]
            .align_y(Alignment::Center),
            text("双时间尺度 EMA、升降双阈值、降速保持与非对称步长，临界温度保护独立于曲线")
                .size(13)
                .style(muted),
        ]
        .spacing(8);
        let Some(d) = &c.smart else {
            return col.push(text("启用风扇控制后开始计算").size(14)).into();
        };
        for (i, (name, sd, fan)) in [("CPU", &d.cpu, &c.fans[0]), ("GPU", &d.gpu, &c.fans[1])]
            .into_iter()
            .enumerate()
        {
            let fan_line = format!(
                "{name} 风扇：目标 {} · 写入 {} · 读回 {} · {} RPM · {}{} · {}",
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
                    .map(|f| driver_label(f[i].driver))
                    .unwrap_or_default(),
                response_label(fan.response),
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
        let ev = &s.control.events;
        let simulated = s.control.simulated;
        let shown_n = if self.prefs.show_events {
            ev.len()
        } else {
            ev.len().min(EVENTS_COLLAPSED)
        };
        let header = row![
            text("最近事件")
                .size(16)
                .font(style::font(Weight::Semibold)),
            text(format!(
                "显示 {shown_n} / 共 {}，最多保留 {}",
                ev.len(),
                crate::control::MAX_EVENTS
            ))
            .size(12)
            .style(muted)
            .width(Length::Fill),
            button(
                text(if self.prefs.show_events {
                    "收起"
                } else {
                    "展开"
                })
                .size(13)
            )
            .padding([6, 12])
            .style(style::ghost_button)
            .on_press_maybe((ev.len() > EVENTS_COLLAPSED).then_some(Message::ToggleEvents)),
        ]
        .spacing(10)
        .align_y(Alignment::Center);
        // newest first; every row uses the full card width and wraps long text
        let mut list = Column::new().spacing(6).width(Length::Fill);
        for e in ev.iter().rev().take(shown_n) {
            let tone = match e.level.as_str() {
                "error" => Tone::Error,
                "warn" => Tone::Warn,
                _ => Tone::Neutral,
            };
            let when = if e.count > 1 && e.first_unix > 0 {
                format!(
                    "{} 次 · 首次 {} · 最近 {}",
                    e.count,
                    fmt_time(e.first_unix),
                    fmt_time(e.unix_s)
                )
            } else {
                fmt_time(e.unix_s)
            };
            let mut line = Row::new()
                .spacing(10)
                .align_y(Alignment::Start)
                .push(
                    container(space().width(7).height(7))
                        .width(7)
                        .height(7)
                        .padding(0)
                        .style(move |t| style::dot(t, tone)),
                )
                .push(
                    column![
                        text(e.text.clone()).size(13).width(Length::Fill),
                        text(when).size(11).style(muted),
                    ]
                    .spacing(2)
                    .width(Length::Fill),
                );
            if simulated {
                line = line.push(
                    container(text("模拟").size(11))
                        .padding([2, 8])
                        .style(|t| style::chip(t, Tone::Info)),
                );
            }
            list = list.push(container(line).padding([2, 0]).width(Length::Fill));
        }
        if ev.is_empty() {
            list = list.push(text("暂无事件").size(13).style(muted));
        }
        self.panel(column![header, list].spacing(10).width(Length::Fill))
    }

    fn settings(&self) -> Element<'_, Message> {
        let mut looks = Row::new().spacing(10);
        for a in Appearance::ALL {
            let selected = self.prefs.appearance == a;
            looks = looks.push(
                button(container(text(a.label()).size(14)).center_x(Length::Fill))
                    .padding([9, 6])
                    .width(Length::Fill)
                    .style(move |t, st| style::glass_button(t, st, selected))
                    .on_press(Message::Appearance(a)),
            );
        }
        let autostart = match self.autostart {
            Some(on) => toggler(on)
                .label("登录时自动启动后台服务")
                .text_size(14)
                .style(style::glass_toggler)
                .on_toggle(Message::SetAutostart),
            None => toggler(false)
                .label("登录时自动启动（服务未安装）")
                .text_size(14)
                .style(style::glass_toggler),
        };
        let quit = button(text("彻底退出（撤销 boost 并恢复固件控制）").size(14))
            .padding([10, 14])
            .style(style::danger_button)
            .on_press_maybe((self.status.is_some() && !self.quitting).then_some(Message::Quit));
        self.panel(
            column![
                text("设置").size(16).font(style::font(Weight::Semibold)),
                autostart,
                text("外观").size(14).style(muted),
                looks,
                row![
                    text("关闭窗口不会停止后台控制").size(13).style(muted),
                    space().width(Length::Fill),
                    quit
                ]
                .align_y(Alignment::Center),
            ]
            .spacing(12),
        )
    }
}

/// Upper bound of notification bars at the top; the rest is in the history
const MAX_NOTICES: usize = 4;

/// Below this window width the cards stack vertically
const NARROW_WIDTH: f32 = 640.0;

/// Events shown while the history is folded
const EVENTS_COLLAPSED: usize = 6;

fn muted(t: &Theme) -> text::Style {
    text::Style {
        color: Some(style::muted(t)),
    }
}

/// Short condition of a fan for the card chip: failure, unconfirmed, normal
fn fan_condition(c: &ControlStatus, f: &FanStatus) -> (Tone, &'static str) {
    if !c.enabled || f.target.is_none() {
        return (Tone::Neutral, "固件控制");
    }
    if f.blocked.is_some() || f.last_error.is_some() {
        return (Tone::Error, "控制失败");
    }
    match f.response {
        ResponseState::Unconfirmed => (Tone::Info, "响应无法确认"),
        ResponseState::Confirmed => (Tone::Ok, "正常 · 已确认响应"),
        _ => (Tone::Ok, "正常运行"),
    }
}

fn response_label(r: ResponseState) -> &'static str {
    match r {
        ResponseState::Idle => "未加速",
        ResponseState::Observing => "观察中",
        ResponseState::Confirmed => "已确认响应",
        ResponseState::Unconfirmed => "响应无法确认",
    }
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
