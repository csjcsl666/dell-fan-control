//! Time aware smoothing
//!
//! CoolerControl computes its EMA with `alpha = 2 / (period + 1)` per sample,
//! which assumes a fixed poll interval. The daemon here polls at different
//! rates (1 s while a window is open, slower otherwise), so the smoothing
//! factor is derived from the elapsed time instead:
//! `alpha = 1 - exp(-dt / tau)`. With a constant interval this is the usual
//! EMA; with a varying interval the response time in seconds stays the same

#[derive(Clone, Debug)]
pub struct Ema {
    tau_s: f32,
    value: Option<f32>,
}

impl Ema {
    pub fn new(tau_s: f32) -> Self {
        Ema {
            tau_s: tau_s.max(0.001),
            value: None,
        }
    }

    pub fn value(&self) -> Option<f32> {
        self.value
    }

    pub fn reset(&mut self) {
        self.value = None;
    }

    pub fn set_tau(&mut self, tau_s: f32) {
        self.tau_s = tau_s.max(0.001);
    }

    /// Feed one sample taken `dt_s` seconds after the previous one
    pub fn update(&mut self, sample: f32, dt_s: f32) -> f32 {
        let v = match self.value {
            None => sample,
            Some(prev) => {
                let dt = dt_s.max(0.0);
                let alpha = 1.0 - (-dt / self.tau_s).exp();
                prev + alpha * (sample - prev)
            }
        };
        self.value = Some(v);
        v
    }
}

/// Smoothed rate of change in degrees per second
#[derive(Clone, Debug)]
pub struct Slope {
    last: Option<f32>,
    ema: Ema,
}

impl Slope {
    pub fn new(tau_s: f32) -> Self {
        Slope {
            last: None,
            ema: Ema::new(tau_s),
        }
    }

    pub fn value(&self) -> f32 {
        self.ema.value().unwrap_or(0.0)
    }

    pub fn reset(&mut self) {
        self.last = None;
        self.ema.reset();
    }

    pub fn update(&mut self, x: f32, dt_s: f32) -> f32 {
        let raw = match self.last {
            Some(prev) if dt_s > 0.0 => (x - prev) / dt_s,
            _ => 0.0,
        };
        self.last = Some(x);
        self.ema.update(raw, dt_s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_sample_initialises() {
        let mut e = Ema::new(10.0);
        assert_eq!(e.update(50.0, 1.0), 50.0);
    }

    #[test]
    fn response_time_does_not_depend_on_poll_rate() {
        // after one time constant a step input has covered ~63 percent
        let run = |dt: f32| {
            let mut e = Ema::new(10.0);
            e.update(0.0, dt);
            let steps = (10.0 / dt).round() as usize;
            let mut v = 0.0;
            for _ in 0..steps {
                v = e.update(100.0, dt);
            }
            v
        };
        let a = run(1.0);
        let b = run(5.0);
        assert!((a - 63.2).abs() < 0.5, "{a}");
        assert!((b - 63.2).abs() < 0.5, "{b}");
    }

    #[test]
    fn slope_tracks_a_ramp() {
        let mut s = Slope::new(2.0);
        let mut v = 0.0;
        for i in 0..30 {
            v = s.update(50.0 + i as f32 * 0.5, 1.0);
        }
        assert!((v - 0.5).abs() < 0.02, "{v}");
    }
}
