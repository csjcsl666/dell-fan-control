//! Real hardware backend on top of the alienware-wmi hwmon device
//!
//! Safety rules enforced here:
//! - only the hwmon device whose `name` is `alienware_wmi` is used
//! - the only files ever written are `fan<N>_boost` with N taken from
//!   discovery, never from a caller
//! - before every write the device identity is checked again, so a module
//!   reload that renumbers hwmon devices cannot redirect a write
//! - writes below a real `/sys` are refused unless the binary explicitly
//!   enabled them with [`allow_real_writes`], which tests never do

use super::{Boost, Fan, FanHardware, FanReading, HwError, SensorReadings, WriteAccess};
use std::ffi::CString;
use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

pub const HWMON_NAME: &str = "alienware_wmi";
pub const PLATFORM_PROFILE_NAME: &str = "alienware-wmi";

static REAL_WRITES_ALLOWED: AtomicBool = AtomicBool::new(false);

/// Called once by the binary for commands that are meant to control fans
pub fn allow_real_writes() {
    REAL_WRITES_ALLOWED.store(true, Ordering::SeqCst);
}

fn real_writes_allowed() -> bool {
    REAL_WRITES_ALLOWED.load(Ordering::SeqCst)
}

#[derive(Clone, Debug)]
struct FanAttrs {
    index: u8,
}

#[derive(Debug)]
pub struct AlienwareSysfs {
    root: PathBuf,
    hwmon_dir: PathBuf,
    /// Canonical path of the underlying device, used to detect renumbering
    device_identity: Option<PathBuf>,
    fans: [Option<FanAttrs>; 2],
    cpu_temp_index: Option<u8>,
    gpu_temp_index: Option<u8>,
}

fn read_trimmed(path: &Path) -> Result<String, HwError> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(s.trim().to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err(HwError::NotFound(path.display().to_string()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            Err(HwError::PermissionDenied(path.display().to_string()))
        }
        Err(e) => Err(HwError::Io(format!("{}: {e}", path.display()))),
    }
}

fn read_u32(path: &Path) -> Result<u32, HwError> {
    let s = read_trimmed(path)?;
    s.parse::<u32>()
        .map_err(|_| HwError::Parse(format!("{}: {s:?}", path.display())))
}

fn is_writable(path: &Path) -> bool {
    let Ok(c) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: c is a valid NUL terminated string for the duration of the call
    unsafe { libc::access(c.as_ptr(), libc::W_OK) == 0 }
}

impl AlienwareSysfs {
    /// Discover the device on the running system
    pub fn discover() -> Result<Self, HwError> {
        Self::discover_in(Path::new("/"))
    }

