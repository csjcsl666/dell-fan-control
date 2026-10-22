//! Power profile coordination through power-profiles-daemon
//!
//! The kernel platform profile is owned by power-profiles-daemon on this
//! system. Writing /sys/firmware/acpi/platform_profile directly would need
//! root and would fight with ppd, which rewrites it on its own changes. The
//! program therefore asks ppd over D-Bus, exactly as the desktop panel does,
//! and only on explicit mode changes, never periodically. polkit authorises
//! `switch-profile` for the active session user without a prompt; the call
//! is made without the interactive authorisation flag so it can never open a
//! password dialog

use std::time::Duration;

pub trait ProfileBackend: Send {
    fn describe(&self) -> String;
    fn get(&mut self) -> Result<String, String>;
    fn set(&mut self, profile: &str) -> Result<(), String>;
}

const PPD_NAMES: [(&str, &str, &str); 2] = [
    (
        "org.freedesktop.UPower.PowerProfiles",
        "/org/freedesktop/UPower/PowerProfiles",
        "org.freedesktop.UPower.PowerProfiles",
    ),
    (
        "net.hadess.PowerProfiles",
        "/net/hadess/PowerProfiles",
        "net.hadess.PowerProfiles",
    ),
];

pub struct PpdBackend {
    conn: Option<zbus::blocking::Connection>,
    names: Option<(&'static str, &'static str, &'static str)>,
}

impl Default for PpdBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl PpdBackend {
    pub fn new() -> Self {
        PpdBackend {
            conn: None,
            names: None,
        }
    }

    fn proxy(&mut self) -> Result<zbus::blocking::Proxy<'static>, String> {
        if self.conn.is_none() {
            let c = zbus::blocking::Connection::system()
                .map_err(|e| format!("无法连接系统 D-Bus：{e}"))?;
            self.conn = Some(c);
        }
        let conn = self.conn.as_ref().expect("connection set above");
        let candidates: Vec<_> = match self.names {
            Some(n) => vec![n],
            None => PPD_NAMES.to_vec(),
        };
        let mut last_err = String::from("power-profiles-daemon 不可用");
        for (dest, path, iface) in candidates {
            let proxy = zbus::blocking::proxy::Builder::<zbus::blocking::Proxy<'static>>::new(conn)
                .destination(dest)
                .and_then(|b| b.path(path))
                .and_then(|b| b.interface(iface))
                .map(|b| b.cache_properties(zbus::proxy::CacheProperties::No))
                .and_then(|b| b.build());
            match proxy {
                Ok(p) => {
                    if p.get_property::<String>("ActiveProfile").is_ok() {
                        self.names = Some((dest, path, iface));
                        return Ok(p);
                    }
                    last_err = format!("{dest} 无响应");
                }
                Err(e) => last_err = format!("{dest}：{e}"),
            }
        }
        // drop the connection so the next call reconnects, e.g. after ppd restarted
        self.conn = None;
        self.names = None;
        Err(last_err)
    }
}

impl ProfileBackend for PpdBackend {
    fn describe(&self) -> String {
        "power-profiles-daemon（D-Bus）".into()
    }

    fn get(&mut self) -> Result<String, String> {
        let p = self.proxy()?;
        p.get_property::<String>("ActiveProfile")
            .map_err(|e| format!("读取 ActiveProfile 失败：{e}"))
    }

    fn set(&mut self, profile: &str) -> Result<(), String> {
        if !matches!(profile, "performance" | "balanced" | "power-saver") {
            return Err(format!("拒绝未知档位 {profile:?}"));
        }
        let p = self.proxy()?;
        p.set_property("ActiveProfile", profile)
            .map_err(|e| format!("切换到 {profile} 失败：{e}"))?;
        // confirm the change instead of trusting the call
        for _ in 0..10 {
            if p.get_property::<String>("ActiveProfile").ok().as_deref() == Some(profile) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Err(format!("ppd 接受了请求，但读回的档位不是 {profile}"))
    }
}

/// In-memory backend for tests and `--simulate`
#[derive(Debug, Clone)]
pub struct FakeProfile {
    pub current: String,
    pub fail_set: bool,
    pub history: Vec<String>,
}

impl Default for FakeProfile {
    fn default() -> Self {
        FakeProfile {
            current: "balanced".into(),
            fail_set: false,
            history: Vec::new(),
        }
    }
}

impl ProfileBackend for FakeProfile {
    fn describe(&self) -> String {
        "模拟档位（不访问 power-profiles-daemon）".into()
    }

    fn get(&mut self) -> Result<String, String> {
        Ok(self.current.clone())
    }

    fn set(&mut self, profile: &str) -> Result<(), String> {
        if self.fail_set {
            return Err("模拟：切换失败".into());
        }
        self.current = profile.to_string();
        self.history.push(profile.to_string());
        Ok(())
    }
}

#[derive(Clone, Debug, Default)]
pub struct SharedProfile(pub std::sync::Arc<std::sync::Mutex<FakeProfile>>);

impl SharedProfile {
    pub fn lock(&self) -> std::sync::MutexGuard<'_, FakeProfile> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl ProfileBackend for SharedProfile {
    fn describe(&self) -> String {
        self.lock().describe()
    }
    fn get(&mut self) -> Result<String, String> {
        self.lock().get()
    }
    fn set(&mut self, profile: &str) -> Result<(), String> {
        self.lock().set(profile)
    }
}
