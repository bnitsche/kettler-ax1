//! Follows the cockpit's recovery (heart rate recovery) measurement from its statuses.
//!
//! Recorded on the AX1: after the button press the time reads `00;00` until a pulse is
//! there (without one the cockpit gives up after about 17 s), then counts down from
//! `00;59`. At `00;00` the pulse field holds the pulse drop P1 - P2 until the mode is left;
//! the fitness grade the cockpit shows is not sent and is computed here.

use std::time::Duration;

use serde::Serialize;

use crate::db::RecoverySource;
use crate::protocol::Status;

/// How long the server measures with a chest strap.
pub const STRAP_RECOVERY: Duration = Duration::from_secs(60);
/// P2 may be at most this old at the end of a strap measurement (and at most half of it).
const P2_MAX_AGE: Duration = Duration::from_secs(5);

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RecoveryView {
    /// Recovery mode, the countdown has not started (no pulse yet).
    Waiting,
    Measuring {
        remaining_secs: u32,
        pulse_start: Option<u16>,
        pulse: Option<u16>,
    },
    Result {
        /// P1: pulse at the start of the countdown.
        pulse_start: Option<u16>,
        /// P1 - P2 as reported by the cockpit in the pulse field.
        difference: u16,
        /// Fitness grade × 10 (10 = F1,0 very good … 60 = F6,0 insufficient).
        grade_x10: Option<u16>,
    },
    /// The strap measurement ended without a pulse for P2.
    Failed,
}

/// The manual's rule of thumb: grade = 6 - (10 × (P1 - P2) / P1)², limited to 1.0..=6.0.
fn grade_x10(p1: u16, difference: u16) -> u16 {
    let ratio = 10.0 * f64::from(difference.min(p1)) / f64::from(p1.max(1));
    ((6.0 - ratio * ratio).clamp(1.0, 6.0) * 10.0).round() as u16
}

/// A completed measurement with a known P1, reported once by `RecoveryTracker::update`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecoveryResult {
    pub pulse_start: u16,
    pub difference: u16,
    pub grade_x10: u16,
    pub source: RecoverySource,
}

#[derive(Default, Debug)]
pub struct RecoveryTracker {
    counting: bool,
    pulse_start: Option<u16>,
    pulse_last: Option<u16>,
    view: Option<RecoveryView>,
}

impl RecoveryTracker {
    /// Returns the result once, with the first status that shows it.
    pub fn update(&mut self, status: &Status) -> Option<RecoveryResult> {
        if !status.recovery {
            self.reset();
            return None;
        }
        let had_result = matches!(self.view, Some(RecoveryView::Result { .. }));
        let pulse = (status.pulse > 0).then_some(status.pulse);
        self.view = Some(if status.elapsed_secs > 0 {
            self.counting = true;
            if pulse.is_some() {
                self.pulse_start = self.pulse_start.or(pulse);
                self.pulse_last = pulse;
            }
            RecoveryView::Measuring {
                remaining_secs: status.elapsed_secs,
                pulse_start: self.pulse_start,
                pulse: pulse.or(self.pulse_last),
            }
        } else if self.counting {
            RecoveryView::Result {
                pulse_start: self.pulse_start,
                difference: status.pulse,
                grade_x10: self.pulse_start.map(|p1| grade_x10(p1, status.pulse)),
            }
        } else {
            RecoveryView::Waiting
        });
        match self.view {
            Some(RecoveryView::Result { pulse_start: Some(pulse_start), difference, grade_x10: Some(grade_x10) })
                if !had_result =>
            {
                Some(RecoveryResult { pulse_start, difference, grade_x10, source: RecoverySource::Cockpit })
            }
            _ => None,
        }
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn view(&self) -> Option<RecoveryView> {
        self.view
    }
}

/// The server's own measurement with a chest strap: P1 at the start, P2 after
/// `STRAP_RECOVERY`. A pulse that rose counts as no drop.
#[derive(Debug)]
pub struct StrapRecovery {
    duration: Duration,
    start: Option<(Duration, u16)>,
    last: Option<(Duration, u16)>,
    view: Option<RecoveryView>,
}

impl Default for StrapRecovery {
    fn default() -> Self {
        Self::with_duration(STRAP_RECOVERY)
    }
}

impl StrapRecovery {
    /// `STRAP_RECOVERY` outside of tests.
    pub fn with_duration(duration: Duration) -> Self {
        Self { duration, start: None, last: None, view: None }
    }

