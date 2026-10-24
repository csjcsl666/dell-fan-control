//! Does the fan actually react to the boost we wrote?
//!
//! A single check twelve seconds after a write produced false alarms on the
//! G15 5515: the EC reports RPM in steps of 100, the firmware moves its own
//! base speed at the same time, and when control starts the firmware is
//! often already at its balanced ceiling (the driver's reported max). This
//! monitor therefore collects evidence instead of judging one sample:
//!
//! - step evidence: after an increase of at least `MIN_STEP`, any sample in
//!   the following `WINDOW_S` seconds at least `MIN_RISE_RPM` above the
//!   speed before the write
//! - ceiling evidence: with boost above zero the fan runs at least
//!   `ABOVE_CEILING_RPM` above the reported max, which the firmware does not
//!   reach on its own in the balanced profile (measured 2026-10-23)
//!
//! Only two consecutive step windows without any evidence mark the response
//! as unconfirmed, and any later evidence clears it again. Nothing is judged
//! during the first `STARTUP_S` seconds after control starts or the mode
//! changes. "Unconfirmed" is not "failed": failures are read back mismatches
//! and write errors, handled elsewhere

use crate::hw::Boost;
use serde::{Deserialize, Serialize};

pub const MIN_STEP: u8 = 40;
pub const WINDOW_S: f64 = 25.0;
pub const MIN_RISE_RPM: u32 = 150;
pub const ABOVE_CEILING_RPM: u32 = 200;
pub const STARTUP_S: f64 = 30.0;
pub const WINDOWS_BEFORE_UNCONFIRMED: u32 = 2;

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseState {
    /// Boost is zero or the fan is not controlled: nothing to check
    #[default]
    Idle,
    /// Control just started or a step is being watched
    Observing,
    /// The fan demonstrably reacts to boost
    Confirmed,
    /// Repeated steps showed no effect; not a failure, but worth showing
    Unconfirmed,
}

#[derive(Clone, Debug, Default)]
pub struct ResponseMonitor {
    state: ResponseState,
    started_at: Option<f64>,
    window: Option<(f64, u32)>,
    /// Highest speed seen during the window, kept when the window fails
    window_peak: Option<u32>,
    /// Speed level of the failed windows; rising clearly above it later is
    /// evidence too, so an unconfirmed fan can recover without a new step
    stuck_rpm: Option<u32>,
    misses: u32,
    recent_rpm: [Option<u32>; 3],
}

/// What changed this tick, so the caller can log transitions only
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Transition {
    None,
    BecameConfirmed,
    BecameUnconfirmed,
}

impl ResponseMonitor {
    pub fn state(&self) -> ResponseState {
        self.state
    }

    pub fn misses(&self) -> u32 {
        self.misses
    }

    /// Forget everything, e.g. when control stops or the mode changes
    pub fn reset(&mut self) {
        *self = ResponseMonitor::default();
    }

    fn baseline(&self) -> Option<u32> {
        self.recent_rpm.iter().flatten().copied().max()
    }

    /// Call when a value is written; `before` is the previous value and
    /// `rpm_before` the speed read in the same cycle before the write
    pub fn note_write(&mut self, before: Boost, after: Boost, now: f64, rpm_before: Option<u32>) {
        let base = match (self.baseline(), rpm_before) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        if after > before
            && after.get() - before.get() >= MIN_STEP
            && let Some(base) = base
        {
            self.window = Some((now, base));
            self.window_peak = None;
            if self.state != ResponseState::Unconfirmed {
                self.state = ResponseState::Observing;
            }
        }
    }

