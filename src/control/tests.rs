//! Controller scenarios against the simulated laptop. None of these touch
//! real hardware or power-profiles-daemon

use super::*;
use crate::hw::mock::{SharedLaptop, SimulatedLaptop};
use crate::profile::SharedProfile;

struct Rig {
    hw: SharedLaptop,
    profile: SharedProfile,
    dir: tempfile::TempDir,
    c: Controller,
    now: f64,
}

impl Rig {
    fn new() -> Self {
        Self::with(SimulatedLaptop::default())
    }

    fn with(m: SimulatedLaptop) -> Self {
        let hw = SharedLaptop::new(m);
        let profile = SharedProfile::default();
        let dir = tempfile::tempdir().unwrap();
        let c = Self::controller(&hw, &profile, &dir);
        Rig {
            hw,
            profile,
            dir,
            c,
            now: 0.0,
        }
    }

    fn controller(hw: &SharedLaptop, p: &SharedProfile, dir: &tempfile::TempDir) -> Controller {
        Controller::new(
            Box::new(hw.clone()),
            Box::new(p.clone()),
            MarkerStore::open(dir.path()),
            false,
            Mode::Balanced,
            [Boost::ZERO; 2],
            SmartParams::default(),
        )
    }

    fn tick(&mut self) -> ControlStatus {
        self.now += 1.0;
        self.hw.lock().step(1.0);
        self.c.tick(self.now, Extras::default()).clone()
    }

    fn ticks(&mut self, n: usize) -> ControlStatus {
        let mut s = self.tick();
        for _ in 1..n {
            s = self.tick();
        }
        s
    }

    fn boost(&self) -> [u8; 2] {
        let m = self.hw.lock();
        [m.boost[0].get(), m.boost[1].get()]
    }

    fn writes(&self) -> usize {
        self.hw.lock().writes.len()
    }

    fn profile(&self) -> String {
        self.profile.lock().current.clone()
    }
}

#[test]
fn disabled_by_default_and_never_writes() {
    let mut r = Rig::new();
    let s = r.ticks(30);
    assert_eq!(s.state, ControlState::Off);
    assert_eq!(r.writes(), 0);
    assert!(r.profile.lock().history.is_empty());
}

#[test]
fn switching_off_releases_boost_and_stops_writing() {
    let mut r = Rig::new();
    r.c.set_mode(Mode::Custom);
    r.c.set_custom(Fan::Cpu, Boost::new(150));
    r.c.set_custom(Fan::Gpu, Boost::new(90));
    r.c.set_enabled(true);
    r.ticks(3);
    assert_eq!(r.boost(), [150, 90]);
    assert!(!r.c.marker().is_empty());

    r.c.set_enabled(false);
    let s = r.tick();
    assert_eq!(s.state, ControlState::Off);
    assert_eq!(
        r.boost(),
        [0, 0],
        "switching off hands control back with boost 0"
    );
    assert!(r.c.marker().is_empty());
    let n = r.writes();
    r.ticks(60);
    assert_eq!(r.writes(), n, "no periodic writes once switched off");
}

#[test]
fn switching_off_does_not_touch_boost_this_program_never_set() {
    let mut r = Rig::new();
    // some other tool left a boost behind; we never wrote it
    r.hw.lock().boost[1] = Boost::new(60);
    r.c.set_enabled(true);
    r.ticks(5);
    r.c.set_enabled(false);
    r.ticks(5);
    assert_eq!(r.boost(), [0, 60]);
    assert_eq!(r.writes(), 0);
}

#[test]
fn performance_mode_uses_the_profile_and_restores_it() {
    let mut r = Rig::new();
    r.c.set_mode(Mode::Performance);
    r.c.set_enabled(true);
    let s = r.ticks(3);
    assert_eq!(s.state, ControlState::Firmware);
    assert_eq!(r.profile(), "performance");
    assert_eq!(
        r.writes(),
        0,
        "performance mode leaves boost to the firmware"
    );
    r.c.set_enabled(false);
    r.tick();
    assert_eq!(r.profile(), "balanced");
    assert!(r.c.marker().is_empty());
}

#[test]
fn a_profile_changed_by_someone_else_is_left_alone() {
    let mut r = Rig::new();
    r.c.set_mode(Mode::Performance);
    r.c.set_enabled(true);
    r.ticks(2);
    // the user picks power-saver in the desktop panel
    r.profile.lock().current = "power-saver".into();
    r.ticks(5);
    assert_eq!(
        r.profile(),
        "power-saver",
        "no fighting with ppd while enabled"
    );
    r.c.set_enabled(false);
    r.tick();
    assert_eq!(r.profile(), "power-saver");
    assert!(r.c.marker().is_empty());
}