    /// Ignored while measuring.
    pub fn start(&mut self, at: Duration, p1: u16) {
        if self.is_measuring() {
            return;
        }
        self.start = Some((at, p1));
        self.last = Some((at, p1));
        self.view = Some(RecoveryView::Measuring {
            remaining_secs: self.duration.as_secs() as u32,
            pulse_start: Some(p1),
            pulse: Some(p1),
        });
    }

    pub fn is_measuring(&self) -> bool {
        self.start.is_some()
    }

    /// Call with each tick and status; `pulse` is the strap's current valid pulse.
    /// Returns the result once at the end; without P2 the view becomes `Failed`.
    pub fn update(&mut self, at: Duration, pulse: Option<u16>) -> Option<RecoveryResult> {
        let (started, p1) = self.start?;
        if let Some(p) = pulse {
            self.last = Some((at, p));
        }
        let end = started + self.duration;
        if at < end {
            self.view = Some(RecoveryView::Measuring {
                remaining_secs: (end - at).as_secs_f64().ceil() as u32,
                pulse_start: Some(p1),
                pulse: self.last.map(|(_, p)| p),
            });
            return None;
        }
        self.start = None;
        let max_age = P2_MAX_AGE.min(self.duration / 2);
        let p2 = self.last.take().filter(|&(when, _)| at.saturating_sub(when) <= max_age).map(|(_, p)| p);
        let Some(p2) = p2 else {
            self.view = Some(RecoveryView::Failed);
            return None;
        };
        let difference = p1.saturating_sub(p2);
        let grade = grade_x10(p1, difference);
        self.view = Some(RecoveryView::Result { pulse_start: Some(p1), difference, grade_x10: Some(grade) });
        Some(RecoveryResult { pulse_start: p1, difference, grade_x10: grade, source: RecoverySource::Strap })
    }

    /// Ends the measurement without a result.
    pub fn cancel(&mut self) {
        self.clear();
    }

    /// A result or failure stays until `clear`.
    pub fn view(&self) -> Option<RecoveryView> {
        self.view
    }

