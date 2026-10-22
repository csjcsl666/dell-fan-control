//! Simulated laptop used by tests and by `--simulate`
//!
//! The model is deliberately simple: two thermal masses (CPU and GPU) that
//! exchange heat, a firmware fan curve that sets a base speed from
//! temperature, and boost applied on top of that base exactly as the kernel
//! documentation describes. It is good enough to check that the control loop
//! reacts in the right direction, settles, and limits writes; it says nothing
//! about the real laptop's temperatures

use super::{Boost, Fan, FanHardware, FanReading, HwError, SensorReadings, WriteAccess};
use std::time::Instant;

#[derive(Clone, Debug, Default)]
pub struct Faults {
    /// Temperatures are reported as missing
    pub sensors_missing: bool,
    /// The whole read fails, like an unloaded driver
    pub read_error: bool,
    /// Writes fail with an I/O error
    pub write_error: bool,
    /// Firmware silently resets boost to zero after every write
    pub firmware_resets_boost: bool,
    /// Writes are rejected with a permission error
    pub permission_denied: bool,
    /// Boost reads back fine but has no effect on the fan speed
    pub boost_ignored: bool,
}

#[derive(Debug)]
pub struct SimulatedLaptop {
    pub ambient_c: f32,
    pub cpu_temp_c: f32,
    pub gpu_temp_c: f32,
    pub cpu_heat_w: f32,
    pub gpu_heat_w: f32,
    pub max_rpm: [u32; 2],
    pub boost: [Boost; 2],
    pub faults: Faults,
    pub writes: Vec<(Fan, Boost)>,
    pub realtime: Option<Instant>,
}

impl Default for SimulatedLaptop {
    fn default() -> Self {
        SimulatedLaptop {
            ambient_c: 30.0,
            cpu_temp_c: 55.0,
            gpu_temp_c: 48.0,
            cpu_heat_w: 15.0,
            gpu_heat_w: 15.0,
            max_rpm: [4200, 3900],
            boost: [Boost::ZERO; 2],
            faults: Faults::default(),
            writes: Vec::new(),
            realtime: None,
        }
    }
}

impl SimulatedLaptop {
    /// A model that advances with wall clock time on every read, for `--simulate`
    pub fn realtime() -> Self {
        SimulatedLaptop {
            realtime: Some(Instant::now()),
            ..Default::default()
        }
    }

    /// Speed the firmware would choose on its own
    pub fn firmware_base_rpm(&self, fan: Fan) -> f32 {
        let t = match fan {
            Fan::Cpu => self.cpu_temp_c,
            Fan::Gpu => self.gpu_temp_c,
        };
        let max = self.max_rpm[fan.index()] as f32;
        if t < 45.0 {
            0.0
        } else {
            (1500.0 + (t - 45.0) / 50.0 * (max - 1500.0)).min(max)
        }
    }

    pub fn rpm(&self, fan: Fan) -> f32 {
        let base = self.firmware_base_rpm(fan);
        if self.faults.boost_ignored {
            return base;
        }
        let max = self.max_rpm[fan.index()] as f32;
        let base = base.max(if self.boost[fan.index()].get() > 0 {
            1500.0
        } else {
            base
        });
        base + self.boost[fan.index()].fraction() * (max - base)
    }

    /// Advance the thermal model by `dt` seconds
    pub fn step(&mut self, dt: f32) {
        let mut remaining = dt;
        while remaining > 0.0 {
            let h = remaining.min(0.25);
            remaining -= h;
            let g = |fan: Fan, s: &Self| {
                let r = s.rpm(fan) / s.max_rpm[fan.index()] as f32;
                0.45 + 1.6 * r
            };
            let gc = g(Fan::Cpu, self);
            let gg = g(Fan::Gpu, self);
            let k = 0.35;
            let c = 45.0;
            let dc = (self.cpu_heat_w - gc * (self.cpu_temp_c - self.ambient_c)
                + k * (self.gpu_temp_c - self.cpu_temp_c))
                / c;
            let dg = (self.gpu_heat_w - gg * (self.gpu_temp_c - self.ambient_c)
                + k * (self.cpu_temp_c - self.gpu_temp_c))
                / c;
            self.cpu_temp_c += dc * h;
            self.gpu_temp_c += dg * h;
        }
    }
}

