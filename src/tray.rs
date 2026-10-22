//! StatusNotifierItem tray of the background service
//!
//! The tray lives in the service, not in the GUI, so it stays available after
//! the window is closed. Menu actions are sent to the control thread as typed
//! commands; nothing here touches hardware directly

use crate::control::{ControlState, Mode};
use crate::daemon::Command;
use crate::ipc::Request;
use ksni::menu::{CheckmarkItem, RadioGroup, RadioItem, StandardItem, SubMenu};
use ksni::{Icon, MenuItem, ToolTip};
use std::sync::mpsc::Sender;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TraySummary {
    pub enabled: bool,
    pub mode: Mode,
    pub state: String,
    pub cpu: String,
    pub gpu: String,
}

impl TraySummary {
    pub fn from_status(s: &crate::control::ControlStatus) -> Self {
        let r = s.readings.as_ref();
        let fan = |t: Option<f32>, rpm: Option<u32>| {
            format!(
                "{} · {}",
                t.map(|v| format!("{v:.0} °C")).unwrap_or("-- °C".into()),
                rpm.map(|v| format!("{v} RPM")).unwrap_or("-- RPM".into())
            )
        };
        TraySummary {
            enabled: s.enabled,
            mode: s.mode,
            state: state_label(&s.state),
            cpu: fan(r.and_then(|r| r.cpu_temp_c), s.fans[0].rpm),
            gpu: fan(r.and_then(|r| r.gpu_temp_c), s.fans[1].rpm),
        }
    }
}

pub fn state_label(s: &ControlState) -> String {
    match s {
        ControlState::Off => "未启用，固件控制".into(),
        ControlState::Restoring => "正在恢复固件控制".into(),
        ControlState::Firmware => "固件策略".into(),
        ControlState::Controlling => "软件控制中".into(),
        ControlState::Fallback(r) => format!("已交还固件：{r}"),
        ControlState::NoPermission(_) => "缺少写入权限".into(),
        ControlState::Fault(r) => format!("故障：{r}"),
    }
}

pub struct FanTray {
    pub summary: TraySummary,
    pub commands: Sender<Command>,
}

impl FanTray {
    fn send(&self, req: Request) {
        if self.commands.send(Command::Request(req, None)).is_err() {
            log::warn!("托盘命令发送失败，控制线程已退出");
        }
    }
}

impl ksni::Tray for FanTray {
    fn id(&self) -> String {
        "dell-fan-control".into()
    }

    fn title(&self) -> String {
        "戴尔风扇控制".into()
    }

    fn icon_name(&self) -> String {
        "dell-fan-control".into()
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        [22u32, 32, 48, 64]
            .iter()
            .map(|&s| Icon {
                width: s as i32,
                height: s as i32,
                data: crate::icon::argb(s),
            })
            .collect()
    }

    fn tool_tip(&self) -> ToolTip {
        let s = &self.summary;
        ToolTip {
            title: format!(
                "戴尔风扇控制 · {}",
                if s.enabled {
                    format!("{}模式", s.mode.label_zh())
                } else {
                    "未启用".into()
                }
            ),
            description: format!("CPU {}\nGPU {}\n{}", s.cpu, s.gpu, s.state),
            icon_name: String::new(),
            icon_pixmap: Vec::new(),
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        if let Err(e) = crate::sys::launch_gui() {
            log::error!("{e}");
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let s = &self.summary;
        let modes = Mode::ALL;
        vec![
            StandardItem {
                label: "打开主界面".into(),
                activate: Box::new(|_: &mut Self| {
                    if let Err(e) = crate::sys::launch_gui() {
                        log::error!("{e}");
                    }
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: format!("CPU  {}", s.cpu),
                enabled: false,
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: format!("GPU  {}", s.gpu),
                enabled: false,
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: format!("状态：{}", s.state),
                enabled: false,
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            CheckmarkItem {
                label: "启用风扇控制".into(),
                checked: s.enabled,
                activate: Box::new(|t: &mut Self| {
                    let on = !t.summary.enabled;
                    t.send(Request::SetEnabled { enabled: on });
                }),
                ..Default::default()
            }
            .into(),
            SubMenu {
                label: format!("散热模式：{}", s.mode.label_zh()),
                submenu: vec![
                    RadioGroup {
                        selected: modes.iter().position(|m| *m == s.mode).unwrap_or(0),
                        select: Box::new(move |t: &mut Self, i| {
                            if let Some(m) = modes.get(i) {
                                t.send(Request::SetMode { mode: *m });
                            }
                        }),
                        options: modes
                            .iter()
                            .map(|m| RadioItem {
                                label: m.label_zh().into(),
                                ..Default::default()
                            })
                            .collect(),
                    }
                    .into(),
                ],
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "彻底退出（恢复固件控制）".into(),
                activate: Box::new(|t: &mut Self| t.send(Request::Quit)),
                ..Default::default()
            }
            .into(),
        ]
    }
}
