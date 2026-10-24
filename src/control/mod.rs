//! Control core: master switch, the four modes, restore and fault handling
//!
//! The controller is driven by `tick()` from the daemon's control thread and
//! by commands coming from the GUI and the tray. It owns the hardware and the
//! profile backend so every write goes through one place

pub mod curve;
pub mod filter;
pub mod response;
pub mod smart;

use crate::hw::gpu::GpuReading;
use crate::hw::{Boost, Fan, FanHardware, HwError, SensorReadings, WriteAccess};
use crate::profile::ProfileBackend;
use crate::state::{AppliedMarker, MarkerStore, ProfileChange};
use response::{ResponseMonitor, ResponseState, Transition};
use serde::{Deserialize, Serialize};
use smart::{
    Guard, OutputPlan, OutputStage, SmartDecision, SmartEngine, SmartInputs, SmartParams,
    StepLimits,
};
use std::collections::VecDeque;

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Dell firmware balanced policy, no boost
    #[default]
    Balanced,
    /// Performance profile, which the kernel maps to G-Mode on G-series laptops
    Performance,
    /// Fixed user boost per fan
    Custom,
    /// Automatic control
    Smart,
}

impl Mode {
    pub const ALL: [Mode; 4] = [Mode::Balanced, Mode::Performance, Mode::Custom, Mode::Smart];