    pub fn clear(&mut self) {
        self.start = None;
        self.last = None;
        self.view = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::parse_status;

    fn feed(t: &mut RecoveryTracker, line: &str) -> Option<RecoveryView> {
        t.update(&parse_status(line).unwrap());
        t.view()
    }

    #[test]
    fn reports_result_once_and_only_with_p1() {
        let mut t = RecoveryTracker::default();
        let st = |line: &str| parse_status(line).unwrap();
        assert_eq!(t.update(&st("106	000	198	005	100	0028	00;59	000")), None);
        let done = st("006	000	198	005	100	0028	00;00	000");
        assert_eq!(t.update(&done), Some(RecoveryResult { pulse_start: 106, difference: 6, grade_x10: 57, source: RecoverySource::Cockpit }));
        assert_eq!(t.update(&done), None);
        // Without any pulse during the countdown there is no P1 and nothing to keep.
        t.reset();
        assert_eq!(t.update(&st("000	000	198	005	100	0028	00;30	000")), None);
        assert_eq!(t.update(&done), None);
    }

    #[test]
    fn follows_recorded_measurement() {
        let mut t = RecoveryTracker::default();
        assert_eq!(feed(&mut t, "000\t057\t202\t005\t100\t0022\t01:31\t100"), None);
        assert_eq!(feed(&mut t, "000\t000\t163\t005\t100\t0023\t00;00\t000"), Some(RecoveryView::Waiting));
        assert_eq!(
            feed(&mut t, "106\t050\t198\t005\t100\t0028\t00;59\t100"),
            Some(RecoveryView::Measuring { remaining_secs: 59, pulse_start: Some(106), pulse: Some(106) })
        );
        // A missing reading keeps the last pulse.
        assert_eq!(
            feed(&mut t, "000\t000\t198\t005\t100\t0028\t00;30\t000"),
            Some(RecoveryView::Measuring { remaining_secs: 30, pulse_start: Some(106), pulse: Some(106) })
        );
        feed(&mut t, "100\t000\t198\t005\t100\t0028\t00;01\t000");
        let result = Some(RecoveryView::Result { pulse_start: Some(106), difference: 6, grade_x10: Some(57) });
        assert_eq!(feed(&mut t, "006\t000\t198\t005\t100\t0028\t00;00\t000"), result);
        assert_eq!(feed(&mut t, "006\t000\t198\t005\t100\t0028\t00;00\t000"), result);
        assert_eq!(feed(&mut t, "000\t040\t198\t005\t100\t0028\t01:46\t040"), None);
        // A new measurement starts from scratch.
        assert_eq!(feed(&mut t, "000\t000\t163\t005\t100\t0023\t00;00\t000"), Some(RecoveryView::Waiting));
    }

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    #[test]
    fn strap_measurement_reports_once_after_60s() {
        let mut r = StrapRecovery::default();
        r.start(secs(0), 120);
        assert!(r.is_measuring());
        assert_eq!(r.update(secs(30), Some(110)), None);
        assert_eq!(r.view(), Some(RecoveryView::Measuring { remaining_secs: 30, pulse_start: Some(120), pulse: Some(110) }));
        let expected = RecoveryResult { pulse_start: 120, difference: 20, grade_x10: grade_x10(120, 20), source: RecoverySource::Strap };
        assert_eq!(r.update(secs(60), Some(100)), Some(expected));
        assert!(!r.is_measuring());
        assert_eq!(r.update(secs(61), Some(100)), None);
        let result = Some(RecoveryView::Result { pulse_start: Some(120), difference: 20, grade_x10: Some(grade_x10(120, 20)) });
        assert_eq!(r.view(), result);
        r.clear();
        assert_eq!(r.view(), None);
    }

    #[test]
    fn rising_pulse_gives_zero_difference() {
        let mut r = StrapRecovery::default();
        r.start(secs(0), 100);
        let result = r.update(secs(60), Some(110)).unwrap();
        assert_eq!((result.difference, result.grade_x10), (0, 60));
    }

    #[test]
    fn p2_may_be_a_few_seconds_old() {
        let mut r = StrapRecovery::default();
        r.start(secs(0), 120);
        r.update(secs(57), Some(101));
        assert_eq!(r.update(secs(60), None).map(|x| x.difference), Some(19));
        // Older than that is no P2.
        let mut r = StrapRecovery::default();
        r.start(secs(0), 120);
        r.update(secs(50), Some(101));
        assert_eq!(r.update(secs(60), None), None);
        assert_eq!(r.view(), Some(RecoveryView::Failed));
        assert!(!r.is_measuring());
    }

    #[test]
    fn short_measurement_needs_a_recent_p2() {
        let mut r = StrapRecovery::with_duration(secs(2));
        r.start(secs(0), 120);
        assert_eq!(r.update(secs(2), None), None);
        assert_eq!(r.view(), Some(RecoveryView::Failed));
    }

    #[test]
    fn missing_p2_fails() {
        let mut r = StrapRecovery::default();
        r.start(secs(0), 120);
        assert_eq!(r.update(secs(60), None), None);
        assert_eq!(r.view(), Some(RecoveryView::Failed));
    }

    #[test]
    fn cancel_reports_nothing() {
        let mut r = StrapRecovery::default();
        r.start(secs(0), 120);
        r.update(secs(20), Some(110));
        r.cancel();
        assert_eq!(r.view(), None);
        assert!(!r.is_measuring());
        assert_eq!(r.update(secs(60), Some(90)), None);
    }

    #[test]
    fn start_while_measuring_is_ignored() {
        let mut r = StrapRecovery::with_duration(secs(10));
        r.start(secs(0), 120);
        r.start(secs(5), 90);
        assert_eq!(r.update(secs(10), Some(100)).map(|x| x.pulse_start), Some(120));
    }

    #[test]
    fn serializes_failed() {
        assert_eq!(serde_json::to_string(&RecoveryView::Failed).unwrap(), r#"{"state":"failed"}"#);
    }

    #[test]
    fn cockpit_results_have_cockpit_source() {
        let mut t = RecoveryTracker::default();
        t.update(&parse_status("106	000	198	005	100	0028	00;59	000").unwrap());
        let done = t.update(&parse_status("006	000	198	005	100	0028	00;00	000").unwrap()).unwrap();
        assert_eq!(done.source, RecoverySource::Cockpit);
    }

    #[test]
    fn grade_follows_manual_formula() {
        assert_eq!(grade_x10(106, 6), 57); // 6 - 0.566² = 5.68
        assert_eq!(grade_x10(150, 30), 20); // 6 - 2² = 2.0
        assert_eq!(grade_x10(140, 0), 60);
        assert_eq!(grade_x10(150, 60), 10); // 6 - 4² is below the best grade
    }

    #[test]
    fn serializes_with_state_tag() {
        let v = RecoveryView::Measuring { remaining_secs: 42, pulse_start: Some(106), pulse: None };
        assert_eq!(
            serde_json::to_string(&v).unwrap(),
            r#"{"state":"measuring","remaining_secs":42,"pulse_start":106,"pulse":null}"#
        );
    }
}