    /// Discover the device below an alternative root (used by tests)
    pub fn discover_in(root: &Path) -> Result<Self, HwError> {
        let class = root.join("sys/class/hwmon");
        let entries = fs::read_dir(&class)
            .map_err(|e| HwError::NotFound(format!("{}: {e}", class.display())))?;
        let mut found = None;
        for entry in entries.flatten() {
            let dir = entry.path();
            if read_trimmed(&dir.join("name")).ok().as_deref() == Some(HWMON_NAME) {
                found = Some(dir);
                break;
            }
        }
        let link = found.ok_or_else(|| {
            HwError::NotFound(format!(
                "没有找到 name 为 {HWMON_NAME} 的 hwmon 设备，alienware_wmi 驱动可能未加载"
            ))
        })?;
        let hwmon_dir =
            fs::canonicalize(&link).map_err(|e| HwError::Io(format!("{}: {e}", link.display())))?;
        let device_identity = fs::canonicalize(hwmon_dir.join("device")).ok();

        let mut fans: [Option<FanAttrs>; 2] = [None, None];
        let mut unlabeled = Vec::new();
        for n in 1u8..=6 {
            if !hwmon_dir.join(format!("fan{n}_input")).exists() {
                continue;
            }
            let label = read_trimmed(&hwmon_dir.join(format!("fan{n}_label")))
                .unwrap_or_default()
                .to_ascii_uppercase();
            let slot = if label.contains("CPU") {
                Some(Fan::Cpu)
            } else if label.contains("GPU") {
                Some(Fan::Gpu)
            } else {
                None
            };
            match slot {
                Some(f) if fans[f.index()].is_none() => {
                    fans[f.index()] = Some(FanAttrs { index: n })
                }
                _ => unlabeled.push(n),
            }
        }
        // Unlabelled fans fall back to the documented order: fan1 CPU, fan2 GPU
        for n in unlabeled {
            let f = if n == 1 { Fan::Cpu } else { Fan::Gpu };
            if fans[f.index()].is_none() {
                fans[f.index()] = Some(FanAttrs { index: n });
            }
        }

        let mut cpu_temp_index = None;
        let mut gpu_temp_index = None;
        for n in 1u8..=10 {
            if !hwmon_dir.join(format!("temp{n}_input")).exists() {
                continue;
            }
            let label = read_trimmed(&hwmon_dir.join(format!("temp{n}_label")))
                .unwrap_or_default()
                .to_ascii_uppercase();
            if label.contains("CPU") && cpu_temp_index.is_none() {
                cpu_temp_index = Some(n);
            } else if label.contains("GPU") && gpu_temp_index.is_none() {
                gpu_temp_index = Some(n);
            }
        }

        Ok(AlienwareSysfs {
            root: root.to_path_buf(),
            hwmon_dir,
            device_identity,
            fans,
            cpu_temp_index,
            gpu_temp_index,
        })
    }

    pub fn hwmon_dir(&self) -> &Path {
        &self.hwmon_dir
    }

    fn boost_path(&self, fan: Fan) -> Option<PathBuf> {
        self.fans[fan.index()]
            .as_ref()
            .map(|a| self.hwmon_dir.join(format!("fan{}_boost", a.index)))
    }

    fn read_temp(&self, index: Option<u8>) -> Option<f32> {
        let n = index?;
        let milli = read_u32(&self.hwmon_dir.join(format!("temp{n}_input"))).ok()?;
        Some(milli as f32 / 1000.0)
    }

    fn read_fan(&self, fan: Fan) -> FanReading {
        let Some(a) = self.fans[fan.index()].as_ref() else {
            return FanReading::default();
        };
        let p = |s: &str| self.hwmon_dir.join(format!("fan{}_{s}", a.index));
        FanReading {
            rpm: read_u32(&p("input")).ok(),
            min_rpm: read_u32(&p("min")).ok(),
            max_rpm: read_u32(&p("max")).ok(),
            boost: read_u32(&p("boost"))
                .ok()
                .and_then(|v| u8::try_from(v).ok())
                .map(Boost::new),
        }
    }

    /// Make sure the cached directory still is the same alienware_wmi device
    fn verify_identity(&self) -> Result<(), HwError> {
        let name = read_trimmed(&self.hwmon_dir.join("name"))?;
        if name != HWMON_NAME {
            return Err(HwError::DeviceChanged(format!(
                "{} 的 name 变为 {name}",
                self.hwmon_dir.display()
            )));
        }
        let now = fs::canonicalize(self.hwmon_dir.join("device")).ok();
        if now != self.device_identity {
            return Err(HwError::DeviceChanged(format!(
                "{} 对应的设备已变化",
                self.hwmon_dir.display()
            )));
        }
        Ok(())
    }

    fn writes_target_real_sysfs(&self) -> bool {
        self.root == Path::new("/") || self.hwmon_dir.starts_with("/sys")
    }
}

impl FanHardware for AlienwareSysfs {
    fn describe(&self) -> String {
        format!("alienware_wmi hwmon {}", self.hwmon_dir.display())
    }

