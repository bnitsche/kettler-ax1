//! Modelled elevation profile from power.
//!
//! Riding at `flat_watts` is treated as flat ground; every watt above or below it climbs
//! or descends at `P / (m g)` metres per second with `m` = rider + bike. Gain is counted
//! with hysteresis so pedalling jitter around the flat power adds nothing.

use serde::Serialize;

use crate::program::{target_at, FlatSegment};

/// Assumed bike weight added to the rider's weight.
pub const BIKE_KG: f64 = 10.0;
/// Minimum swing in metres before a climb or a descent counts.
pub const GAIN_THRESHOLD_M: f64 = 1.0;
const GRAVITY: f64 = 9.81;
/// Seconds between preview points.
const PREVIEW_STEP_SECS: u32 = 10;

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct ElevationParams {
    pub weight_kg: f64,
    /// Power that corresponds to flat ground.
    pub flat_watts: u16,
}

impl ElevationParams {
    pub const DEFAULT: ElevationParams = ElevationParams { weight_kg: 75.0, flat_watts: 70 };

    /// Height change in metres for one second at `watts`.
    pub fn delta_m(&self, watts: f64) -> f64 {
        (watts - f64::from(self.flat_watts)) / ((self.weight_kg + BIKE_KG) * GRAVITY)
    }
}

/// Running height and hysteresis gain.
#[derive(Clone, Debug, Default)]
pub struct ElevationTrack {
    height: f64,
    gain: f64,
    climbing: bool,
    valley: f64,
    peak: f64,
}

impl ElevationTrack {
    /// Advances by one second at `watts`.
    pub fn step(&mut self, params: &ElevationParams, watts: f64) {
        self.height += params.delta_m(watts);
        let h = self.height;
        if self.climbing {
            if h > self.peak {
                self.gain += h - self.peak;
                self.peak = h;
            } else if h < self.peak - GAIN_THRESHOLD_M {
                self.climbing = false;
                self.valley = h;
            }
        } else if h < self.valley {
            self.valley = h;
        } else if h > self.valley + GAIN_THRESHOLD_M {
            self.climbing = true;
            self.gain += h - self.valley;
            self.peak = h;
        }
    }

    pub fn height_m(&self) -> f64 {
        self.height
    }

    pub fn gain_m(&self) -> f64 {
        self.gain
    }
}

/// The height after each second (same length as the input) and the gain.
pub fn heights(
    params: &ElevationParams,
    watts: impl IntoIterator<Item = f64>,
) -> (Vec<f64>, f64) {
    let mut track = ElevationTrack::default();
    let out = watts
        .into_iter()
        .map(|w| {
            track.step(params, w);
            track.height_m()
        })
        .collect();
    (out, track.gain_m())
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ElevationPreview {
    /// `(secs, height)` every 10 s plus the last second, starting with `(0, 0.0)`.
    pub points: Vec<(u32, f32)>,
    pub gain_m: f64,
}

/// Elevation a program would produce if ridden exactly on target.
pub fn preview(params: &ElevationParams, segments: &[FlatSegment]) -> ElevationPreview {
    // `expand` yields contiguous segments; a gap would count as flat ground, not as 0 W.
    let total = segments.last().map_or(0, |s| s.start_secs + s.duration_secs);
    let watts = (0..total).map(|t| target_at(segments, t).map_or(f64::from(params.flat_watts), |(_, w)| f64::from(w)));
    let (h, gain_m) = heights(params, watts);
    let mut points = vec![(0, 0.0f32)];
    for t in 1..=total {
        if t % PREVIEW_STEP_SECS == 0 || t == total {
            points.push((t, h[t as usize - 1] as f32));
        }
    }
    ElevationPreview { points, gain_m }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::program::templates;

    const P85: ElevationParams = ElevationParams { weight_kg: 85.0, flat_watts: 70 };

    fn template_preview(name: &str) -> ElevationPreview {
        let p = templates().into_iter().find(|p| p.name == name).unwrap();
        preview(&P85, &p.expand())
    }

    fn seg(start: u32, dur: u32, w: u16) -> FlatSegment {
        FlatSegment {
            name: String::new(),
            label: String::new(),
            start_secs: start,
            duration_secs: dur,
            start_watts: w,
            end_watts: w,
        }
    }

    #[test]
    fn preview_ends_at_the_total_when_not_a_multiple_of_the_step() {
        let pv = preview(&P85, &[seg(0, 25, 170)]);
        let ts: Vec<u32> = pv.points.iter().map(|p| p.0).collect();
        assert_eq!(ts, [0, 10, 20, 25]);
        assert!(pv.points[3].1 > pv.points[2].1);
    }

    #[test]
    fn preview_gap_counts_as_flat_ground() {
        let pv = preview(&P85, &[seg(0, 5, 70), seg(10, 5, 70)]);
        assert!(pv.points.iter().all(|p| p.1 == 0.0));
        assert_eq!(pv.gain_m, 0.0);
    }

    #[test]
    fn delta_matches_formula() {
        let d = P85.delta_m(170.0);
        assert!((d - 100.0 / (95.0 * 9.81)).abs() < 1e-9);
        assert!((d - 0.1073).abs() < 1e-4);
    }

    #[test]
    fn four_by_four_preview_gains_121_m() {
        let pv = template_preview("4 × 4");
        assert!((120.0..=122.0).contains(&pv.gain_m), "gain {}", pv.gain_m);
        let last = pv.points.last().unwrap().1;
        assert!((f64::from(last) - 120.4).abs() <= 1.0, "last {last}");
    }

    #[test]
    fn pyramid_preview_gains_62_m() {
        let pv = template_preview("Pyramide");
        assert!((61.0..=63.0).contains(&pv.gain_m), "gain {}", pv.gain_m);
    }

    #[test]
    fn jitter_around_flat_counts_no_gain() {
        let watts = (0..300).map(|i| if i % 2 == 0 { 65.0 } else { 75.0 });
        let (h, gain) = heights(&ElevationParams::DEFAULT, watts);
        assert_eq!(h.len(), 300);
        assert_eq!(gain, 0.0);
        assert!(h.last().unwrap().abs() < 0.1);
    }

    #[test]
    fn climb_then_descent() {
        let watts = std::iter::repeat_n(170.0, 120).chain(std::iter::repeat_n(0.0, 120));
        let (h, gain) = heights(&P85, watts);
        let peak = h[119];
        assert!((gain - peak).abs() < 1e-9);
        let drop = peak - h[239];
        assert!((drop - 120.0 * 70.0 / 932.0).abs() < 0.2, "drop {drop}");
    }

    #[test]
    fn small_climbs_below_threshold_are_ignored() {
        let watts = (0..600).map(|i| if (i / 30) % 2 == 0 { 90.0 } else { 50.0 });
        let (_, gain) = heights(&ElevationParams { weight_kg: 85.0, ..ElevationParams::DEFAULT }, watts);
        assert_eq!(gain, 0.0);
    }

    #[test]
    fn preview_points_every_10_s() {
        let p = templates().into_iter().find(|p| p.name == "Pyramide").unwrap();
        let pv = preview(&ElevationParams::DEFAULT, &p.expand());
        assert_eq!(pv.points[0], (0, 0.0));
        assert_eq!(pv.points[1].0, 10);
        assert_eq!(pv.points[2].0, 20);
        assert_eq!(pv.points.last().unwrap().0, p.total_secs());
        assert_eq!(pv.points.len(), 1 + 144);
    }
}