impl FanHardware for SimulatedLaptop {
    fn describe(&self) -> String {
        "模拟笔记本（不访问真实硬件）".into()
    }

    fn read(&mut self) -> Result<SensorReadings, HwError> {
        if let Some(t) = self.realtime {
            let now = Instant::now();
            self.step(now.duration_since(t).as_secs_f32());
            self.realtime = Some(now);
        }
        if self.faults.read_error {
            return Err(HwError::NotFound("模拟：驱动未加载".into()));
        }
        let fan = |f: Fan, s: &Self| FanReading {
            rpm: Some(s.rpm(f).round() as u32),
            min_rpm: Some(0),
            max_rpm: Some(s.max_rpm[f.index()]),
            boost: Some(s.boost[f.index()]),
        };
        Ok(SensorReadings {
            cpu_temp_c: (!self.faults.sensors_missing).then_some(self.cpu_temp_c),
            gpu_temp_c: (!self.faults.sensors_missing).then_some(self.gpu_temp_c),
            fans: [fan(Fan::Cpu, self), fan(Fan::Gpu, self)],
        })
    }

    fn write_access(&self) -> WriteAccess {
        if self.faults.permission_denied {
            WriteAccess::PermissionDenied
        } else {
            WriteAccess::Writable
        }
    }

    fn write_boost(&mut self, fan: Fan, boost: Boost) -> Result<Option<Boost>, HwError> {
        if self.faults.permission_denied {
            return Err(HwError::PermissionDenied("模拟：无权限".into()));
        }
        if self.faults.write_error {
            return Err(HwError::Io("模拟：写入失败".into()));
        }
        self.writes.push((fan, boost));
        self.boost[fan.index()] = if self.faults.firmware_resets_boost {
            Boost::ZERO
        } else {
            boost
        };
        Ok(Some(self.boost[fan.index()]))
    }

    fn is_simulated(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boost_never_lowers_speed_below_firmware_base() {
        let mut m = SimulatedLaptop {
            cpu_temp_c: 80.0,
            ..Default::default()
        };
        let base = m.rpm(Fan::Cpu);
        m.boost[0] = Boost::new(128);
        assert!(m.rpm(Fan::Cpu) > base);
        m.boost[0] = Boost::MAX;
        assert!((m.rpm(Fan::Cpu) - 4200.0).abs() < 1.0);
    }

    #[test]
    fn more_boost_means_lower_steady_temperature() {
        let run = |b: u8| {
            let mut m = SimulatedLaptop {
                cpu_heat_w: 45.0,
                ..Default::default()
            };
            m.boost = [Boost::new(b); 2];
            m.step(1200.0);
            m.cpu_temp_c
        };
        assert!(run(255) < run(0) - 3.0);
    }
}

/// Shared handle so a test can inspect the simulated laptop while the
/// controller owns it
#[derive(Clone, Debug, Default)]
pub struct SharedLaptop(pub std::sync::Arc<std::sync::Mutex<SimulatedLaptop>>);

impl SharedLaptop {
    pub fn new(m: SimulatedLaptop) -> Self {
        SharedLaptop(std::sync::Arc::new(std::sync::Mutex::new(m)))
    }

    pub fn lock(&self) -> std::sync::MutexGuard<'_, SimulatedLaptop> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl FanHardware for SharedLaptop {
    fn describe(&self) -> String {
        self.lock().describe()
    }
    fn read(&mut self) -> Result<SensorReadings, HwError> {
        self.lock().read()
    }
    fn write_access(&self) -> WriteAccess {
        self.lock().write_access()
    }
    fn write_boost(&mut self, fan: Fan, boost: Boost) -> Result<Option<Boost>, HwError> {
        self.lock().write_boost(fan, boost)
    }
    fn is_simulated(&self) -> bool {
        true
    }
}