    pub fn label_zh(self) -> &'static str {
        match self {
            Mode::Balanced => "均衡",
            Mode::Performance => "性能",
            Mode::Custom => "自定义",
            Mode::Smart => "智能",
        }
    }

    fn writes_boost(self) -> bool {
        matches!(self, Mode::Custom | Mode::Smart)
    }

    fn profile(self) -> Option<&'static str> {
        match self {
            Mode::Balanced => Some("balanced"),
            Mode::Performance => Some("performance"),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Extras {
    pub cpu_load_pct: Option<f32>,
    pub gpu: Option<GpuReading>,
    pub on_ac: Option<bool>,
    pub platform_profile: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FanStatus {
    pub fan: Option<Fan>,
    /// Value the algorithm or the user asked for
    pub target: Option<Boost>,
    /// Last value written by this program
    pub written: Option<Boost>,
    /// Value read back from the driver
    pub readback: Option<Boost>,
    pub rpm: Option<u32>,
    pub max_rpm: Option<u32>,
    pub plan: Option<OutputPlan>,
    pub response: ResponseState,
    /// Step windows without evidence since the last confirmation
    pub response_misses: u32,
    pub override_count: u32,
    pub blocked: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "detail")]
pub enum ControlState {
    /// Master switch off, firmware in control
    Off,
    /// Undoing changes after switching off or quitting
    Restoring,
    /// Enabled in a firmware driven mode (balanced or performance)
    Firmware,
    /// Writing boost values
    Controlling,
    /// Smart control handed the fans back to the firmware temporarily
    Fallback(String),
    /// Boost cannot be written by this user
    NoPermission(String),
    /// Control stopped after repeated failures; needs the switch toggled
    Fault(String),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub unix_s: u64,
    pub level: String,
    pub text: String,
    /// Identical events are merged: how often it happened
    #[serde(default = "one")]
    pub count: u32,
    /// First occurrence of a merged event, unix seconds
    #[serde(default)]
    pub first_unix: u64,
}

fn one() -> u32 {
    1
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertLevel {
    /// Something the user should know, not a fault
    Info,
    /// The program is protecting the machine or fell back to the firmware
    Warning,
    /// Control failed or cannot work
    Error,
}

/// A condition that holds right now. Alerts are rebuilt every tick, so a
/// condition that clears disappears by itself and one condition is never
/// listed twice
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Alert {
    pub key: String,
    pub level: AlertLevel,
    pub fan: Option<Fan>,
    pub text: String,
    /// When this condition was first seen, unix seconds
    pub since_unix: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ControlStatus {
    pub enabled: bool,
    pub mode: Mode,
    pub custom: [Boost; 2],
    pub state: ControlState,
    pub readings: Option<SensorReadings>,
    pub read_error: Option<String>,
    pub cpu_load_pct: Option<f32>,
    pub gpu: Option<GpuReading>,
    pub on_ac: Option<bool>,
    pub platform_profile: Option<String>,
    pub ppd_profile: Option<String>,
    pub fans: [FanStatus; 2],
    pub smart: Option<SmartDecision>,
    pub guard: Guard,
    pub marker: AppliedMarker,
    pub write_access: WriteAccess,
    pub simulated: bool,
    pub hardware: String,
    pub alerts: Vec<Alert>,
    pub events: Vec<Event>,
}

#[derive(Debug, Default)]
struct FanTrack {
    output: OutputStage,
    response: ResponseMonitor,
    override_times: VecDeque<f64>,
    blocked: Option<String>,
    failures: u32,
    retry_at: f64,
    last_error: Option<String>,
    last_plan: Option<OutputPlan>,
    target: Option<Boost>,
}

/// Outcome of a restore, used by the `restore` command and the quit path
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RestoreReport {
    pub fans_released: Vec<Fan>,
    pub profile_restored: Option<String>,
    pub profile_left_alone: Option<String>,
    pub errors: Vec<String>,
}

impl RestoreReport {
    pub fn ok(&self) -> bool {
        self.errors.is_empty()
    }
}

pub const MAX_EVENTS: usize = 50;
const OVERRIDE_SETTLE_S: f64 = 2.5;
const OVERRIDE_WINDOW_S: f64 = 120.0;
const OVERRIDE_LIMIT: usize = 3;
const WRITE_FAILURE_LIMIT: u32 = 3;
const GUARD_LONG_S: f64 = 120.0;

fn custom_limits() -> StepLimits {
    // Custom values come from a slider; only debounce them
    StepLimits {
        inc_min: 1,
        inc_max: 255,
        dec_min: 1,
        dec_max: 255,
        inc_interval_s: 0.3,
        dec_interval_s: 0.3,
    }
}

pub struct Controller {
    enabled: bool,
    mode: Mode,
    custom: [Boost; 2],
    hw: Box<dyn FanHardware>,
    profile: Box<dyn ProfileBackend>,
    marker: MarkerStore,
    engine: SmartEngine,
    custom_guard: Guard,
    fans: [FanTrack; 2],
    applied_mode: Option<Mode>,
    fault: Option<String>,
    restore_retry_at: f64,
    events: VecDeque<Event>,
    status: ControlStatus,
    permission_reported: bool,
    /// Set by every hardware write so the tick can read back fresh values
    wrote: bool,
    guard_long_reported: bool,
    alert_since: std::collections::HashMap<String, u64>,
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Controller {
    pub fn new(
        hw: Box<dyn FanHardware>,
        profile: Box<dyn ProfileBackend>,
        marker: MarkerStore,
        enabled: bool,
        mode: Mode,
        custom: [Boost; 2],
        smart: SmartParams,
    ) -> Self {
        let status = ControlStatus {
            enabled,
            mode,
            custom,
            state: ControlState::Off,
            readings: None,
            read_error: None,
            cpu_load_pct: None,
            gpu: None,
            on_ac: None,
            platform_profile: None,
            ppd_profile: None,
            fans: Default::default(),
            smart: None,
            guard: Guard::default(),
            marker: marker.get().clone(),
            write_access: hw.write_access(),
            simulated: hw.is_simulated(),
            hardware: hw.describe(),
            alerts: Vec::new(),
            events: Vec::new(),
        };
        Controller {
            enabled,
            mode,
            custom,
            hw,
            profile,
            marker,
            engine: SmartEngine::new(smart),
            custom_guard: Guard::default(),
            fans: Default::default(),
            applied_mode: None,
            fault: None,
            restore_retry_at: 0.0,
            events: VecDeque::new(),
            status,
            permission_reported: false,
            wrote: false,
            guard_long_reported: false,
            alert_since: std::collections::HashMap::new(),
        }
    }

    pub fn log(&mut self, level: &str, text: impl Into<String>) {
        let text = text.into();
        match level {
            "error" => log::error!("{text}"),
            "warn" => log::warn!("{text}"),
            _ => log::info!("{text}"),
        }
        // an identical event anywhere in the history is merged into one entry
        // that moves to the newest position and counts its occurrences
        let now = unix_now();
        if let Some(pos) = self
            .events
            .iter()
            .position(|e| e.text == text && e.level == level)
            && let Some(mut e) = self.events.remove(pos)
        {
            e.count = e.count.saturating_add(1);
            e.unix_s = now;
            self.events.push_back(e);
            return;
        }
        self.events.push_back(Event {
            unix_s: now,
            level: level.into(),
            text,
            count: 1,
            first_unix: now,
        });
        while self.events.len() > MAX_EVENTS {
            self.events.pop_front();
        }
    }

    pub fn status(&self) -> &ControlStatus {
        &self.status
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn custom(&self) -> [Boost; 2] {
        self.custom
    }

    pub fn smart_params(&self) -> &SmartParams {
        self.engine.params()
    }

    pub fn marker(&self) -> &AppliedMarker {
        self.marker.get()
    }

    fn clear_runtime_state(&mut self) {
        self.engine.reset();
        self.custom_guard = Guard::default();
        for f in &mut self.fans {
            *f = FanTrack::default();
        }
        self.fault = None;
        self.permission_reported = false;
    }

    pub fn set_enabled(&mut self, on: bool) {
        if on == self.enabled {
            return;
        }
        self.enabled = on;
        self.clear_runtime_state();
        self.applied_mode = None;
        self.log(
            "info",
            if on {
                "已启用风扇控制"
            } else {
                "已关闭风扇控制，正在交还固件"
            },
        );
    }

    pub fn set_mode(&mut self, mode: Mode) {
        if mode == self.mode {
            return;
        }
        self.log(
            "info",
            format!("模式切换：{} → {}", self.mode.label_zh(), mode.label_zh()),
        );
        self.mode = mode;
        self.clear_runtime_state();
    }

    pub fn set_custom(&mut self, fan: Fan, boost: Boost) {
        self.custom[fan.index()] = boost.min(Boost::VERIFIED_MAX);
    }

    pub fn set_smart_params(&mut self, p: SmartParams) {
        self.engine.set_params(p);
    }

    fn set_profile_owned(&mut self, target: &str) {
        let current = match self.profile.get() {
            Ok(c) => c,
            Err(e) => {
                self.log("warn", format!("无法读取电源档位：{e}"));
                return;
            }
        };
        if current == target {
            return;
        }
        // keep the very first original if the program already changed it
        let original = self
            .marker
            .get()
            .profile
            .as_ref()
            .map(|c| c.original.clone())
            .unwrap_or(current.clone());
        let change = ProfileChange {
            original,
            ours: target.to_string(),
        };
        if let Err(e) = self.marker.set_profile(Some(change)) {
            self.log("error", format!("无法记录档位变更，已取消切换：{e}"));
            return;
        }
        match self.profile.set(target) {
            Ok(()) => self.log("info", format!("电源档位 {current} → {target}")),
            Err(e) => {
                self.log("error", format!("切换电源档位失败：{e}"));
                // nothing changed, forget the record unless it was older
                if self.profile.get().ok().as_deref() == Some(current.as_str()) {
                    let keep = self
                        .marker
                        .get()
                        .profile
                        .as_ref()
                        .filter(|c| c.original != current)
                        .cloned();
                    let _ = self.marker.set_profile(keep);
                }
            }
        }
    }

    /// Put the profile back if it is still the one this program selected.
    /// If somebody else changed it in the meantime, their choice wins
    fn restore_profile(&mut self, report: &mut RestoreReport) {
        let Some(change) = self.marker.get().profile.clone() else {
            return;
        };
        match self.profile.get() {
            Ok(cur) if cur == change.ours => {
                if cur == change.original {
                    let _ = self.marker.set_profile(None);
                    return;
                }
                match self.profile.set(&change.original) {
                    Ok(()) => {
                        report.profile_restored = Some(change.original.clone());
                        let _ = self.marker.set_profile(None);
                        self.log("info", format!("电源档位已恢复为 {}", change.original));
                    }
                    Err(e) => report.errors.push(format!("恢复电源档位失败：{e}")),
                }
            }
            Ok(cur) => {
                report.profile_left_alone = Some(cur.clone());
                let _ = self.marker.set_profile(None);
                self.log(
                    "info",
                    format!(
                        "电源档位已被其他程序改为 {cur}，不再改回 {}",
                        change.original
                    ),
                );
            }
            Err(e) => report.errors.push(format!("读取电源档位失败：{e}")),
        }
    }

    /// Release every boost this program set, verify the read back, and put
    /// the profile back. Used when switching off, quitting and recovering
    pub fn restore_all(&mut self) -> RestoreReport {
        let mut report = RestoreReport::default();
        let readings = self.hw.read().ok();
        for fan in Fan::ALL {
            let ours = self.marker.get().boosted_fans.contains(&fan)
                || self.fans[fan.index()]
                    .output
                    .last_written
                    .is_some_and(|b| b != Boost::ZERO);
            if !ours {
                continue;
            }
            let already_zero = readings.as_ref().and_then(|r| r.fan(fan).boost)
                == Some(Boost::ZERO)
                && self.fans[fan.index()]
                    .output
                    .last_written
                    .is_none_or(|b| b == Boost::ZERO);
            let result = if already_zero {
                Ok(Some(Boost::ZERO))
            } else {
                self.wrote = true;
                self.hw.write_boost(fan, Boost::ZERO)
            };
            match result {
                Ok(Some(b)) if b == Boost::ZERO => {
                    let _ = self.marker.remove_fan(fan);
                    self.fans[fan.index()].output.note_written(Boost::ZERO, 0.0);
                    report.fans_released.push(fan);
                }
                Ok(back) => report.errors.push(format!(
                    "{} 写入 0 后读回 {:?}",
                    fan.label_zh(),
                    back.map(|b| b.get())
                )),
                Err(e) => {
                    report
                        .errors
                        .push(format!("{} 释放失败 [{}]：{e}", fan.label_zh(), e.code()))
                }
            }
        }
        self.restore_profile(&mut report);
        if !report.fans_released.is_empty() {
            let names: Vec<_> = report.fans_released.iter().map(|f| f.label_zh()).collect();
            self.log("info", format!("已撤销 boost：{}", names.join("、")));
        }
        for e in report.errors.clone() {
            self.log("error", e);
        }
        report
    }

    /// Called once at daemon start. A marker left behind means the previous
    /// instance did not get to restore (crash, SIGKILL, power loss)
    pub fn recover_leftovers(&mut self) -> Option<RestoreReport> {
        if self.marker.get().is_empty() {
            return None;
        }
        self.log("warn", "发现上次运行留下的覆盖记录，先恢复固件控制");
        Some(self.restore_all())
    }

    fn write(&mut self, fan: Fan, value: Boost, now: f64, rpm_before: Option<u32>) {
        let i = fan.index();
        // hard ceiling for every path: modes, guard, requests
        let value = value.min(Boost::VERIFIED_MAX);
        if now < self.fans[i].retry_at {
            return;
        }
        if value != Boost::ZERO
            && let Err(e) = self.marker.add_fan(fan)
        {
            self.log(
                "error",
                format!("无法写入覆盖记录，取消写入 {}：{e}", fan.label_zh()),
            );
            return;
        }
        let before = self.fans[i].output.last_written;
        self.wrote = true;
        match self.hw.write_boost(fan, value) {
            Ok(back) => {
                let t = &mut self.fans[i];
                t.failures = 0;
                t.last_error = None;
                t.output.note_written(value, now);
                if back != Some(value) {
                    t.last_error = Some(format!(
                        "写入 {} 后读回 {:?}",
                        value.get(),
                        back.map(|b| b.get())
                    ));
                }
                if value == Boost::ZERO && back == Some(Boost::ZERO) {
                    let _ = self.marker.remove_fan(fan);
                }
                let from = before.unwrap_or(Boost::ZERO);
                t.response.note_write(from, value, now, rpm_before);
                log::debug!(
                    "{fan} boost {} -> {} 读回 {:?}",
                    from.get(),
                    value.get(),
                    back
                );
            }
            Err(e) => {
                let code = e.code();
                let t = &mut self.fans[i];
                t.failures += 1;
                t.retry_at = now + 2f64.powi(t.failures.min(5) as i32);
                t.last_error = Some(format!("[{code}] {e}"));
                let failures = t.failures;
                let msg = format!(
                    "{} 写入 {} 失败（第 {failures} 次）[{code}]：{e}",
                    fan.label_zh(),
                    value.get()
                );
                self.log("error", msg);
                if matches!(e, HwError::PermissionDenied(_)) || failures >= WRITE_FAILURE_LIMIT {
                    self.fault = Some(format!("{} 连续写入失败 [{code}]：{e}", fan.label_zh()));
                }
            }
        }
    }

    fn track_feedback(&mut self, now: f64, readings: &SensorReadings) {
        for fan in Fan::ALL {
            let i = fan.index();
            let reading = readings.fan(fan).clone();
            let mut new_event = None;
            {
                let t = &mut self.fans[i];
                match t
                    .response
                    .update(now, reading.boost, reading.rpm, reading.max_rpm)
                {
                    Transition::BecameUnconfirmed => {
                        new_event = Some(format!(
                            "{} 连续 {} 次提高 boost 后都没有观察到转速变化，响应无法确认",
                            fan.label_zh(),
                            t.response.misses()
                        ))
                    }
                    Transition::BecameConfirmed => {
                        new_event = Some(format!("{} 已观察到转速随 boost 变化", fan.label_zh()))
                    }
                    Transition::None => {}
                }
                // firmware override: the driver reports something else than we wrote
                if let (Some(w), Some(r)) = (t.output.last_written, reading.boost)
                    && r != w
                    && t.blocked.is_none()
                {
                    let settled = t
                        .output
                        .last_write_at()
                        .is_none_or(|at| now - at >= OVERRIDE_SETTLE_S);
                    if settled {
                        t.override_times.push_back(now);
                        while t
                            .override_times
                            .front()
                            .is_some_and(|x| now - x > OVERRIDE_WINDOW_S)
                        {
                            t.override_times.pop_front();
                        }
                        // writing again is the "safety latch" idea: re-apply once
                        t.output.forget();
                        if t.override_times.len() >= OVERRIDE_LIMIT {
                            t.blocked = Some(format!(
                                "boost 在 {OVERRIDE_WINDOW_S:.0} 秒内被固件改回 {OVERRIDE_LIMIT} 次，已停止控制该风扇"
                            ));
                        }
                        new_event = Some(format!(
                            "{} 读回 boost {}，与写入的 {} 不一致，可能被固件覆盖",
                            fan.label_zh(),
                            r.get(),
                            w.get()
                        ));
                    }
                }
            }
            if let Some(e) = new_event {
                self.log("warn", e);
            }
        }
    }

    /// One control cycle. `now` is monotonic seconds
    pub fn tick(&mut self, now: f64, extras: Extras) -> &ControlStatus {
        self.wrote = false;
        let (mut readings, mut read_error) = match self.hw.read() {
            Ok(r) => (Some(r), None),
            Err(e) => (None, Some(format!("[{}] {e}", e.code()))),
        };
        let access = self.hw.write_access();
        let ppd = self.profile.get().ok();

        let state = if !self.enabled {
            self.tick_disabled(now)
        } else {
            self.tick_enabled(now, &extras, readings.as_ref(), access)
        };
        if self.wrote {
            // show what the driver reports after this tick's writes, not before
            match self.hw.read() {
                Ok(r) => {
                    readings = Some(r);
                    read_error = None;
                }
                Err(e) => read_error = Some(format!("[{}] {e}", e.code())),
            }
        }

        if let Some(r) = readings.as_ref() {
            self.track_feedback(now, r);
        }

        let fans = Fan::ALL.map(|fan| {
            let t = &self.fans[fan.index()];
            let r = readings
                .as_ref()
                .map(|r| r.fan(fan).clone())
                .unwrap_or_default();
            FanStatus {
                fan: Some(fan),
                target: t.target,
                written: t.output.last_written,
                readback: r.boost,
                rpm: r.rpm,
                max_rpm: r.max_rpm,
                plan: t.last_plan.clone(),
                response: t.response.state(),
                response_misses: t.response.misses(),
                override_count: t.override_times.len() as u32,
                blocked: t.blocked.clone(),
                last_error: t.last_error.clone(),
            }
        });
        let smart = if self.enabled && self.mode == Mode::Smart {
            self.status.smart.clone()
        } else {
            None
        };
        self.status = ControlStatus {
            enabled: self.enabled,
            mode: self.mode,
            custom: self.custom,
            state,
            readings,
            read_error,
            cpu_load_pct: extras.cpu_load_pct,
            gpu: extras.gpu,
            on_ac: extras.on_ac,
            platform_profile: extras.platform_profile.or_else(|| {
                // the simulator has no kernel profile; mirror its fake ppd
                self.hw.is_simulated().then(|| ppd.clone()).flatten()
            }),
            ppd_profile: ppd,
            fans,
            smart,
            guard: if self.mode == Mode::Custom {
                self.custom_guard.clone()
            } else {
                self.status.guard.clone()
            },
            marker: self.marker.get().clone(),
            write_access: access,
            simulated: self.hw.is_simulated(),
            hardware: self.hw.describe(),
            alerts: Vec::new(),
            events: self.events.iter().cloned().collect(),
        };
        self.status.alerts = self.current_alerts();
        &self.status
    }

    fn tick_disabled(&mut self, now: f64) -> ControlState {
        self.applied_mode = None;
        for f in &mut self.fans {
            f.target = None;
            f.last_plan = None;
        }
        if self.marker.get().is_empty() {
            return ControlState::Off;
        }
        if now < self.restore_retry_at {
            return ControlState::Restoring;
        }
        let report = self.restore_all();
        if report.ok() && self.marker.get().is_empty() {
            ControlState::Off
        } else {
            self.restore_retry_at = now + 5.0;
            ControlState::Restoring
        }
    }

    fn tick_enabled(
        &mut self,
        now: f64,
        extras: &Extras,
        readings: Option<&SensorReadings>,
        access: WriteAccess,
    ) -> ControlState {
        if let Some(f) = self.fault.clone() {
            // stop controlling, keep trying to give the fans back
            if !self.marker.get().is_empty() && now >= self.restore_retry_at {
                let r = self.restore_all();
                if !r.ok() {
                    self.restore_retry_at = now + 10.0;
                }
            }
            return ControlState::Fault(f);
        }

        // mode entry and exit actions, once per change
        if self.applied_mode != Some(self.mode) {
            let mut report = RestoreReport::default();
            if self.mode.profile().is_none() {
                self.restore_profile(&mut report);
            }
            if let Some(p) = self.mode.profile() {
                self.set_profile_owned(p);
            }
            if self.mode == Mode::Smart {
                self.engine.reset();
            }
            self.applied_mode = Some(self.mode);
        }

        let needs_release: Vec<Fan> = Fan::ALL
            .into_iter()
            .filter(|f| {
                self.marker.get().boosted_fans.contains(f)
                    || self.fans[f.index()]
                        .output
                        .last_written
                        .is_some_and(|b| b != Boost::ZERO)
            })
            .collect();

        // the targets for this tick; None means "leave to firmware"
        let mut targets: [Option<(Boost, bool)>; 2] = [None, None];
        let mut state = ControlState::Firmware;
        let mut limits = self.engine.params().steps.clone();

        match self.mode {
            Mode::Balanced | Mode::Performance => {
                for f in &needs_release {
                    targets[f.index()] = Some((Boost::ZERO, true));
                }
                self.status.smart = None;
            }
            Mode::Custom => {
                limits = custom_limits();
                let was_on = self.custom_guard.engaged;
                let guard_on = self.custom_guard.update(
                    &self.engine.params().guard,
                    readings.and_then(|r| r.cpu_temp_c),
                    readings.and_then(|r| r.gpu_temp_c),
                    now,
                );
                self.log_guard_change(was_on, &self.custom_guard.clone(), now);
                let guard_boost =
                    Boost::new(self.engine.params().guard.boost).min(Boost::VERIFIED_MAX);
                for fan in Fan::ALL {
                    targets[fan.index()] = Some(if guard_on {
                        (guard_boost.max(self.custom[fan.index()]), true)
                    } else {
                        (self.custom[fan.index()], false)
                    });
                }
                state = ControlState::Controlling;
                self.status.smart = None;
            }
            Mode::Smart => {
                let input = SmartInputs {
                    cpu_temp_c: readings.and_then(|r| r.cpu_temp_c),
                    gpu_temp_c: readings.and_then(|r| r.gpu_temp_c),
                    cpu_load_pct: extras.cpu_load_pct,
                    gpu_power_w: extras.gpu.as_ref().and_then(|g| g.power_w),
                };
                let was_on = self.status.guard.engaged;
                let d = self.engine.step(&input, now);
                self.log_guard_change(was_on, &d.guard, now);
                match &d.fans {
                    Some(fans) => {
                        for fd in fans {
                            let bypass = fd.driver == smart::FanDriver::Guard;
                            targets[fd.fan.index()] = Some((fd.target, bypass));
                        }
                        state = ControlState::Controlling;
                    }
                    None => {
                        let reason = d.fallback.clone().unwrap_or_else(|| "传感器无效".into());
                        if !matches!(self.status.state, ControlState::Fallback(_)) {
                            self.log("warn", reason.clone());
                        }
                        for f in &needs_release {
                            targets[f.index()] = Some((Boost::ZERO, true));
                        }
                        state = ControlState::Fallback(reason);
                    }
                }
                self.status.guard = d.guard.clone();
                self.status.smart = Some(d);
            }
        }

        // a blocked fan is released once and then left alone
        for fan in Fan::ALL {
            if self.fans[fan.index()].blocked.is_some() {
                targets[fan.index()] = needs_release.contains(&fan).then_some((Boost::ZERO, true));
            }
        }

        let any_write_needed = targets.iter().flatten().any(|_| true);
        if any_write_needed && access != WriteAccess::Writable {
            let msg = match access {
                WriteAccess::PermissionDenied => {
                    "没有 fan*_boost 写入权限，请先执行 scripts/install-system.sh 安装 udev 规则"
                        .to_string()
                }
                _ => "没有找到 fan*_boost 属性，alienware_wmi 不支持风扇 boost".to_string(),
            };
            if !self.permission_reported {
                self.log("error", msg.clone());
                self.permission_reported = true;
            }
            for (i, t) in targets.iter().enumerate() {
                self.fans[i].target = t.map(|(b, _)| b);
            }
            return ControlState::NoPermission(msg);
        }

        for fan in Fan::ALL {
            let i = fan.index();
            let Some((target, bypass)) = targets[i] else {
                self.fans[i].target = None;
                self.fans[i].last_plan = None;
                continue;
            };
            self.fans[i].target = Some(target);
            let readback = readings.and_then(|r| r.fan(fan).boost);
            let plan = self.fans[i]
                .output
                .plan(&limits, target, readback, now, bypass);
            self.fans[i].last_plan = Some(plan.clone());
            if let OutputPlan::Write(v) = plan {
                self.write(fan, v, now, readings.and_then(|r| r.fan(fan).rpm));
            }
        }
        if let Some(f) = self.fault.clone() {
            return ControlState::Fault(f);
        }
        if self.fans.iter().all(|f| f.blocked.is_some()) && self.mode.writes_boost() {
            return ControlState::Fallback("两个风扇的 boost 都被固件持续覆盖".into());
        }
        state
    }

    fn log_guard_change(&mut self, was_on: bool, g: &Guard, now: f64) {
        match (was_on, g.engaged) {
            (false, true) => {
                self.guard_long_reported = false;
                let b = self
                    .engine
                    .params()
                    .guard
                    .boost
                    .min(crate::hw::MAX_VERIFIED_BOOST);
                self.log(
                    "warn",
                    format!(
                        "临界温度保护触发：{}，两个风扇 boost {b}（已验证的最高值）",
                        g.reason.clone().unwrap_or_default()
                    ),
                )
            }
            (true, false) => self.log("info", "临界温度保护解除，回到正常控制"),
            (true, true)
                if !self.guard_long_reported
                    && g.engaged_at.is_some_and(|at| now - at >= GUARD_LONG_S) =>
            {
                self.guard_long_reported = true;
                self.log(
                        "warn",
                        "临界保护已持续 120 秒，软件已使用验证过的最高 boost，此后依靠 Dell 固件自身的过热保护（降频）",
                    );
            }
            _ => {}
        }
    }

    fn current_alerts(&mut self) -> Vec<Alert> {
        let st = &self.status;
        let mut out: Vec<(String, AlertLevel, Option<Fan>, String)> = Vec::new();
        match &st.state {
            ControlState::Fault(r) => out.push((
                "fault".into(),
                AlertLevel::Error,
                None,
                format!("控制失败，已停止写入：{r}；请关闭后重新打开总开关"),
            )),
            ControlState::NoPermission(r) => {
                out.push(("permission".into(), AlertLevel::Error, None, r.clone()))
            }
            ControlState::Fallback(r) => out.push((
                "fallback".into(),
                AlertLevel::Warning,
                None,
                format!("已临时交还固件：{r}"),
            )),
            ControlState::Restoring => out.push((
                "restoring".into(),
                AlertLevel::Warning,
                None,
                "正在撤销软件设置并交还固件控制".into(),
            )),
            _ => {}
        }
        if let Some(e) = &st.read_error {
            out.push((
                "read".into(),
                AlertLevel::Error,
                None,
                format!("读取传感器失败：{e}"),
            ));
        }
        if st.guard.engaged {
            out.push((
                "guard".into(),
                AlertLevel::Warning,
                None,
                format!(
                    "临界温度保护：{}，两个风扇使用已验证的最高 boost",
                    st.guard.reason.clone().unwrap_or_default()
                ),
            ));
        }
        if st.enabled {
            for f in &st.fans {
                let Some(fan) = f.fan else { continue };
                if let Some(b) = &f.blocked {
                    out.push((
                        format!("blocked-{fan}"),
                        AlertLevel::Error,
                        Some(fan),
                        format!("{} 控制失败：{b}", fan.label_zh()),
                    ));
                } else if let Some(e) = &f.last_error {
                    out.push((
                        format!("write-{fan}"),
                        AlertLevel::Error,
                        Some(fan),
                        format!("{} 写入异常：{e}", fan.label_zh()),
                    ));
                } else if f.override_count > 0 {
                    out.push((
                        format!("override-{fan}"),
                        AlertLevel::Warning,
                        Some(fan),
                        format!(
                            "{} 最近 {} 次读回与写入不一致，正在重新写入",
                            fan.label_zh(),
                            f.override_count
                        ),
                    ));
                }
                if f.response == ResponseState::Unconfirmed {
                    out.push((
                        format!("response-{fan}"),
                        AlertLevel::Info,
                        Some(fan),
                        format!("{} 响应无法确认：最近几次提高 boost 后没有观察到转速变化，控制本身没有报错", fan.label_zh()),
                    ));
                }
            }
        }
        let now = unix_now();
        let mut since = std::mem::take(&mut self.alert_since);
        since.retain(|k, _| out.iter().any(|(key, ..)| key == k));
        let alerts = out
            .into_iter()
            .map(|(key, level, fan, text)| {
                let s = *since.entry(key.clone()).or_insert(now);
                Alert {
                    key,
                    level,
                    fan,
                    text,
                    since_unix: s,
                }
            })
            .collect();
        self.alert_since = since;
        alerts
    }

    /// Restore everything without changing the saved switch, for quitting
    /// and service shutdown
    pub fn shutdown(&mut self) -> RestoreReport {
        let r = self.restore_all();
        self.applied_mode = None;
        r
    }
}

#[cfg(test)]
mod tests;