#[test]
fn leaving_performance_for_custom_restores_the_original_profile() {
    let mut r = Rig::new();
    r.profile.lock().current = "power-saver".into();
    r.c.set_mode(Mode::Performance);
    r.c.set_enabled(true);
    r.ticks(2);
    r.c.set_mode(Mode::Custom);
    r.ticks(2);
    assert_eq!(r.profile(), "power-saver");
}

#[test]
fn custom_to_smart_and_smart_back_to_firmware() {
    let mut r = Rig::with(SimulatedLaptop {
        cpu_temp_c: 82.0,
        cpu_heat_w: 45.0,
        ..Default::default()
    });
    r.c.set_mode(Mode::Custom);
    r.c.set_custom(Fan::Cpu, Boost::new(30));
    r.c.set_enabled(true);
    r.ticks(3);
    assert_eq!(r.boost()[0], 30);

    r.c.set_mode(Mode::Smart);
    let s = r.ticks(10);
    assert_eq!(s.state, ControlState::Controlling);
    assert!(r.boost()[0] > 30, "smart mode reacts to 82 °C");
    assert!(s.smart.is_some());

    r.c.set_mode(Mode::Balanced);
    let s = r.ticks(3);
    assert_eq!(s.state, ControlState::Firmware);
    assert_eq!(r.boost(), [0, 0]);
    assert!(r.c.marker().boosted_fans.is_empty());
}

#[test]
fn smart_mode_closed_loop_cools_and_limits_writes() {
    let mut r = Rig::with(SimulatedLaptop {
        cpu_heat_w: 85.0,
        gpu_heat_w: 10.0,
        ..Default::default()
    });
    // firmware only
    r.ticks(1500);
    let firmware_temp = r.hw.lock().cpu_temp_c;
    r.c.set_mode(Mode::Smart);
    r.c.set_enabled(true);
    r.ticks(1500);
    let smart_temp = r.hw.lock().cpu_temp_c;
    assert!(
        firmware_temp > 75.0,
        "scenario must be hot enough: {firmware_temp}"
    );
    assert!(
        smart_temp < firmware_temp - 1.0,
        "{smart_temp} vs {firmware_temp}"
    );
    let writes_to_settle = r.writes();
    // steady state: very few writes in the last ten minutes
    let n = r.writes();
    r.ticks(600);
    println!(
        "closed loop: firmware only {firmware_temp:.1} C, smart {smart_temp:.1} C, boost {:?}, writes to settle {writes_to_settle}, writes in next 600 s {}",
        r.boost(),
        r.writes() - n
    );
    assert!(
        r.writes() - n <= 2,
        "{} writes in steady state",
        r.writes() - n
    );
}

#[test]
fn quitting_restores_everything_and_reports() {
    let mut r = Rig::new();
    r.c.set_mode(Mode::Custom);
    r.c.set_custom(Fan::Gpu, Boost::new(200));
    r.c.set_enabled(true);
    r.ticks(3);
    let report = r.c.shutdown();
    assert!(report.ok(), "{report:?}");
    assert_eq!(report.fans_released, vec![Fan::Gpu]);
    assert_eq!(r.boost(), [0, 0]);
    assert!(r.c.marker().is_empty());
}

#[test]
fn a_crashed_instance_is_cleaned_up_by_the_next_start() {
    let mut r = Rig::new();
    r.c.set_mode(Mode::Performance);
    r.c.set_enabled(true);
    r.ticks(2);
    r.c.set_mode(Mode::Custom);
    r.c.set_custom(Fan::Cpu, Boost::new(180));
    r.ticks(2);
    r.c.set_mode(Mode::Performance);
    r.ticks(1);
    r.c.set_mode(Mode::Custom);
    r.ticks(2);
    assert_eq!(r.boost()[0], 180);
    // simulate SIGKILL: the controller is dropped without any restore
    let marker_path = r.c.marker.path().to_path_buf();
    drop(std::mem::replace(
        &mut r.c,
        Rig::controller(
            &SharedLaptop::default(),
            &SharedProfile::default(),
            &tempfile::tempdir().unwrap(),
        ),
    ));
    assert!(marker_path.exists(), "the marker survives the crash");

    // a new process (restore command or new daemon) with the same state dir
    let mut fresh = Rig::controller(&r.hw, &r.profile, &r.dir);
    let report = fresh.recover_leftovers().expect("leftovers found");
    assert!(report.ok(), "{report:?}");
    assert_eq!(r.boost(), [0, 0]);
    assert!(!marker_path.exists());
}

#[test]
fn restore_without_marker_does_nothing() {
    let r = Rig::new();
    let mut fresh = Rig::controller(&r.hw, &r.profile, &r.dir);
    assert!(fresh.recover_leftovers().is_none());
    assert_eq!(r.writes(), 0);
}

