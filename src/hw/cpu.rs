//! CPU utilisation from /proc/stat

use std::fs;

#[derive(Clone, Copy, Debug, Default)]
struct Times {
    busy: u64,
    total: u64,
}

fn parse_cpu_line(stat: &str) -> Option<Times> {
    let line = stat.lines().find(|l| l.starts_with("cpu "))?;
    let v: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|x| x.parse().ok())
        .collect();
    if v.len() < 4 {
        return None;
    }
    // user nice system idle iowait irq softirq steal
    let idle = v[3] + v.get(4).copied().unwrap_or(0);
    let total: u64 = v.iter().take(8).sum();
    Some(Times {
        busy: total.saturating_sub(idle),
        total,
    })
}

/// Tracks the previous sample so each call returns the load since the last one
#[derive(Debug, Default)]
pub struct CpuLoad {
    last: Option<Times>,
}

impl CpuLoad {
    pub fn sample(&mut self) -> Option<f32> {
        let stat = fs::read_to_string("/proc/stat").ok()?;
        self.sample_from(&stat)
    }

    pub fn sample_from(&mut self, stat: &str) -> Option<f32> {
        let now = parse_cpu_line(stat)?;
        let prev = self.last.replace(now)?;
        let dt = now.total.checked_sub(prev.total)?;
        if dt == 0 {
            return None;
        }
        let db = now.busy.saturating_sub(prev.busy);
        Some((db as f32 / dt as f32 * 100.0).clamp(0.0, 100.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_load_between_samples() {
        let mut c = CpuLoad::default();
        assert_eq!(c.sample_from("cpu  100 0 100 800 0 0 0 0 0 0\n"), None);
        let l = c.sample_from("cpu  150 0 150 900 0 0 0 0 0 0\n").unwrap();
        assert!((l - 50.0).abs() < 0.01, "{l}");
    }
}