    /// Call once per tick with the current readings
    pub fn update(
        &mut self,
        now: f64,
        boost: Option<Boost>,
        rpm: Option<u32>,
        reported_max: Option<u32>,
    ) -> Transition {
        let boosted = boost.is_some_and(|b| b > Boost::ZERO);
        if !boosted {
            // a released fan has nothing to prove; any doubt goes with it
            self.reset();
            self.push_rpm(rpm);
            return Transition::None;
        }
        let started = *self.started_at.get_or_insert(now);
        let before = self.state;

        let ceiling_evidence =
            matches!((rpm, reported_max), (Some(r), Some(m)) if r >= m + ABOVE_CEILING_RPM);
        let step_evidence =
            matches!((self.window, rpm), (Some((_, base)), Some(r)) if r >= base + MIN_RISE_RPM);
        let recovery_evidence =
            matches!((self.stuck_rpm, rpm), (Some(s), Some(r)) if r >= s + MIN_RISE_RPM);
        if self.window.is_some()
            && let Some(r) = rpm
        {
            self.window_peak = Some(self.window_peak.map_or(r, |p| p.max(r)));
        }

        if ceiling_evidence || step_evidence || recovery_evidence {
            self.state = ResponseState::Confirmed;
            self.misses = 0;
            self.window = None;
            self.window_peak = None;
            self.stuck_rpm = None;
        } else if let Some((at, base)) = self.window
            && now - at >= WINDOW_S
        {
            self.window = None;
            let peak = self.window_peak.take().unwrap_or(base).max(base);
            self.stuck_rpm = Some(self.stuck_rpm.map_or(peak, |s| s.max(peak)));
            // a window without evidence only counts after the startup period,
            // and a single one is not enough to doubt the fan
            if now - started >= STARTUP_S {
                self.misses += 1;
                if self.misses >= WINDOWS_BEFORE_UNCONFIRMED {
                    self.state = ResponseState::Unconfirmed;
                }
            }
        } else if self.state == ResponseState::Idle {
            self.state = ResponseState::Observing;
        }
        self.push_rpm(rpm);

        match (before, self.state) {
            (ResponseState::Unconfirmed, ResponseState::Confirmed) => Transition::BecameConfirmed,
            (b, ResponseState::Unconfirmed) if b != ResponseState::Unconfirmed => {
                Transition::BecameUnconfirmed
            }
            _ => Transition::None,
        }
    }

