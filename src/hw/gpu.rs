//! Optional NVIDIA readings through NVML
//!
//! NVML is loaded at runtime. When the library or the GPU is missing the
//! reader simply reports nothing; fan control never depends on these values
//! being present

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct GpuReading {
    pub power_w: Option<f32>,
    pub util_pct: Option<f32>,
    pub temp_c: Option<f32>,
    /// Performance state such as "P0" or "P8"
    pub pstate: Option<String>,
}

/// NVML names performance states as words ("Zero" .. "Fifteen")
pub fn pstate_label(debug_name: &str) -> String {
    const WORDS: [&str; 16] = [
        "Zero", "One", "Two", "Three", "Four", "Five", "Six", "Seven", "Eight", "Nine", "Ten",
        "Eleven", "Twelve", "Thirteen", "Fourteen", "Fifteen",
    ];
    match WORDS.iter().position(|w| *w == debug_name) {
        Some(n) => format!("P{n}"),
        None => debug_name.to_string(),
    }
}

#[cfg(feature = "nvml")]
pub struct GpuReader {
    nvml: Option<nvml_wrapper::Nvml>,
    failed_once: bool,
}

#[cfg(feature = "nvml")]
impl GpuReader {
    pub fn new() -> Self {
        let nvml = match nvml_wrapper::Nvml::init() {
            Ok(n) => Some(n),
            Err(e) => {
                log::info!("NVML 不可用，GPU 功耗与负载不显示：{e}");
                None
            }
        };
        GpuReader {
            nvml,
            failed_once: false,
        }
    }

    pub fn sample(&mut self) -> Option<GpuReading> {
        let nvml = self.nvml.as_ref()?;
        let dev = match nvml.device_by_index(0) {
            Ok(d) => d,
            Err(e) => {
                if !self.failed_once {
                    log::warn!("NVML 无法打开 GPU 0：{e}");
                    self.failed_once = true;
                }
                return None;
            }
        };
        use nvml_wrapper::enum_wrappers::device::TemperatureSensor;
        Some(GpuReading {
            power_w: dev.power_usage().ok().map(|mw| mw as f32 / 1000.0),
            util_pct: dev.utilization_rates().ok().map(|u| u.gpu as f32),
            temp_c: dev
                .temperature(TemperatureSensor::Gpu)
                .ok()
                .map(|t| t as f32),
            pstate: dev
                .performance_state()
                .ok()
                .map(|p| pstate_label(&format!("{p:?}"))),
        })
    }
}

#[cfg(not(feature = "nvml"))]
pub struct GpuReader;

#[cfg(not(feature = "nvml"))]
impl GpuReader {
    pub fn new() -> Self {
        GpuReader
    }

    pub fn sample(&mut self) -> Option<GpuReading> {
        None
    }
}

impl Default for GpuReader {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn pstate_names() {
        assert_eq!(super::pstate_label("Zero"), "P0");
        assert_eq!(super::pstate_label("Eight"), "P8");
        assert_eq!(super::pstate_label("Unknown"), "Unknown");
    }
}