#[test]
fn firmware_override_is_detected_and_the_fan_is_given_up() {
    let mut r = Rig::new();
    r.hw.lock().faults.firmware_resets_boost = true;
    r.c.set_mode(Mode::Custom);
    r.c.set_custom(Fan::Cpu, Boost::new(120));
    r.c.set_enabled(true);
    let s = r.ticks(30);
    let fan = &s.fans[0];
    assert!(fan.blocked.is_some(), "{fan:?}");
    assert!(s.events.iter().any(|e| e.text.contains("固件覆盖")));
    let n = r.writes();
    r.ticks(30);
    assert_eq!(r.writes(), n, "a blocked fan is no longer written");
}

#[test]
fn write_success_without_rpm_change_is_reported() {
    let mut r = Rig::with(SimulatedLaptop {
        cpu_temp_c: 60.0,
        ..Default::default()
    });
    r.hw.lock().faults.boost_ignored = true;
    r.c.set_mode(Mode::Custom);
    r.c.set_custom(Fan::Cpu, Boost::new(200));
    r.c.set_enabled(true);
    let s = r.ticks(15);
    assert_eq!(
        s.fans[0].readback,
        Some(Boost::new(200)),
        "read back matches"
    );
    assert_eq!(
        s.fans[0].response,
        Response::NoResponse,
        "but the fan did not react"
    );
    assert!(s.events.iter().any(|e| e.text.contains("未见明显响应")));
}

#[test]
fn a_responding_fan_is_reported_as_such() {
    let mut r = Rig::with(SimulatedLaptop {
        cpu_temp_c: 60.0,
        ..Default::default()
    });
    r.c.set_mode(Mode::Custom);
    r.c.set_custom(Fan::Cpu, Boost::new(200));
    r.c.set_enabled(true);
    let s = r.ticks(15);
    assert_eq!(s.fans[0].response, Response::Responding);
}

#[test]
fn write_failures_end_in_a_fault_and_restore_attempts() {
    let mut r = Rig::new();
    r.c.set_mode(Mode::Custom);
    r.c.set_custom(Fan::Cpu, Boost::new(100));
    r.c.set_enabled(true);
    r.hw.lock().faults.write_error = true;
    let s = r.ticks(40);
    assert!(matches!(s.state, ControlState::Fault(_)), "{:?}", s.state);
    // toggling clears the fault
    r.hw.lock().faults.write_error = false;
    r.c.set_enabled(false);
    r.c.set_enabled(true);
    let s = r.ticks(3);
    assert_eq!(s.state, ControlState::Controlling);
}

#[test]
fn missing_permission_is_reported_without_writing() {
    let mut r = Rig::new();
    r.hw.lock().faults.permission_denied = true;
    r.c.set_mode(Mode::Smart);
    r.c.set_enabled(true);
    r.hw.lock().cpu_temp_c = 85.0;
    let s = r.ticks(5);
    assert!(matches!(s.state, ControlState::NoPermission(_)));
    assert_eq!(r.writes(), 0);
}

#[test]
fn sensor_loss_in_smart_mode_releases_the_fans() {
    let mut r = Rig::with(SimulatedLaptop {
        cpu_temp_c: 84.0,
        ..Default::default()
    });
    r.c.set_mode(Mode::Smart);
    r.c.set_enabled(true);
    r.ticks(5);
    assert!(r.boost()[0] > 0);
    r.hw.lock().faults.sensors_missing = true;
    let s = r.ticks(6);
    assert!(
        matches!(s.state, ControlState::Fallback(_)),
        "{:?}",
        s.state
    );
    assert_eq!(r.boost(), [0, 0]);
    // sensors come back: control resumes after a few stable samples
    r.hw.lock().faults.sensors_missing = false;
    let s = r.ticks(8);
    assert_eq!(s.state, ControlState::Controlling);
}

#[test]
fn driver_disappearing_keeps_marker_for_later_restore() {
    let mut r = Rig::new();
    r.c.set_mode(Mode::Custom);
    r.c.set_custom(Fan::Cpu, Boost::new(100));
    r.c.set_enabled(true);
    r.ticks(2);
    r.hw.lock().faults.read_error = true;
    r.hw.lock().faults.write_error = true;
    r.c.set_enabled(false);
    let s = r.ticks(3);
    assert_eq!(s.state, ControlState::Restoring);
    assert!(
        !r.c.marker().is_empty(),
        "the record stays until restore succeeds"
    );
    r.hw.lock().faults = Default::default();
    let s = r.ticks(10);
    assert_eq!(s.state, ControlState::Off);
    assert_eq!(r.boost(), [0, 0]);
}

#[test]
fn custom_mode_guard_overrides_low_values() {
    let mut r = Rig::with(SimulatedLaptop {
        cpu_temp_c: 95.0,
        cpu_heat_w: 80.0,
        ..Default::default()
    });
    r.c.set_mode(Mode::Custom);
    r.c.set_custom(Fan::Cpu, Boost::new(10));
    r.c.set_enabled(true);
    let s = r.tick();
    assert!(s.guard.engaged);
    assert_eq!(r.boost(), [255, 255]);
}