    fn push_rpm(&mut self, rpm: Option<u32>) {
        self.recent_rpm.rotate_left(1);
        self.recent_rpm[2] = rpm;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(v: u8) -> Option<Boost> {
        Some(Boost::new(v))
    }

    /// Feed `n` seconds of constant readings starting at `t`
    fn feed(
        m: &mut ResponseMonitor,
        t: &mut f64,
        n: usize,
        boost: u8,
        rpm: u32,
    ) -> Vec<Transition> {
        (0..n)
            .map(|_| {
                *t += 1.0;
                m.update(*t, b(boost), Some(rpm), Some(4200))
            })
            .collect()
    }

    #[test]
    fn a_clear_rise_confirms() {
        let mut m = ResponseMonitor::default();
        let mut t = 0.0;
        feed(&mut m, &mut t, 3, 0, 3600);
        m.note_write(Boost::ZERO, Boost::new(128), t, None);
        feed(&mut m, &mut t, 3, 128, 3600);
        feed(&mut m, &mut t, 5, 128, 4300);
        assert_eq!(m.state(), ResponseState::Confirmed);
    }

    #[test]
    fn running_above_the_reported_max_confirms_without_a_step() {
        let mut m = ResponseMonitor::default();
        let mut t = 0.0;
        feed(&mut m, &mut t, 5, 155, 4600);
        assert_eq!(m.state(), ResponseState::Confirmed);
    }

    #[test]
    fn firmware_already_at_ceiling_is_not_an_immediate_alarm() {
        // the case seen in the field: firmware at 4200, boost written, the
        // speed needs time; a single slow window must not raise a warning
        let mut m = ResponseMonitor::default();
        let mut t = 0.0;
        feed(&mut m, &mut t, 3, 0, 4200);
        m.note_write(Boost::ZERO, Boost::new(96), t, None);
        let tr = feed(&mut m, &mut t, 40, 96, 4200);
        assert!(!tr.contains(&Transition::BecameUnconfirmed));
        assert_ne!(m.state(), ResponseState::Unconfirmed);
        // then the fan climbs above the ceiling
        feed(&mut m, &mut t, 3, 96, 4500);
        assert_eq!(m.state(), ResponseState::Confirmed);
    }

    #[test]
    fn repeated_steps_without_effect_become_unconfirmed_and_clear_later() {
        let mut m = ResponseMonitor::default();
        let mut t = 0.0;
        feed(&mut m, &mut t, 35, 10, 3600); // past the startup window
        m.note_write(Boost::new(10), Boost::new(60), t, None);
        feed(&mut m, &mut t, 26, 60, 3600);
        assert_ne!(
            m.state(),
            ResponseState::Unconfirmed,
            "one miss is not enough"
        );
        m.note_write(Boost::new(60), Boost::new(110), t, None);
        let tr = feed(&mut m, &mut t, 26, 110, 3600);
        assert!(tr.contains(&Transition::BecameUnconfirmed));
        assert_eq!(m.state(), ResponseState::Unconfirmed);
        // later evidence clears it
        let tr = feed(&mut m, &mut t, 2, 110, 4500);
        assert!(tr.contains(&Transition::BecameConfirmed));
        assert_eq!(m.state(), ResponseState::Confirmed);
    }

    #[test]
    fn startup_window_suppresses_judgement() {
        let mut m = ResponseMonitor::default();
        let mut t = 0.0;
        feed(&mut m, &mut t, 1, 10, 3600);
        for _ in 0..2 {
            m.note_write(Boost::new(10), Boost::new(60), t, None);
            feed(&mut m, &mut t, 13, 60, 3600);
        }
        assert_ne!(m.state(), ResponseState::Unconfirmed);
    }

    #[test]
    fn releasing_the_fan_resets_to_idle() {
        let mut m = ResponseMonitor::default();
        let mut t = 0.0;
        feed(&mut m, &mut t, 35, 10, 3600);
        for v in [60u8, 110] {
            m.note_write(Boost::new(v - 50), Boost::new(v), t, None);
            feed(&mut m, &mut t, 26, v, 3600);
        }
        assert_eq!(m.state(), ResponseState::Unconfirmed);
        feed(&mut m, &mut t, 1, 0, 3600);
        assert_eq!(m.state(), ResponseState::Idle);
    }

    #[test]
    fn small_steps_are_not_judged() {
        let mut m = ResponseMonitor::default();
        let mut t = 0.0;
        feed(&mut m, &mut t, 35, 100, 3600);
        for _ in 0..5 {
            m.note_write(Boost::new(100), Boost::new(110), t, None);
            feed(&mut m, &mut t, 26, 110, 3600);
        }
        assert_ne!(m.state(), ResponseState::Unconfirmed);
    }
}

#[cfg(test)]
mod recovery_tests {
    use super::*;

    #[test]
    fn first_write_uses_the_speed_read_before_it() {
        let mut m = ResponseMonitor::default();
        m.note_write(Boost::ZERO, Boost::new(150), 1.0, Some(2300));
        m.update(1.0, Some(Boost::new(150)), Some(2300), Some(4200));
        m.update(3.0, Some(Boost::new(150)), Some(3400), Some(4200));
        assert_eq!(m.state(), ResponseState::Confirmed);
    }

    #[test]
    fn an_unconfirmed_fan_recovers_when_speed_rises_later() {
        let mut m = ResponseMonitor::default();
        let mut t = 0.0;
        for _ in 0..35 {
            t += 1.0;
            m.update(t, Some(Boost::new(10)), Some(3000), Some(4200));
        }
        for v in [60u8, 110] {
            m.note_write(Boost::new(v - 50), Boost::new(v), t, Some(3000));
            for _ in 0..26 {
                t += 1.0;
                m.update(t, Some(Boost::new(v)), Some(3000), Some(4200));
            }
        }
        assert_eq!(m.state(), ResponseState::Unconfirmed);
        t += 1.0;
        assert_eq!(
            m.update(t, Some(Boost::new(110)), Some(3200), Some(4200)),
            Transition::BecameConfirmed
        );
    }
}
