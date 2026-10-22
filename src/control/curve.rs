//! Monotonic piecewise linear temperature to boost curves

use crate::hw::Boost;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CurvePoint {
    pub temp_c: f32,
    pub boost: u8,
}

/// A validated curve: at least two points, strictly increasing temperatures
/// and non-decreasing boost, so a hotter reading can never ask for less
/// cooling than a cooler one
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "Vec<CurvePoint>", into = "Vec<CurvePoint>")]
pub struct Curve {
    points: Vec<CurvePoint>,
}

#[derive(Debug, PartialEq)]
pub enum CurveError {
    TooFewPoints,
    TempNotIncreasing,
    BoostDecreasing,
    TempOutOfRange,
}

impl std::fmt::Display for CurveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            CurveError::TooFewPoints => "曲线至少需要两个点",
            CurveError::TempNotIncreasing => "曲线温度必须严格递增",
            CurveError::BoostDecreasing => "曲线 boost 不能随温度降低",
            CurveError::TempOutOfRange => "曲线温度必须在 0 到 110 °C 之间",
        })
    }
}

impl TryFrom<Vec<CurvePoint>> for Curve {
    type Error = CurveError;

    fn try_from(points: Vec<CurvePoint>) -> Result<Self, Self::Error> {
        if points.len() < 2 {
            return Err(CurveError::TooFewPoints);
        }
        for p in &points {
            if !(0.0..=110.0).contains(&p.temp_c) || !p.temp_c.is_finite() {
                return Err(CurveError::TempOutOfRange);
            }
        }
        for w in points.windows(2) {
            if w[1].temp_c <= w[0].temp_c {
                return Err(CurveError::TempNotIncreasing);
            }
            if w[1].boost < w[0].boost {
                return Err(CurveError::BoostDecreasing);
            }
        }
        Ok(Curve { points })
    }
}

impl From<Curve> for Vec<CurvePoint> {
    fn from(c: Curve) -> Self {
        c.points
    }
}

impl Curve {
    pub fn new(points: &[(f32, u8)]) -> Result<Self, CurveError> {
        Curve::try_from(
            points
                .iter()
                .map(|&(temp_c, boost)| CurvePoint { temp_c, boost })
                .collect::<Vec<_>>(),
        )
    }

    pub fn points(&self) -> &[CurvePoint] {
        &self.points
    }

    /// Boost requested at `temp_c`, clamped to the end points
    pub fn eval(&self, temp_c: f32) -> Boost {
        let first = &self.points[0];
        let last = &self.points[self.points.len() - 1];
        if temp_c.is_nan() {
            return Boost::new(last.boost);
        }
        if temp_c <= first.temp_c {
            return Boost::new(first.boost);
        }
        if temp_c >= last.temp_c {
            return Boost::new(last.boost);
        }
        for w in self.points.windows(2) {
            let (a, b) = (&w[0], &w[1]);
            if temp_c <= b.temp_c {
                let t = (temp_c - a.temp_c) / (b.temp_c - a.temp_c);
                let v = f32::from(a.boost) + t * (f32::from(b.boost) - f32::from(a.boost));
                return Boost::from_f32_clamped(v);
            }
        }
        Boost::new(last.boost)
    }

    /// Index of the segment that `temp_c` falls into, for the status page
    pub fn segment(&self, temp_c: f32) -> usize {
        self.points
            .iter()
            .rposition(|p| temp_c >= p.temp_c)
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolates_and_clamps() {
        let c = Curve::new(&[(60.0, 0), (80.0, 200)]).unwrap();
        assert_eq!(c.eval(50.0), Boost::ZERO);
        assert_eq!(c.eval(70.0), Boost::new(100));
        assert_eq!(c.eval(95.0), Boost::new(200));
        assert_eq!(c.eval(f32::NAN), Boost::new(200));
    }

    #[test]
    fn rejects_non_monotonic_curves() {
        assert_eq!(
            Curve::new(&[(60.0, 100), (70.0, 50)]).unwrap_err(),
            CurveError::BoostDecreasing
        );
        assert_eq!(
            Curve::new(&[(60.0, 0), (60.0, 50)]).unwrap_err(),
            CurveError::TempNotIncreasing
        );
        assert_eq!(
            Curve::new(&[(60.0, 0)]).unwrap_err(),
            CurveError::TooFewPoints
        );
    }

    #[test]
    fn hotter_never_requests_less() {
        let c = Curve::new(&[(55.0, 0), (65.0, 30), (75.0, 90), (85.0, 200), (92.0, 255)]).unwrap();
        let mut prev = Boost::ZERO;
        let mut t = 30.0;
        while t < 100.0 {
            let b = c.eval(t);
            assert!(b >= prev);
            prev = b;
            t += 0.25;
        }
    }

    #[test]
    fn deserialisation_validates() {
        let bad = r#"[{"temp_c":70,"boost":10},{"temp_c":60,"boost":20}]"#;
        assert!(serde_json::from_str::<Curve>(bad).is_err());
    }
}
