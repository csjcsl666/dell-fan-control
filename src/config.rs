//! User settings of the background service

use crate::control::Mode;
use crate::control::smart::SmartParams;
use crate::hw::Boost;
use crate::state::write_json_atomic;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 2: guard thresholds recalibrated after the first hardware run; smart
/// parameters of older files are default values (there is no editor yet) and
/// are replaced
/// 3: boost ceiling 160 for curves, noise cap and guard
pub const CONFIG_VERSION: u32 = 3;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub version: u32,
    /// Master switch; off on first start
    pub enabled: bool,
    pub mode: Mode,
    /// Custom mode boost values, CPU then GPU
    pub custom: [Boost; 2],
    pub smart: SmartParams,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            version: CONFIG_VERSION,
            enabled: false,
            mode: Mode::Balanced,
            custom: [Boost::ZERO; 2],
            smart: SmartParams::default(),
        }
    }
}

pub fn default_config_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("XDG_CONFIG_HOME").filter(|d| !d.is_empty()) {
        return PathBuf::from(d).join("dell-fan-control");
    }
    let home = std::env::var_os("HOME").unwrap_or_else(|| "/tmp".into());
    PathBuf::from(home).join(".config/dell-fan-control")
}

#[derive(Debug)]
pub struct SettingsStore {
    path: PathBuf,
}

impl SettingsStore {
    pub fn new(dir: &Path) -> Self {
        SettingsStore {
            path: dir.join("config.json"),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Missing file means defaults; a broken file is reported and replaced by
    /// defaults (control stays off), never half applied
    pub fn load(&self) -> (Settings, Option<String>) {
        match std::fs::read(&self.path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Settings::default(), None),
            Err(e) => (
                Settings::default(),
                Some(format!("无法读取配置 {}：{e}", self.path.display())),
            ),
            Ok(bytes) => match serde_json::from_slice::<Settings>(&bytes) {
                Ok(mut s) => {
                    if s.version < CONFIG_VERSION {
                        log::info!(
                            "配置版本 {} 升级到 {CONFIG_VERSION}，智能参数恢复为新默认值",
                            s.version
                        );
                        s.smart = SmartParams::default();
                        s.version = CONFIG_VERSION;
                    }
                    (s, None)
                }
                Err(e) => {
                    let backup = self.path.with_extension("json.invalid");
                    let _ = std::fs::rename(&self.path, &backup);
                    (
                        Settings::default(),
                        Some(format!(
                            "配置文件无效，已改名为 {} 并使用默认值：{e}",
                            backup.display()
                        )),
                    )
                }
            },
        }
    }

    pub fn save(&self, s: &Settings) -> std::io::Result<()> {
        write_json_atomic(&self.path, s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_start_is_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        let (s, err) = SettingsStore::new(tmp.path()).load();
        assert!(!s.enabled);
        assert!(err.is_none());
    }

    #[test]
    fn round_trip_and_invalid_file() {
        let tmp = tempfile::tempdir().unwrap();
        let st = SettingsStore::new(tmp.path());
        let s = Settings {
            enabled: true,
            mode: Mode::Smart,
            custom: [Boost::new(10), Boost::new(200)],
            ..Default::default()
        };
        st.save(&s).unwrap();
        assert_eq!(st.load().0, s);
        std::fs::write(st.path(), b"{ not json").unwrap();
        let (d, err) = st.load();
        assert!(!d.enabled);
        assert!(err.is_some());
    }

    #[test]
    fn version_1_smart_parameters_are_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let st = SettingsStore::new(tmp.path());
        let mut old = Settings {
            version: 1,
            enabled: true,
            mode: Mode::Custom,
            ..Default::default()
        };
        old.smart.guard.cpu_trip_c = 92.0;
        st.save(&old).unwrap();
        let (s, _) = st.load();
        assert_eq!(s.version, CONFIG_VERSION);
        assert!(s.enabled, "user choices are kept");
        assert_eq!(s.mode, Mode::Custom);
        assert_eq!(
            s.smart.guard.cpu_trip_c,
            SmartParams::default().guard.cpu_trip_c
        );
    }

    #[test]
    fn unknown_boost_values_are_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let st = SettingsStore::new(tmp.path());
        std::fs::create_dir_all(tmp.path()).unwrap();
        std::fs::write(st.path(), br#"{"enabled":true,"custom":[10,300]}"#).unwrap();
        let (s, err) = st.load();
        assert!(!s.enabled, "an out of range value must not enable control");
        assert!(err.is_some());
    }
}
