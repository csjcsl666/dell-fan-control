//! Smart thermal control
//!
//! Pipeline, one stage per type so each can be tested and inspected:
//!
//! 1. [`SensorGate`]   validity check, short hold of the last good value
//! 2. [`SourceState`]  raw temperature -> fast EMA, slow EMA, slope
//! 3. demand           curve lookup with dual thresholds, separate per source
//! 4. coordination     per fan output from both demands
//! 5. [`Guard`]        critical temperature latch on raw values
//! 6. [`OutputStage`]  minimum change, asymmetric step size, write interval
//!
//! Design references (ideas only, independent implementation):
//! - CoolerControl `engine/processors/functions.rs`: asymmetric min/max step
//!   sizes, response delay applied to falling temperatures only, bypass of the
//!   minimum step at the extremes, failsafe on missing temperatures
//! - NBFC-Linux `temperature_threshold_manager.c`: UpThreshold/DownThreshold so
//!   a level is entered at one temperature and left at a lower one
//! - thinkpad-fan-control `thinkpad-fand`: one curve and one hysteresis state per
//!   heat source, then combine the *requests*, never the raw temperatures;
//!   latched critical override with a release margin
//! - thinkfan `temperature_state.cpp`: a bias added when temperature jumps, so
//!   a fast rise is answered before the average catches up
//! - Cryo `daemon/engine.py`: thermal guard that outranks curves and only
//!   releases after a margin and a minimum hold

use super::curve::Curve;
use super::filter::{Ema, Slope};
use crate::hw::{Boost, Fan};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StepLimits {
    /// Ignore increases smaller than this (unless reaching 255)
    pub inc_min: u8,
    /// Largest increase per write
    pub inc_max: u8,
    /// Ignore decreases smaller than this (unless reaching 0)
    pub dec_min: u8,
    /// Largest decrease per write
    pub dec_max: u8,
    /// Minimum seconds between two increasing writes
    pub inc_interval_s: f32,
    /// Minimum seconds between two decreasing writes
    pub dec_interval_s: f32,
}

