//! Hardware access layer
//!
//! Everything that touches the machine goes through the [`FanHardware`] trait.
//! The real implementation ([`sysfs::AlienwareSysfs`]) only ever writes the
//! `fanN_boost` attributes of the `alienware_wmi` hwmon device. The simulated
//! implementation ([`mock::SimulatedLaptop`]) is used by tests and by
//! `--simulate` runs so that algorithms can be exercised without touching fans

pub mod cpu;
pub mod gpu;
pub mod mock;
pub mod sysfs;

use serde::{Deserialize, Serialize};
use std::fmt;

/// The two fans of a Dell G15 5515
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fan {
    Cpu,
    Gpu,
}

impl Fan {
    pub const ALL: [Fan; 2] = [Fan::Cpu, Fan::Gpu];

    pub fn index(self) -> usize {
        match self {
            Fan::Cpu => 0,
            Fan::Gpu => 1,
        }
    }

    pub fn label_zh(self) -> &'static str {
        match self {
            Fan::Cpu => "CPU 风扇",
            Fan::Gpu => "GPU 风扇",
        }
    }
}

impl fmt::Display for Fan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Fan::Cpu => "cpu",
            Fan::Gpu => "gpu",
        })
    }
}

/// A fan boost value as defined by the alienware-wmi driver
///
/// The kernel accepts integers from 0 to 255, which is exactly the range of
/// `u8`, so an invalid value cannot be represented. Boost is *not* a
/// percentage of maximum speed: the driver documents the approximate
/// behaviour `pwm = pwm_base + boost / 255 * (pwm_max - pwm_base)` where
/// `pwm_base` is whatever the firmware would have chosen on its own
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Boost(u8);

impl Boost {
    pub const ZERO: Boost = Boost(0);
    pub const MAX: Boost = Boost(255);

    pub const fn new(value: u8) -> Self {
        Boost(value)
    }

    pub const fn get(self) -> u8 {
        self.0
    }

    /// Share of the range between firmware base speed and maximum speed
    pub fn fraction(self) -> f32 {
        f32::from(self.0) / 255.0
    }

    pub fn saturating_add(self, delta: u8) -> Self {
        Boost(self.0.saturating_add(delta))
    }

    pub fn saturating_sub(self, delta: u8) -> Self {
        Boost(self.0.saturating_sub(delta))
    }

    pub fn from_f32_clamped(value: f32) -> Self {
        if value.is_nan() {
            return Boost::ZERO;
        }
        Boost(value.round().clamp(0.0, 255.0) as u8)
    }
}

impl fmt::Display for Boost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// One fan as reported by the hwmon device
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FanReading {
    pub rpm: Option<u32>,
    pub min_rpm: Option<u32>,
    pub max_rpm: Option<u32>,
    /// Value read back from `fanN_boost`
    pub boost: Option<Boost>,
}

/// Raw sensor readings from the fan hardware
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SensorReadings {
    pub cpu_temp_c: Option<f32>,
    pub gpu_temp_c: Option<f32>,
    pub fans: [FanReading; 2],
}

impl SensorReadings {
    pub fn fan(&self, fan: Fan) -> &FanReading {
        &self.fans[fan.index()]
    }
}

/// Whether the current process may write the boost attributes
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteAccess {
    /// Both boost attributes are writable by this user
    Writable,
    /// The attributes exist but are owned by root (udev rule not installed)
    PermissionDenied,
    /// No boost attributes were found
    Unsupported,
}

#[derive(Debug)]
pub enum HwError {
    NotFound(String),
    PermissionDenied(String),
    Io(String),
    Parse(String),
    /// The hwmon device behind a cached path changed (module reload)
    DeviceChanged(String),
}

impl HwError {
    pub fn code(&self) -> &'static str {
        match self {
            HwError::NotFound(_) => "HW_NOT_FOUND",
            HwError::PermissionDenied(_) => "HW_PERMISSION_DENIED",
            HwError::Io(_) => "HW_IO",
            HwError::Parse(_) => "HW_PARSE",
            HwError::DeviceChanged(_) => "HW_DEVICE_CHANGED",
        }
    }
}

impl fmt::Display for HwError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HwError::NotFound(m) => write!(f, "设备或属性不存在：{m}"),
            HwError::PermissionDenied(m) => write!(f, "没有写入权限：{m}"),
            HwError::Io(m) => write!(f, "读写失败：{m}"),
            HwError::Parse(m) => write!(f, "数据格式无法解析：{m}"),
            HwError::DeviceChanged(m) => write!(f, "硬件设备已变化：{m}"),
        }
    }
}

impl std::error::Error for HwError {}

/// Access to temperatures, fan speeds and fan boost
pub trait FanHardware: Send {
    /// Short human readable description, for logs and the status page
    fn describe(&self) -> String;

    fn read(&mut self) -> Result<SensorReadings, HwError>;

    fn write_access(&self) -> WriteAccess;

    /// Write a boost value and return the value read back afterwards
    fn write_boost(&mut self, fan: Fan, boost: Boost) -> Result<Option<Boost>, HwError>;

    /// True for the simulated backend, so the UI can never mistake it for
    /// real hardware
    fn is_simulated(&self) -> bool {
        false
    }
}

/// Placeholder when the alienware_wmi device could not be found, so the
/// service still starts and can report the problem
#[derive(Debug)]
pub struct Unavailable {
    pub reason: String,
}

impl FanHardware for Unavailable {
    fn describe(&self) -> String {
        format!("不可用：{}", self.reason)
    }

    fn read(&mut self) -> Result<SensorReadings, HwError> {
        Err(HwError::NotFound(self.reason.clone()))
    }

    fn write_access(&self) -> WriteAccess {
        WriteAccess::Unsupported
    }

    fn write_boost(&mut self, _fan: Fan, _boost: Boost) -> Result<Option<Boost>, HwError> {
        Err(HwError::NotFound(self.reason.clone()))
    }
}
