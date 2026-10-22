//! Persistent record of everything the program changed on the machine
//!
//! The marker is written *before* a non-zero boost or a profile change is
//! applied and removed only after the restore was verified. If the daemon is
//! killed, crashes or hangs, the marker survives; `dell-fan-control restore`
//! (run by systemd as ExecStopPost and at every daemon start) uses it to
//! undo exactly what this program did and nothing else

use crate::hw::Fan;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileChange {
    /// Profile that was active before this program changed it
    pub original: String,
    /// Profile this program selected
    pub ours: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppliedMarker {
    /// Fans whose boost this program set to a non-zero value
    pub boosted_fans: Vec<Fan>,
    pub profile: Option<ProfileChange>,
}

impl AppliedMarker {
    pub fn is_empty(&self) -> bool {
        self.boosted_fans.is_empty() && self.profile.is_none()
    }
}

pub fn default_state_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("XDG_STATE_HOME").filter(|d| !d.is_empty()) {
        return PathBuf::from(d).join("dell-fan-control");
    }
    let home = std::env::var_os("HOME").unwrap_or_else(|| "/tmp".into());
    PathBuf::from(home).join(".local/state/dell-fan-control")
}

/// Atomic JSON write: temporary file in the same directory, fsync, rename
pub fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".{}.tmp.{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("state"),
        std::process::id()
    ));
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(&serde_json::to_vec_pretty(value).map_err(std::io::Error::other)?)?;
        f.write_all(b"\n")?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)
}

#[derive(Debug)]
pub struct MarkerStore {
    path: PathBuf,
    current: AppliedMarker,
}

impl MarkerStore {
    pub fn open(dir: &Path) -> Self {
        let path = dir.join("applied.json");
        let current = fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        MarkerStore { path, current }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn get(&self) -> &AppliedMarker {
        &self.current
    }

    fn save(&self) -> std::io::Result<()> {
        if self.current.is_empty() {
            match fs::remove_file(&self.path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            }
        } else {
            write_json_atomic(&self.path, &self.current)
        }
    }

    pub fn add_fan(&mut self, fan: Fan) -> std::io::Result<()> {
        if !self.current.boosted_fans.contains(&fan) {
            self.current.boosted_fans.push(fan);
            self.save()?;
        }
        Ok(())
    }

    pub fn remove_fan(&mut self, fan: Fan) -> std::io::Result<()> {
        if self.current.boosted_fans.contains(&fan) {
            self.current.boosted_fans.retain(|f| *f != fan);
            self.save()?;
        }
        Ok(())
    }

    pub fn set_profile(&mut self, change: Option<ProfileChange>) -> std::io::Result<()> {
        if self.current.profile != change {
            self.current.profile = change;
            self.save()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_round_trip_and_removal() {
        let tmp = tempfile::tempdir().unwrap();
        let mut m = MarkerStore::open(tmp.path());
        assert!(m.get().is_empty());
        m.add_fan(Fan::Gpu).unwrap();
        m.set_profile(Some(ProfileChange {
            original: "balanced".into(),
            ours: "performance".into(),
        }))
        .unwrap();
        let again = MarkerStore::open(tmp.path());
        assert_eq!(again.get().boosted_fans, vec![Fan::Gpu]);
        m.remove_fan(Fan::Gpu).unwrap();
        m.set_profile(None).unwrap();
        assert!(!m.path().exists());
    }
}