    fn read(&mut self) -> Result<SensorReadings, HwError> {
        // The name file is read first so an unloaded driver is reported as an
        // error instead of a set of empty readings
        self.verify_identity()?;
        Ok(SensorReadings {
            cpu_temp_c: self.read_temp(self.cpu_temp_index),
            gpu_temp_c: self.read_temp(self.gpu_temp_index),
            fans: [self.read_fan(Fan::Cpu), self.read_fan(Fan::Gpu)],
        })
    }

    fn write_access(&self) -> WriteAccess {
        let paths: Vec<PathBuf> = Fan::ALL
            .iter()
            .filter_map(|f| self.boost_path(*f))
            .collect();
        if paths.is_empty() || paths.iter().any(|p| !p.exists()) {
            return WriteAccess::Unsupported;
        }
        if paths.iter().all(|p| is_writable(p)) {
            WriteAccess::Writable
        } else {
            WriteAccess::PermissionDenied
        }
    }

    fn write_boost(&mut self, fan: Fan, boost: Boost) -> Result<Option<Boost>, HwError> {
        let path = self
            .boost_path(fan)
            .ok_or_else(|| HwError::NotFound(format!("{fan} 风扇没有 boost 属性")))?;
        if self.writes_target_real_sysfs() && !real_writes_allowed() {
            return Err(HwError::PermissionDenied(
                "当前进程未启用真实硬件写入（测试或只读命令）".into(),
            ));
        }
        self.verify_identity()?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::PermissionDenied => {
                    HwError::PermissionDenied(path.display().to_string())
                }
                std::io::ErrorKind::NotFound => HwError::NotFound(path.display().to_string()),
                _ => HwError::Io(format!("{}: {e}", path.display())),
            })?;
        file.write_all(format!("{}\n", boost.get()).as_bytes())
            .map_err(|e| HwError::Io(format!("{}: {e}", path.display())))?;
        drop(file);
        Ok(read_u32(&path)
            .ok()
            .and_then(|v| u8::try_from(v).ok())
            .map(Boost::new))
    }
}

/// Read-only view of the kernel platform profile
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PlatformProfileInfo {
    /// Current value of /sys/firmware/acpi/platform_profile
    pub current: Option<String>,
    pub choices: Vec<String>,
    /// True when the alienware-wmi driver provides the profile
    pub provided_by_alienware: bool,
}

pub fn read_platform_profile() -> PlatformProfileInfo {
    read_platform_profile_in(Path::new("/"))
}