impl Default for StepLimits {
    fn default() -> Self {
        StepLimits {
            inc_min: 8,
            inc_max: 96,
            dec_min: 8,
            dec_max: 24,
            inc_interval_s: 2.0,
            dec_interval_s: 8.0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GuardParams {
    pub cpu_trip_c: f32,
    pub cpu_release_c: f32,
    pub gpu_trip_c: f32,
    pub gpu_release_c: f32,
    /// The guard stays engaged at least this long once tripped
    pub min_hold_s: f32,
    /// Boost applied while the guard is engaged. Never above the verified
    /// ceiling; the firmware's own over temperature protection stays
    /// underneath because boost only adds to the firmware speed
    pub boost: u8,
}

impl Default for GuardParams {
    fn default() -> Self {
        // Calibrated against the first hardware run (2026-10-23): with only the
        // Steam client open the firmware keeps the CPU at 91 to 93 °C with the
        // fan at 3600 RPM, so a trip at 92 °C fired during normal use. The
        // trip now sits above that band and below the 105 °C limit of the
        // 5800H; the firmware keeps its own protection underneath
        GuardParams {
            cpu_trip_c: 96.0,
            cpu_release_c: 90.0,
            gpu_trip_c: 87.0,
            gpu_release_c: 80.0,
            min_hold_s: 30.0,
            boost: crate::hw::MAX_VERIFIED_BOOST,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SmartParams {
    /// Time constant of the fast EMA, used for rising temperatures
    pub fast_tau_s: f32,
    /// Time constant of the slow EMA, used before lowering boost
    pub slow_tau_s: f32,
    /// Time constant of the slope filter
    pub slope_tau_s: f32,
    /// Look ahead on a positive slope, in seconds
    pub lead_s: f32,
    /// Upper bound of the look ahead, in degrees
    pub max_lead_c: f32,
    /// A demand is only lowered once the slow EMA is this far below the
    /// temperature that produced it
    pub hysteresis_c: f32,
    /// Minimum time a demand is held before it may be lowered
    pub down_hold_s: f32,
    /// CPU utilisation above which the CPU source looks ahead further
    pub cpu_load_threshold_pct: f32,
    pub cpu_load_lead_c: f32,
    /// GPU power range that adds look ahead to the GPU source
    pub gpu_power_start_w: f32,
    pub gpu_power_full_w: f32,
    pub gpu_power_lead_c: f32,
    /// Share of the other source's demand applied to a fan, 0 disables it
    /// until real measurements show a benefit
    pub cross_coupling: f32,
    /// Normal operation never exceeds this boost; the guard ignores it
    pub noise_cap: u8,
    pub cpu_curve: Curve,
    pub gpu_curve: Curve,
    pub steps: StepLimits,
    pub guard: GuardParams,
    /// Consecutive invalid samples tolerated before giving control back
    pub sensor_grace_samples: u32,
    /// Consecutive valid samples needed before control resumes
    pub sensor_recover_samples: u32,
}

pub fn default_cpu_curve() -> Curve {
    Curve::new(&[
        (55.0, 0),
        (65.0, 20),
        (72.0, 50),
        (78.0, 90),
        (84.0, 130),
        (90.0, 160),
    ])
    .expect("default CPU curve is valid")
}

pub fn default_gpu_curve() -> Curve {
    Curve::new(&[
        (50.0, 0),
        (60.0, 20),
        (68.0, 60),
        (75.0, 100),
        (82.0, 140),
        (86.0, 160),
    ])
    .expect("default GPU curve is valid")
}

impl Default for SmartParams {
    fn default() -> Self {
        SmartParams {
            fast_tau_s: 4.0,
            slow_tau_s: 20.0,
            slope_tau_s: 6.0,
            lead_s: 6.0,
            max_lead_c: 4.0,
            hysteresis_c: 4.0,
            down_hold_s: 25.0,
            cpu_load_threshold_pct: 85.0,
            cpu_load_lead_c: 2.0,
            gpu_power_start_w: 45.0,
            gpu_power_full_w: 90.0,
            gpu_power_lead_c: 3.0,
            cross_coupling: 0.0,
            // boost above 160 has not been verified on this laptop
            noise_cap: crate::hw::MAX_VERIFIED_BOOST,
            cpu_curve: default_cpu_curve(),
            gpu_curve: default_gpu_curve(),
            steps: StepLimits::default(),
            guard: GuardParams::default(),
            sensor_grace_samples: 3,
            sensor_recover_samples: 5,
        }
    }
}

/// Plausible range for a laptop temperature sensor
pub const TEMP_VALID_MIN_C: f32 = 5.0;
pub const TEMP_VALID_MAX_C: f32 = 115.0;

pub fn temp_is_valid(t: f32) -> bool {
    t.is_finite() && (TEMP_VALID_MIN_C..=TEMP_VALID_MAX_C).contains(&t)
}

#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state", content = "count")]
pub enum SensorState {
    Ok,
    /// Last good value reused for this many samples
    Holding(u32),
    Lost,
}

/// Validity check with a short hold, like CoolerControl's missing status
/// threshold but much shorter, because losing the CPU sensor hands control
/// back to the firmware rather than guessing
#[derive(Clone, Debug, Default)]
pub struct SensorGate {
    last_good: Option<f32>,
    bad: u32,
    good_streak: u32,
    lost: bool,
}

impl SensorGate {
    pub fn feed(
        &mut self,
        raw: Option<f32>,
        grace: u32,
        recover: u32,
    ) -> (Option<f32>, SensorState) {
        match raw.filter(|t| temp_is_valid(*t)) {
            Some(t) => {
                self.bad = 0;
                self.good_streak = self.good_streak.saturating_add(1);
                self.last_good = Some(t);
                if self.lost && self.good_streak < recover {
                    return (None, SensorState::Lost);
                }
                self.lost = false;
                (Some(t), SensorState::Ok)
            }
            None => {
                self.bad = self.bad.saturating_add(1);
                self.good_streak = 0;
                if self.lost || self.bad > grace || self.last_good.is_none() {
                    self.lost = true;
                    (None, SensorState::Lost)
                } else {
                    (self.last_good, SensorState::Holding(self.bad))
                }
            }
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Demand just went up
    Rising,
    /// Temperature inside the hysteresis band, demand unchanged
    Holding,
    /// Lower demand allowed by temperature, waiting for the hold time
    WaitingToLower,
    /// Demand just went down
    Lowering,
}

/// Everything that went into one source's demand, for the status page and logs
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SourceDecision {
    pub raw_c: Option<f32>,
    pub sensor: SensorState,
    pub fast_c: Option<f32>,
    pub slow_c: Option<f32>,
    pub slope_c_per_s: f32,
    pub lead_c: f32,
    /// Temperature used for raising: fast EMA plus look ahead
    pub rise_temp_c: Option<f32>,
    /// Temperature used for lowering: slow EMA plus hysteresis
    pub fall_temp_c: Option<f32>,
    pub rise_demand: Boost,
    pub fall_demand: Boost,
    pub demand: Boost,
    pub phase: Phase,
    pub hold_remaining_s: f32,
    pub curve_segment: usize,
}

#[derive(Clone, Debug)]
pub struct SourceState {
    gate: SensorGate,
    fast: Ema,
    slow: Ema,
    slope: Slope,
    demand: Boost,
    demand_changed_at: f64,
}

impl SourceState {
    fn new(p: &SmartParams) -> Self {
        SourceState {
            gate: SensorGate::default(),
            fast: Ema::new(p.fast_tau_s),
            slow: Ema::new(p.slow_tau_s),
            slope: Slope::new(p.slope_tau_s),
            demand: Boost::ZERO,
            demand_changed_at: f64::NEG_INFINITY,
        }
    }

    fn reset_filters(&mut self) {
        self.fast.reset();
        self.slow.reset();
        self.slope.reset();
    }

    #[allow(clippy::too_many_arguments)]
    fn step(
        &mut self,
        p: &SmartParams,
        curve: &Curve,
        raw: Option<f32>,
        extra_lead_c: f32,
        now: f64,
        dt: f32,
    ) -> SourceDecision {
        let (value, sensor) = self
            .gate
            .feed(raw, p.sensor_grace_samples, p.sensor_recover_samples);
        let Some(t) = value else {
            self.reset_filters();
            return SourceDecision {
                raw_c: raw,
                sensor,
                fast_c: None,
                slow_c: None,
                slope_c_per_s: 0.0,
                lead_c: 0.0,
                rise_temp_c: None,
                fall_temp_c: None,
                rise_demand: Boost::ZERO,
                fall_demand: Boost::ZERO,
                demand: self.demand,
                phase: Phase::Holding,
                hold_remaining_s: 0.0,
                curve_segment: 0,
            };
        };
        let first = self.fast.value().is_none();
        let fast = self.fast.update(t, dt);
        let slow = self.slow.update(t, dt);
        let slope = self.slope.update(fast, dt);
        // Look ahead only on rising temperatures; a falling slope never lowers
        // the fan early, the slow EMA and hold time handle that
        let lead = (slope.max(0.0) * p.lead_s).min(p.max_lead_c) + extra_lead_c.max(0.0);
        // The fast EMA lags a real jump; the raw value is used when it is
        // higher so smoothing never hides a hot reading on the way up
        let rise_temp = fast.max(t) + lead;
        let fall_temp = slow + p.hysteresis_c;
        let rise_demand = curve.eval(rise_temp);
        let fall_demand = curve.eval(fall_temp);

        let since = (now - self.demand_changed_at) as f32;
        let mut hold_remaining = 0.0;
        // the first sample after a (re)start seeds the demand directly
        let phase = if first || rise_demand > self.demand {
            self.demand = rise_demand;
            self.demand_changed_at = now;
            Phase::Rising
        } else if fall_demand < self.demand {
            if since >= p.down_hold_s {
                self.demand = fall_demand;
                self.demand_changed_at = now;
                Phase::Lowering
            } else {
                hold_remaining = p.down_hold_s - since;
                Phase::WaitingToLower
            }
        } else {
            Phase::Holding
        };

        SourceDecision {
            raw_c: raw,
            sensor,
            fast_c: Some(fast),
            slow_c: Some(slow),
            slope_c_per_s: slope,
            lead_c: lead,
            rise_temp_c: Some(rise_temp),
            fall_temp_c: Some(fall_temp),
            rise_demand,
            fall_demand,
            demand: self.demand,
            phase,
            hold_remaining_s: hold_remaining,
            curve_segment: curve.segment(rise_temp),
        }
    }
}

/// Critical temperature latch, evaluated on raw temperatures
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Guard {
    pub engaged: bool,
    pub engaged_at: Option<f64>,
    pub reason: Option<String>,
}

impl Guard {
    pub fn update(
        &mut self,
        p: &GuardParams,
        cpu_raw: Option<f32>,
        gpu_raw: Option<f32>,
        now: f64,
    ) -> bool {
        let cpu = cpu_raw.filter(|t| temp_is_valid(*t));
        let gpu = gpu_raw.filter(|t| temp_is_valid(*t));
        let cpu_trip = cpu.is_some_and(|t| t >= p.cpu_trip_c);
        let gpu_trip = gpu.is_some_and(|t| t >= p.gpu_trip_c);
        if cpu_trip || gpu_trip {
            if !self.engaged {
                self.engaged_at = Some(now);
            }
            self.engaged = true;
            self.reason = Some(match (cpu_trip, gpu_trip) {
                (true, true) => "CPU 与 GPU 均达到临界温度".into(),
                (true, false) => format!("CPU 达到 {:.0} °C", p.cpu_trip_c),
                _ => format!("GPU 达到 {:.0} °C", p.gpu_trip_c),
            });
            return true;
        }
        if self.engaged {
            // An unknown temperature leaves the latch as it is
            let cpu_clear = cpu.map(|t| t <= p.cpu_release_c);
            let gpu_clear = gpu.map(|t| t <= p.gpu_release_c);
            let held = self
                .engaged_at
                .is_some_and(|at| (now - at) as f32 >= p.min_hold_s);
            if cpu_clear == Some(true) && gpu_clear != Some(false) && held {
                self.engaged = false;
                self.engaged_at = None;
                self.reason = None;
            }
        }
        self.engaged
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FanDriver {
    /// The fan's own heat source asked for the most
    Own,
    /// The other source's demand, through cross coupling
    Coupled,
    /// The critical temperature guard
    Guard,
    /// Limited by the noise cap
    NoiseCap,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FanDecision {
    pub fan: Fan,
    pub target: Boost,
    pub driver: FanDriver,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SmartDecision {
    pub cpu: SourceDecision,
    pub gpu: SourceDecision,
    pub guard: Guard,
    /// None when control has to be handed back to the firmware
    pub fans: Option<[FanDecision; 2]>,
    pub fallback: Option<String>,
}

/// Extra inputs that may be missing on some systems
#[derive(Clone, Debug, Default)]
pub struct SmartInputs {
    pub cpu_temp_c: Option<f32>,
    pub gpu_temp_c: Option<f32>,
    pub cpu_load_pct: Option<f32>,
    pub gpu_power_w: Option<f32>,
}

#[derive(Clone, Debug)]
pub struct SmartEngine {
    params: SmartParams,
    cpu: SourceState,
    gpu: SourceState,
    guard: Guard,
    last_now: Option<f64>,
}

impl SmartEngine {
    pub fn new(params: SmartParams) -> Self {
        SmartEngine {
            cpu: SourceState::new(&params),
            gpu: SourceState::new(&params),
            guard: Guard::default(),
            last_now: None,
            params,
        }
    }

    pub fn params(&self) -> &SmartParams {
        &self.params
    }

    pub fn set_params(&mut self, params: SmartParams) {
        *self = SmartEngine::new(params);
    }

    pub fn reset(&mut self) {
        *self = SmartEngine::new(self.params.clone());
    }

    pub fn step(&mut self, input: &SmartInputs, now: f64) -> SmartDecision {
        let dt = match self.last_now {
            Some(prev) => (now - prev).max(0.0) as f32,
            None => 0.0,
        };
        self.last_now = Some(now);
        let p = &self.params;

        let cpu_lead = match input.cpu_load_pct {
            Some(l) if l >= p.cpu_load_threshold_pct => p.cpu_load_lead_c,
            _ => 0.0,
        };
        let gpu_lead = match input.gpu_power_w {
            Some(w) if w > p.gpu_power_start_w && p.gpu_power_full_w > p.gpu_power_start_w => {
                ((w - p.gpu_power_start_w) / (p.gpu_power_full_w - p.gpu_power_start_w)).min(1.0)
                    * p.gpu_power_lead_c
            }
            _ => 0.0,
        };

        let cpu = self
            .cpu
            .step(p, &p.cpu_curve, input.cpu_temp_c, cpu_lead, now, dt);
        let gpu = self
            .gpu
            .step(p, &p.gpu_curve, input.gpu_temp_c, gpu_lead, now, dt);
        let guard_on = self
            .guard
            .update(&p.guard, input.cpu_temp_c, input.gpu_temp_c, now);

        let lost: Vec<&str> = [
            (cpu.sensor == SensorState::Lost).then_some("CPU"),
            (gpu.sensor == SensorState::Lost).then_some("GPU"),
        ]
        .into_iter()
        .flatten()
        .collect();

        if !lost.is_empty() && !guard_on {
            return SmartDecision {
                cpu,
                gpu,
                guard: self.guard.clone(),
                fans: None,
                fallback: Some(format!("{} 温度无效，已交还固件控制", lost.join("、"))),
            };
        }

        let fan = |fan: Fan, own: Boost, other: Boost| -> FanDecision {
            if guard_on {
                return FanDecision {
                    fan,
                    target: Boost::new(p.guard.boost.min(crate::hw::MAX_VERIFIED_BOOST)),
                    driver: FanDriver::Guard,
                };
            }
            let coupled =
                Boost::from_f32_clamped(f32::from(other.get()) * p.cross_coupling.clamp(0.0, 1.0));
            let (mut target, mut driver) = if coupled > own {
                (coupled, FanDriver::Coupled)
            } else {
                (own, FanDriver::Own)
            };
            if target.get() > p.noise_cap {
                target = Boost::new(p.noise_cap);
                driver = FanDriver::NoiseCap;
            }
            FanDecision {
                fan,
                target,
                driver,
            }
        };
        let fans = [
            fan(Fan::Cpu, cpu.demand, gpu.demand),
            fan(Fan::Gpu, gpu.demand, cpu.demand),
        ];
        SmartDecision {
            cpu,
            gpu,
            guard: self.guard.clone(),
            fans: Some(fans),
            fallback: None,
        }
    }
}

/// Decides whether and what to write for one fan
#[derive(Clone, Debug, Default)]
pub struct OutputStage {
    pub last_written: Option<Boost>,
    last_write_at: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputPlan {
    /// Nothing to do
    Unchanged,
    /// Difference below the minimum step
    BelowMinStep,
    /// Waiting for the minimum write interval
    WaitingInterval,
    Write(Boost),
}

impl OutputStage {
    /// Forget what was written, e.g. after the firmware took over
    pub fn forget(&mut self) {
        self.last_written = None;
        self.last_write_at = None;
    }

    pub fn note_written(&mut self, b: Boost, now: f64) {
        self.last_written = Some(b);
        self.last_write_at = Some(now);
    }

    /// `readback` is the value currently reported by the driver; `bypass`
    /// skips all limits (guard and hand back)
    pub fn plan(
        &self,
        limits: &StepLimits,
        target: Boost,
        readback: Option<Boost>,
        now: f64,
        bypass: bool,
    ) -> OutputPlan {
        let Some(last) = self.last_written.or(readback) else {
            return OutputPlan::Write(target);
        };
        if self.last_written.is_none() && readback == Some(target) {
            return OutputPlan::Unchanged;
        }
        if target == last {
            return OutputPlan::Unchanged;
        }
        if bypass {
            return OutputPlan::Write(target);
        }
        let since = self
            .last_write_at
            .map(|t| (now - t) as f32)
            .unwrap_or(f32::INFINITY);
        if target > last {
            let diff = target.get() - last.get();
            if diff < limits.inc_min && target != Boost::MAX {
                return OutputPlan::BelowMinStep;
            }
            if since < limits.inc_interval_s {
                return OutputPlan::WaitingInterval;
            }
            OutputPlan::Write(last.saturating_add(diff.min(limits.inc_max)))
        } else {
            let diff = last.get() - target.get();
            if diff < limits.dec_min && target != Boost::ZERO {
                return OutputPlan::BelowMinStep;
            }
            if since < limits.dec_interval_s {
                return OutputPlan::WaitingInterval;
            }
            OutputPlan::Write(last.saturating_sub(diff.min(limits.dec_max)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(cpu: f32, gpu: f32) -> SmartInputs {
        SmartInputs {
            cpu_temp_c: Some(cpu),
            gpu_temp_c: Some(gpu),
            ..Default::default()
        }
    }

    fn run(engine: &mut SmartEngine, temps: &[(f32, f32)], start: f64) -> Vec<SmartDecision> {
        temps
            .iter()
            .enumerate()
            .map(|(i, &(c, g))| engine.step(&input(c, g), start + i as f64))
            .collect()
    }

    fn cpu_target(d: &SmartDecision) -> u8 {
        d.fans.as_ref().unwrap()[0].target.get()
    }

    fn gpu_target(d: &SmartDecision) -> u8 {
        d.fans.as_ref().unwrap()[1].target.get()
    }

    #[test]
    fn rising_temperature_raises_boost_quickly() {
        let mut e = SmartEngine::new(SmartParams::default());
        let temps: Vec<_> = (0..40).map(|i| (60.0 + i as f32 * 0.5, 45.0)).collect();
        let d = run(&mut e, &temps, 0.0);
        let mut prev = 0;
        for x in &d {
            assert!(
                cpu_target(x) >= prev,
                "a rising temperature never lowers the demand"
            );
            prev = cpu_target(x);
        }
        // at 79.5 °C raw the curve asks for ~100; look ahead puts it higher
        assert!(prev >= 110, "{prev}");
        assert_eq!(d.last().unwrap().cpu.phase, Phase::Rising);
    }

    #[test]
    fn lowering_waits_for_hold_and_hysteresis() {
        let mut e = SmartEngine::new(SmartParams::default());
        let mut temps = vec![(82.0, 45.0); 60];
        temps.extend(vec![(70.0, 45.0); 120]);
        let d = run(&mut e, &temps, 0.0);
        let high = cpu_target(&d[59]);
        // a few seconds after the drop nothing has changed yet
        assert_eq!(cpu_target(&d[65]), high);
        assert!(matches!(
            d[65].cpu.phase,
            Phase::WaitingToLower | Phase::Holding
        ));
        // eventually it comes down
        let low = cpu_target(d.last().unwrap());
        assert!(low < high, "{low} < {high}");
        assert_eq!(
            low,
            SmartParams::default()
                .cpu_curve
                .eval(70.0 + 4.0)
                .get()
                .max(low)
        );
    }

    #[test]
    fn short_spike_does_not_drive_the_fan_down_and_up() {
        let mut e = SmartEngine::new(SmartParams::default());
        let mut temps = vec![(70.0, 45.0); 60];
        temps.push((88.0, 45.0));
        temps.push((88.0, 45.0));
        temps.extend(vec![(70.0, 45.0); 15]);
        let d = run(&mut e, &temps, 0.0);
        let changes = d
            .windows(2)
            .filter(|w| cpu_target(&w[0]) != cpu_target(&w[1]))
            .count();
        // one step up for the spike, held afterwards, no oscillation
        assert!(changes <= 2, "{changes}");
        let after = cpu_target(d.last().unwrap());
        assert!(after >= cpu_target(&d[59]));
    }

    #[test]
    fn wobbling_around_a_threshold_does_not_flap() {
        let mut e = SmartEngine::new(SmartParams::default());
        let temps: Vec<_> = (0..300)
            .map(|i| (if i % 2 == 0 { 74.0 } else { 76.0 }, 45.0))
            .collect();
        let d = run(&mut e, &temps, 0.0);
        let tail = &d[60..];
        let changes = tail
            .windows(2)
            .filter(|w| cpu_target(&w[0]) != cpu_target(&w[1]))
            .count();
        assert_eq!(changes, 0, "steady oscillation must not move the demand");
    }

    #[test]
    fn sources_are_independent() {
        let mut e = SmartEngine::new(SmartParams::default());
        // CPU busy, GPU idle
        let d = run(&mut e, &vec![(85.0, 45.0); 30], 0.0);
        assert!(cpu_target(d.last().unwrap()) > 120);
        assert_eq!(gpu_target(d.last().unwrap()), 0);
        // GPU busy, CPU idle
        let mut e = SmartEngine::new(SmartParams::default());
        let d = run(&mut e, &vec![(55.0, 80.0); 30], 0.0);
        assert_eq!(cpu_target(d.last().unwrap()), 0);
        assert!(gpu_target(d.last().unwrap()) > 120);
        // both busy
        let mut e = SmartEngine::new(SmartParams::default());
        let d = run(&mut e, &vec![(85.0, 80.0); 30], 0.0);
        assert!(cpu_target(d.last().unwrap()) > 120 && gpu_target(d.last().unwrap()) > 120);
    }

    #[test]
    fn cross_coupling_is_off_by_default_and_works_when_enabled() {
        let p = SmartParams {
            cross_coupling: 0.5,
            ..Default::default()
        };
        let mut e = SmartEngine::new(p);
        let d = run(&mut e, &vec![(55.0, 82.0); 30], 0.0);
        let last = d.last().unwrap();
        assert_eq!(last.fans.as_ref().unwrap()[0].driver, FanDriver::Coupled);
        assert!(cpu_target(last) > 60);
    }

    #[test]
    fn gpu_power_looks_ahead() {
        let base = |w: Option<f32>| {
            let mut e = SmartEngine::new(SmartParams::default());
            let mut last = None;
            for i in 0..20 {
                last = Some(e.step(
                    &SmartInputs {
                        cpu_temp_c: Some(55.0),
                        gpu_temp_c: Some(66.0),
                        gpu_power_w: w,
                        ..Default::default()
                    },
                    i as f64,
                ));
            }
            gpu_target(&last.unwrap())
        };
        assert!(base(Some(90.0)) > base(Some(20.0)));
        assert_eq!(base(None), base(Some(20.0)));
    }

    #[test]
    fn invalid_sensor_hands_back_after_grace() {
        let mut e = SmartEngine::new(SmartParams::default());
        run(&mut e, &[(75.0, 60.0); 10], 0.0);
        let mut d = Vec::new();
        for i in 0..6 {
            d.push(e.step(
                &SmartInputs {
                    cpu_temp_c: None,
                    gpu_temp_c: Some(60.0),
                    ..Default::default()
                },
                10.0 + i as f64,
            ));
        }
        assert!(
            d[0].fans.is_some(),
            "a single bad sample holds the last value"
        );
        assert_eq!(d[0].cpu.sensor, SensorState::Holding(1));
        assert!(d[5].fans.is_none());
        assert!(d[5].fallback.as_ref().unwrap().contains("CPU"));
        // implausible values count as invalid too
        let mut e = SmartEngine::new(SmartParams::default());
        run(&mut e, &[(75.0, 60.0); 3], 0.0);
        let mut last = None;
        for i in 0..5 {
            last = Some(e.step(&input(75.0, 255.0), 3.0 + i as f64));
        }
        assert!(last.unwrap().fans.is_none());
    }

    #[test]
    fn control_resumes_only_after_stable_readings() {
        let mut e = SmartEngine::new(SmartParams::default());
        let mut t = 0.0;
        let mut step = |e: &mut SmartEngine, c: Option<f32>| {
            t += 1.0;
            e.step(
                &SmartInputs {
                    cpu_temp_c: c,
                    gpu_temp_c: Some(55.0),
                    ..Default::default()
                },
                t,
            )
        };
        for _ in 0..5 {
            step(&mut e, Some(70.0));
        }
        for _ in 0..5 {
            step(&mut e, None);
        }
        for _ in 0..4 {
            assert!(step(&mut e, Some(70.0)).fans.is_none());
        }
        assert!(step(&mut e, Some(70.0)).fans.is_some());
    }

    #[test]
    fn guard_overrides_noise_cap_and_releases_with_margin() {
        let p = SmartParams {
            noise_cap: 100,
            ..Default::default()
        };
        let mut e = SmartEngine::new(p);
        let d = e.step(&input(97.0, 50.0), 0.0);
        // the guard uses the highest verified boost, above the noise cap
        assert_eq!(cpu_target(&d), 160);
        assert_eq!(gpu_target(&d), 160);
        assert_eq!(d.fans.as_ref().unwrap()[0].driver, FanDriver::Guard);
        // below trip but above release: still engaged
        assert!(e.step(&input(93.0, 50.0), 40.0).guard.engaged);
        // below release but before the minimum hold: still engaged
        let mut e2 = SmartEngine::new(SmartParams::default());
        e2.step(&input(97.0, 50.0), 0.0);
        assert!(e2.step(&input(80.0, 50.0), 10.0).guard.engaged);
        assert!(!e2.step(&input(80.0, 50.0), 31.0).guard.engaged);
    }

    #[test]
    fn smart_mode_never_asks_for_more_than_the_verified_ceiling() {
        let mut e = SmartEngine::new(SmartParams::default());
        let temps: Vec<_> = (0..200)
            .map(|i| (60.0 + i as f32 * 0.2, 55.0 + i as f32 * 0.2))
            .collect();
        for d in run(&mut e, &temps, 0.0) {
            assert!(cpu_target(&d) <= 160 && gpu_target(&d) <= 160);
        }
    }

    #[test]
    fn normal_firmware_operating_temperatures_do_not_trip_the_guard() {
        // measured on the G15 5515 with the Steam client open: CPU 91 to 93 °C
        let mut e = SmartEngine::new(SmartParams::default());
        let temps: Vec<_> = (0..300).map(|i| (91.0 + (i % 3) as f32, 73.0)).collect();
        let d = run(&mut e, &temps, 0.0);
        assert!(d.iter().all(|x| !x.guard.engaged));
    }

    #[test]
    fn guard_uses_raw_temperature_not_the_average() {
        let mut e = SmartEngine::new(SmartParams::default());
        run(&mut e, &vec![(60.0, 50.0); 60], 0.0);
        let d = e.step(&input(98.0, 50.0), 60.0);
        assert!(d.guard.engaged);
    }

    #[test]
    fn output_stage_limits_steps_and_rate() {
        let l = StepLimits::default();
        let mut o = OutputStage::default();
        assert_eq!(
            o.plan(&l, Boost::new(40), Some(Boost::ZERO), 0.0, false),
            OutputPlan::Write(Boost::new(40))
        );
        o.note_written(Boost::new(40), 0.0);
        assert_eq!(
            o.plan(&l, Boost::new(44), None, 5.0, false),
            OutputPlan::BelowMinStep
        );
        assert_eq!(
            o.plan(&l, Boost::new(200), None, 1.0, false),
            OutputPlan::WaitingInterval
        );
        assert_eq!(
            o.plan(&l, Boost::new(200), None, 3.0, false),
            OutputPlan::Write(Boost::new(136))
        );
        o.note_written(Boost::new(136), 3.0);
        assert_eq!(
            o.plan(&l, Boost::new(0), None, 6.0, false),
            OutputPlan::WaitingInterval
        );
        assert_eq!(
            o.plan(&l, Boost::new(0), None, 12.0, false),
            OutputPlan::Write(Boost::new(112))
        );
        // extremes bypass the minimum step
        o.note_written(Boost::new(250), 20.0);
        assert_eq!(
            o.plan(&l, Boost::MAX, None, 23.0, false),
            OutputPlan::Write(Boost::MAX)
        );
        o.note_written(Boost::new(4), 30.0);
        assert_eq!(
            o.plan(&l, Boost::ZERO, None, 40.0, false),
            OutputPlan::Write(Boost::ZERO)
        );
        // bypass ignores limits
        o.note_written(Boost::new(10), 50.0);
        assert_eq!(
            o.plan(&l, Boost::MAX, None, 50.1, true),
            OutputPlan::Write(Boost::MAX)
        );
    }

    #[test]
    fn output_stage_adopts_matching_readback_without_writing() {
        let o = OutputStage::default();
        assert_eq!(
            o.plan(
                &StepLimits::default(),
                Boost::ZERO,
                Some(Boost::ZERO),
                0.0,
                false
            ),
            OutputPlan::Unchanged
        );
    }
}