pub fn read_platform_profile_in(root: &Path) -> PlatformProfileInfo {
    let acpi = root.join("sys/firmware/acpi");
    let current = read_trimmed(&acpi.join("platform_profile")).ok();
    let choices = read_trimmed(&acpi.join("platform_profile_choices"))
        .map(|s| s.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default();
    let mut provided_by_alienware = false;
    if let Ok(entries) = fs::read_dir(root.join("sys/class/platform-profile")) {
        for e in entries.flatten() {
            if read_trimmed(&e.path().join("name")).ok().as_deref() == Some(PLATFORM_PROFILE_NAME) {
                provided_by_alienware = true;
            }
        }
    }
    PlatformProfileInfo {
        current,
        choices,
        provided_by_alienware,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn fake_tree(dir: &Path) {
        let hw = dir.join("sys/devices/platform/wmi/hwmon/hwmon7");
        fs::create_dir_all(&hw).unwrap();
        let dev = dir.join("sys/devices/platform/wmi");
        std::os::unix::fs::symlink(&dev, hw.join("device")).unwrap();
        let files = [
            ("name", "alienware_wmi"),
            ("fan1_input", "2000"),
            ("fan1_label", "CPU Fan"),
            ("fan1_min", "0"),
            ("fan1_max", "4200"),
            ("fan1_boost", "0"),
            ("fan2_input", "1900"),
            ("fan2_label", "GPU Fan"),
            ("fan2_min", "0"),
            ("fan2_max", "3900"),
            ("fan2_boost", "0"),
            ("temp1_input", "71000"),
            ("temp1_label", "CPU"),
            ("temp2_input", "54000"),
            ("temp2_label", "GPU"),
        ];
        for (n, v) in files {
            fs::write(hw.join(n), format!("{v}\n")).unwrap();
        }
        let class = dir.join("sys/class/hwmon");
        fs::create_dir_all(&class).unwrap();
        // an unrelated device first, to prove discovery picks by name
        let other = dir.join("sys/devices/other/hwmon/hwmon0");
        fs::create_dir_all(&other).unwrap();
        fs::write(other.join("name"), "k10temp\n").unwrap();
        std::os::unix::fs::symlink(&other, class.join("hwmon0")).unwrap();
        std::os::unix::fs::symlink(&hw, class.join("hwmon7")).unwrap();
        let acpi = dir.join("sys/firmware/acpi");
        fs::create_dir_all(&acpi).unwrap();
        fs::write(acpi.join("platform_profile"), "balanced\n").unwrap();
        fs::write(
            acpi.join("platform_profile_choices"),
            "balanced performance custom\n",
        )
        .unwrap();
        let pp = dir.join("sys/class/platform-profile/platform-profile-0");
        fs::create_dir_all(&pp).unwrap();
        fs::write(pp.join("name"), "alienware-wmi\n").unwrap();
    }

    #[test]
    fn discovers_by_name_and_reads_values() {
        let tmp = tempfile::tempdir().unwrap();
        fake_tree(tmp.path());
        let mut hw = AlienwareSysfs::discover_in(tmp.path()).unwrap();
        let r = hw.read().unwrap();
        assert_eq!(r.cpu_temp_c, Some(71.0));
        assert_eq!(r.gpu_temp_c, Some(54.0));
        assert_eq!(r.fan(Fan::Cpu).rpm, Some(2000));
        assert_eq!(r.fan(Fan::Gpu).max_rpm, Some(3900));
        assert_eq!(r.fan(Fan::Cpu).boost, Some(Boost::ZERO));
    }

    #[test]
    fn writes_only_the_boost_attribute_inside_the_test_root() {
        let tmp = tempfile::tempdir().unwrap();
        fake_tree(tmp.path());
        let mut hw = AlienwareSysfs::discover_in(tmp.path()).unwrap();
        assert!(!hw.writes_target_real_sysfs());
        let back = hw.write_boost(Fan::Gpu, Boost::new(128)).unwrap();
        assert_eq!(back, Some(Boost::new(128)));
        let p = hw.hwmon_dir().join("fan2_boost");
        assert_eq!(fs::read_to_string(p).unwrap().trim(), "128");
        assert_eq!(
            fs::read_to_string(hw.hwmon_dir().join("fan1_boost"))
                .unwrap()
                .trim(),
            "0"
        );
    }

    #[test]
    fn refuses_to_write_after_the_device_changed() {
        let tmp = tempfile::tempdir().unwrap();
        fake_tree(tmp.path());
        let mut hw = AlienwareSysfs::discover_in(tmp.path()).unwrap();
        fs::write(hw.hwmon_dir().join("name"), "nvme\n").unwrap();
        assert!(matches!(
            hw.write_boost(Fan::Cpu, Boost::new(10)),
            Err(HwError::DeviceChanged(_))
        ));
    }

    #[test]
    fn real_sysfs_writes_are_disabled_in_tests() {
        assert!(!real_writes_allowed());
    }

    #[test]
    fn reads_platform_profile() {
        let tmp = tempfile::tempdir().unwrap();
        fake_tree(tmp.path());
        let p = read_platform_profile_in(tmp.path());
        assert_eq!(p.current.as_deref(), Some("balanced"));
        assert_eq!(p.choices, vec!["balanced", "performance", "custom"]);
        assert!(p.provided_by_alienware);
    }
}
