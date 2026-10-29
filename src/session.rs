//! Training session state machine: pure, no I/O. Turns device statuses and user
//! commands into power commands and per-second samples.

use std::time::Duration;

use serde::Serialize;

use crate::program::{FlatSegment, target_at};
use crate::protocol::{DeviceLimits, Status};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Input {
    Connected,
    Disconnected,
    /// `at`: monotonic time since the session started.
    Status {
        status: Status,
        at: Duration,
    },
    /// Sent about once a second, so time-based rules also apply while the device is silent.
    Tick {
        at: Duration,
    },
    Adjust(i16),
    /// The server measures recovery with a chest strap: while `on`, the ride stays paused
    /// (pedalling, `Resume` and the automatic end do nothing). An active ride pauses.
    Hold {
        on: bool,
        at: Duration,
    },
    Pause,
    Resume,
    Stop,
}

#[derive(Debug, Default, PartialEq)]
pub struct Output {
    pub send_power: Option<u16>,
    pub sample: Option<Sample>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Sample {
    pub ride_secs: u32,
    pub status: Status,
    pub target_watts: u16,
    /// Wall clock (unix ms) when the sample was taken; set by the ride manager.
    pub at_unix_ms: Option<i64>,
    /// The chest strap's pulse in that second, if one counted; set by the ride manager.
    pub strap_pulse: Option<u16>,
}

impl Sample {
    /// The strap's pulse if above 0, else the cockpit's if above 0.
    pub fn effective_pulse(&self) -> Option<u16> {
        self.strap_pulse.filter(|&p| p > 0).or((self.status.pulse > 0).then_some(self.status.pulse))
    }
}

#[derive(Serialize, Clone, Copy, PartialEq, Debug)]
pub enum Phase {
    Waiting,
    Active,
    Paused,
    AutoPaused,
    Finished,
}

#[derive(Serialize, Clone, Debug)]
pub struct SessionView {
    pub phase: Phase,
    pub connected: bool,
    pub ride_secs: u32,
    pub program_secs: Option<u32>,
    pub segment_index: Option<usize>,
    pub segment_label: Option<String>,
    pub segment_remaining_secs: Option<u32>,
    pub segment_progress: f32,
    pub target_watts: u16,
    pub overridden: bool,
    /// See `Input::Hold`.
    pub held: bool,
    pub last: Option<Status>,
    pub distance_m: f64,
    pub energy_kj: f64,
}

pub struct Session {
    segments: Option<Vec<FlatSegment>>,
    limits: DeviceLimits,
    phase: Phase,
    connected: bool,
    last: Option<Status>,
    last_at: Option<Duration>,
    /// Last sign of the device in `Active`; reset on resume, set by the next status or tick.
    heard_at: Option<Duration>,
    zero_rpm_since: Option<Duration>,
    auto_paused_since: Option<Duration>,
    active_secs: f64,
    ride_secs: u32,
    distance_m: f64,
    energy_kj: f64,
    segment: Option<usize>,
    override_watts: Option<u16>,
    overridden: bool,
    /// Effective target (override or program) last decided on and sent.
    desired: u16,
    /// Target shown; differs from `desired` only after accepting the device's value.
    target_watts: u16,
    link: Option<Link>,
    /// Set when a device value differing from the wanted one was accepted: `desired`
    /// (Active) or the minimum (other phases) is retried every `RETRY_AFTER` from this
    /// time while the mismatch persists.
    retry_since: Option<Duration>,
    held: bool,
}

/// Last power value sent to the device and whether `ST` has confirmed it.
#[derive(Debug, Clone, Copy)]
struct Link {
    watts: u16,
    confirmed: bool,
    unconfirmed: u8,
    resent: bool,
}

const FREE_RIDE_WATTS: i32 = 50;
const MAX_STEP: Duration = Duration::from_secs(2);
/// Zero rpm this long in `Active` pauses automatically.
const AUTO_PAUSE_AFTER: Duration = Duration::from_secs(3);
/// No status at all this long in `Active` counts as a stop (the device reports about once a second).
const SILENCE_AFTER: Duration = Duration::from_secs(5);
/// Statuses without confirmation tolerated before resending, and again before accepting.
const CONFIRM_STATUSES: u8 = 3;
const RETRY_AFTER: Duration = Duration::from_secs(15);
/// A ride left in `AutoPaused`, or in `Waiting` since its start, this long is considered over.
const AUTO_FINISH_AFTER: Duration = Duration::from_secs(600);

impl Session {
    pub fn new(segments: Option<Vec<FlatSegment>>, limits: DeviceLimits) -> Self {
        let mut s = Self {
            segments,
            limits,
            phase: Phase::Waiting,
            connected: false,
            last: None,
            last_at: None,
            heard_at: None,
            zero_rpm_since: None,
            auto_paused_since: None,
            active_secs: 0.0,
            ride_secs: 0,
            distance_m: 0.0,
            energy_kj: 0.0,
            segment: None,
            override_watts: None,
            overridden: false,
            desired: limits.min,
            target_watts: limits.min,
            link: None,
            retry_since: None,
            held: false,
        };
        if let Some((segment, watts)) = s.program_target() {
            s.segment = segment;
            s.desired = watts;
            s.target_watts = watts;
        }
        s
    }

    pub fn handle(&mut self, input: Input) -> Output {
        if self.phase == Phase::Finished {
            return self.on_finished(input);
        }
        let send_power = match input {
            Input::Status { status, at } => return self.on_status(status, at),
            Input::Tick { at } => self.on_tick(at),
            Input::Connected => {
                self.connected = true;
                match self.phase {
                    Phase::Active => Some(self.send_target()),
                    Phase::Paused | Phase::AutoPaused => Some(self.send(self.limits.min)),
                    Phase::Waiting | Phase::Finished => None,
                }
            }
            Input::Disconnected => {
                self.connected = false;
                None
            }
            Input::Adjust(delta) if self.phase == Phase::Active => {
                let watts = i32::from(self.target_watts) + i32::from(delta);
                self.override_watts = Some(self.limits.clamp(watts));
                self.overridden = true;
                Some(self.send_target())
            }
            Input::Hold { on, at } => {
                self.held = on;
                (on && self.phase == Phase::Active).then(|| self.auto_pause(at))
            }
            Input::Pause if matches!(self.phase, Phase::Active | Phase::AutoPaused) => {
                self.phase = Phase::Paused;
                self.auto_paused_since = None;
                Some(self.send(self.limits.min))
            }
            Input::Resume if self.phase == Phase::Paused && !self.held => {
                self.phase = Phase::Active;
                self.zero_rpm_since = None;
                self.heard_at = None;
                Some(self.send_target())
            }
            Input::Stop => Some(self.finish()),
            Input::Adjust(_) | Input::Pause | Input::Resume => None,
        };
        Output {
            send_power,
            sample: None,
        }
    }

    pub fn view(&self) -> SessionView {
        let segment = self.segments.as_deref().and_then(|segs| {
            let (idx, _) = target_at(segs, self.ride_secs)?;
            Some((idx, &segs[idx]))
        });
        let (segment_index, segment_label, segment_remaining_secs, segment_progress) = match segment
        {
            Some((idx, seg)) => {
                let into = self.ride_secs - seg.start_secs;
                (
                    Some(idx),
                    Some(seg.label.clone()),
                    Some(seg.duration_secs - into),
                    into as f32 / seg.duration_secs as f32,
                )
            }
            None => (None, None, None, 0.0),
        };
        SessionView {
            phase: self.phase,
            connected: self.connected,
            ride_secs: self.ride_secs,
            program_secs: self.segments.as_deref().map(|segs| {
                segs.last()
                    .map_or(0, |s| s.start_secs.saturating_add(s.duration_secs))
            }),
            segment_index,
            segment_label,
            segment_remaining_secs,
            segment_progress,
            target_watts: self.target_watts,
            overridden: self.overridden,
            held: self.held,
            last: self.last,
            distance_m: self.distance_m,
            energy_kj: self.energy_kj,
        }
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    pub fn is_finished(&self) -> bool {
        self.phase == Phase::Finished
    }

    /// The device has confirmed the minimum load as the last value sent.
    pub fn min_confirmed(&self) -> bool {
        self.link.is_some_and(|l| l.confirmed && l.watts == self.limits.min)
    }

    /// After the end only the minimum load is kept up: confirmed, resent, and resent
    /// on reconnect.
    fn on_finished(&mut self, input: Input) -> Output {
        let send_power = match input {
            Input::Status { status, at } => {
                self.last = Some(status);
                self.last_at = Some(at);
                if status.recovery {
                    return Output::default();
                }
                let resend = self.check_link(status, at);
                self.retry_due(status.target_watts, at).or(resend)
            }
            Input::Connected => {
                self.connected = true;
                Some(self.send(self.limits.min))
            }
            Input::Disconnected => {
                self.connected = false;
                None
            }
            _ => None,
        };
        Output {
            send_power,
            sample: None,
        }
    }

    /// Time-based rules without a status: silence in `Active` counts as a stop, and
    /// `Waiting` and `AutoPaused` end after `AUTO_FINISH_AFTER`.
    fn on_tick(&mut self, at: Duration) -> Option<u16> {
        match self.phase {
            Phase::Waiting if at >= AUTO_FINISH_AFTER => Some(self.finish()),
            Phase::Active => {
                let heard = *self.heard_at.get_or_insert(at);
                (at.saturating_sub(heard) > SILENCE_AFTER).then(|| self.auto_pause(at))
            }
            Phase::AutoPaused if !self.held && self.auto_finish_due(at) => Some(self.finish()),
            _ => None,
        }
    }

    fn auto_finish_due(&self, at: Duration) -> bool {
        at.saturating_sub(self.auto_paused_since.unwrap_or(at)) >= AUTO_FINISH_AFTER
    }

    fn auto_pause(&mut self, at: Duration) -> u16 {
        self.phase = Phase::AutoPaused;
        self.auto_paused_since = Some(at);
        self.send(self.limits.min)
    }

    fn on_status(&mut self, status: Status, at: Duration) -> Output {
        self.last = Some(status);
        self.heard_at = Some(at);
        // The last status time is tracked in every phase, so no `dt` jump after a pause.
        let prev_at = self.last_at.replace(at);
        let mut out = Output::default();

        // The cockpit measures recovery: its values are no ride data and its target is
        // left alone. A running ride pauses; pedalling afterwards continues it.
        if status.recovery {
            if self.phase == Phase::Active {
                self.phase = Phase::AutoPaused;
                self.auto_paused_since = Some(at);
                self.zero_rpm_since = None;
            }
            return out;
        }

        match self.phase {
            Phase::Waiting => {
                if status.rpm > 0 && !self.held {
                    self.phase = Phase::Active;
                    out.send_power = self.update_target(true);
                } else if at >= AUTO_FINISH_AFTER {
                    out.send_power = Some(self.finish());
                }
                return out;
            }
            Phase::Paused => {
                let resend = self.check_link(status, at);
                out.send_power = self.retry_due(status.target_watts, at).or(resend);
                return out;
            }
            Phase::AutoPaused => {
                let resend = self.check_link(status, at);
                out.send_power = self.retry_due(status.target_watts, at).or(resend);
                if self.held {
                    // Pedalling and the automatic end wait for the strap measurement.
                } else if status.rpm > 0 {
                    self.phase = Phase::Active;
                    self.zero_rpm_since = None;
                    self.auto_paused_since = None;
                    out.send_power = Some(self.send_target());
                } else if self.auto_finish_due(at) {
                    out.send_power = Some(self.finish());
                }
                return out;
            }
            Phase::Active | Phase::Finished => {}
        }

        out.send_power = self.check_link(status, at);
        if let Some(w) = self.retry_due(status.target_watts, at) {
            out.send_power = Some(w);
        }

        if status.rpm == 0 {
            let since = *self.zero_rpm_since.get_or_insert(at);
            if at.saturating_sub(since) >= AUTO_PAUSE_AFTER {
                out.send_power = Some(self.auto_pause(at));
                return out;
            }
        } else {
            self.zero_rpm_since = None;
        }

        let dt = prev_at
            .map_or(Duration::ZERO, |p| at.saturating_sub(p).min(MAX_STEP))
            .as_secs_f64();
        self.active_secs += dt;
        // km/h × 10 → m/s
        self.distance_m += f64::from(status.speed_x10) / 36.0 * dt;
        self.energy_kj += f64::from(status.actual_watts) * dt / 1000.0;
        // At most one second per status, the rest carries over: with jittery 1 s
        // statuses no second is skipped and each gets its sample.
        let new_second = self.active_secs >= f64::from(self.ride_secs) + 1.0;
        if new_second {
            self.ride_secs += 1;
        }

        if let Some(w) = self.update_target(false) {
            out.send_power = Some(w);
        }
        if new_second {
            out.sample = Some(Sample {
                ride_secs: self.ride_secs,
                status,
                target_watts: self.target_watts,
                at_unix_ms: None,
                strap_pulse: None,
            });
        }
        out
    }

    /// Follows the program at `ride_secs`: clears the override on a segment change and
    /// sends when the effective target changed (always with `force`). Finishes past the end.
    fn update_target(&mut self, force: bool) -> Option<u16> {
        let Some((segment, _)) = self.program_target() else {
            return Some(self.finish());
        };
        if segment != self.segment {
            self.segment = segment;
            self.override_watts = None;
            self.overridden = false;
        }
        (force || self.effective() != self.desired).then(|| self.send_target())
    }

    /// Clamped program target and segment index at `ride_secs`; `None` past the end.
    fn program_target(&self) -> Option<(Option<usize>, u16)> {
        match self.segments.as_deref() {
            None => Some((None, self.limits.clamp(FREE_RIDE_WATTS))),
            Some(segs) => target_at(segs, self.ride_secs)
                .map(|(idx, w)| (Some(idx), self.limits.clamp(i32::from(w)))),
        }
    }

    fn effective(&self) -> u16 {
        self.override_watts
            .or_else(|| self.program_target().map(|(_, w)| w))
            .unwrap_or(self.limits.min)
    }

    fn send_target(&mut self) -> u16 {
        let w = self.effective();
        self.desired = w;
        self.target_watts = w;
        self.send(w)
    }

    fn send(&mut self, watts: u16) -> u16 {
        self.link = Some(Link {
            watts,
            confirmed: false,
            unconfirmed: 0,
            resent: false,
        });
        self.retry_since = None;
        watts
    }

    fn finish(&mut self) -> u16 {
        self.phase = Phase::Finished;
        self.send(self.limits.min)
    }

    /// After an accepted mismatch, resends the wanted value (`desired` while active,
    /// otherwise the minimum) every `RETRY_AFTER` until the device reports it.
    fn retry_due(&mut self, device: u16, at: Duration) -> Option<u16> {
        let since = self.retry_since?;
        let active = self.phase == Phase::Active;
        let settled = if active { self.target_watts == self.desired } else { device == self.limits.min };
        if settled {
            self.retry_since = None;
            return None;
        }
        if at.saturating_sub(since) < RETRY_AFTER {
            return None;
        }
        Some(if active { self.send_target() } else { self.send(self.limits.min) })
    }

    /// Compares the device's target with what we sent: confirms, resends once, then
    /// accepts the device's value. While active, a change after confirmation is a
    /// cockpit override.
    fn check_link(&mut self, status: Status, at: Duration) -> Option<u16> {
        let active = self.phase == Phase::Active;
        let device = status.target_watts;
        let link = self.link.as_mut()?;
        if link.confirmed {
            if device != link.watts && active {
                link.watts = device;
                self.override_watts = Some(device);
                self.overridden = true;
                self.desired = device;
                self.target_watts = device;
            }
            return None;
        }
        if device == link.watts {
            link.confirmed = true;
            return None;
        }
        link.unconfirmed += 1;
        if link.unconfirmed <= CONFIRM_STATUSES {
            return None;
        }
        if !link.resent {
            link.resent = true;
            link.unconfirmed = 0;
            return Some(link.watts);
        }
        let wanted = link.watts;
        link.watts = device;
        link.confirmed = true;
        if active {
            self.target_watts = device;
            if device != self.desired {
                self.retry_since = Some(at);
            }
        } else if device != wanted {
            self.retry_since = Some(at);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::program::fixtures::intervals;

    fn st(rpm: u16, target: u16, actual: u16) -> Status {
        Status {
            pulse: 0,
            rpm,
            speed_x10: rpm * 35 / 10,
            distance: 0,
            target_watts: target,
            energy: 0,
            elapsed_secs: 0,
            actual_watts: actual,
            recovery: false,
        }
    }

    fn status(at_secs: u64, target: u16, actual: u16, rpm: u16) -> Input {
        Input::Status {
            status: st(rpm, target, actual),
            at: Duration::from_secs(at_secs),
        }
    }

    fn program() -> Session {
        Session::new(Some(intervals().expand()), DeviceLimits::default())
    }

    /// A status from a device that has taken over the last value we sent.
    fn echo(s: &mut Session, at_secs: u64, rpm: u16) -> Output {
        let w = s.link.map_or(25, |l| l.watts);
        s.handle(status(at_secs, w, w, rpm))
    }

    /// Starts pedalling at `at` 0.
    fn start(s: &mut Session) -> Output {
        echo(s, 0, 60)
    }

    /// Rides with an echoing device; returns every `(at, send_power)`.
    fn ride(s: &mut Session, ats: std::ops::RangeInclusive<u64>) -> Vec<(u64, u16)> {
        ats.filter_map(|at| echo(s, at, 60).send_power.map(|w| (at, w)))
            .collect()
    }

    #[test]
    fn waits_until_pedalling_then_sends_first_target() {
        let mut s = program();
        assert_eq!(s.handle(status(0, 25, 0, 0)).send_power, None);
        assert_eq!(s.view().phase, Phase::Waiting);
        assert_eq!(s.handle(status(1, 25, 0, 60)).send_power, Some(60));
        assert_eq!(s.view().phase, Phase::Active);
    }

    #[test]
    fn waiting_view_shows_first_target_and_last_status() {
        let mut s = program();
        let v = s.view();
        assert_eq!(v.phase, Phase::Waiting);
        assert_eq!(v.ride_secs, 0);
        assert_eq!(v.target_watts, 60);
        assert_eq!(v.segment_index, Some(0));
        assert_eq!(v.last, None);
        let out = s.handle(status(0, 25, 0, 0));
        assert_eq!(out, Output::default());
        assert_eq!(s.view().last, Some(st(0, 25, 0)));
        assert_eq!(s.handle(Input::Adjust(10)).send_power, None);
        assert_eq!(s.handle(Input::Pause).send_power, None);
        assert_eq!(s.handle(Input::Connected).send_power, None);
        assert_eq!(s.view().phase, Phase::Waiting);
    }

    #[test]
    fn one_sample_per_active_second_and_ride_time_from_server() {
        let mut s = program();
        assert_eq!(start(&mut s).sample, None);
        for at in 1..=10 {
            let sample = echo(&mut s, at, 60).sample.expect("sample");
            assert_eq!(sample.ride_secs, at as u32);
            assert_eq!(sample.target_watts, 60);
        }
        assert_eq!(s.view().ride_secs, 10);
    }

    #[test]
    fn half_second_statuses_give_one_sample_per_second() {
        let mut s = program();
        start(&mut s);
        let mut samples = Vec::new();
        for i in 1..=6u64 {
            let input = Input::Status {
                status: st(60, 60, 60),
                at: Duration::from_millis(i * 500),
            };
            if let Some(sample) = s.handle(input).sample {
                samples.push(sample.ride_secs);
            }
        }
        assert_eq!(samples, vec![1, 2, 3]);
        assert_eq!(s.view().ride_secs, 3);
    }

    #[test]
    fn time_step_is_capped_at_two_seconds() {
        let mut s = program();
        start(&mut s);
        echo(&mut s, 1, 60);
        // 1 s + 2 s (capped): at most one second per status, the rest carries over.
        assert_eq!(echo(&mut s, 11, 60).sample.map(|x| x.ride_secs), Some(2));
        assert_eq!(echo(&mut s, 11, 60).sample.map(|x| x.ride_secs), Some(3));
        assert_eq!(echo(&mut s, 11, 60).sample, None);
        assert_eq!(s.view().ride_secs, 3);
    }

    #[test]
    fn jitter_keeps_one_sample_per_second() {
        let mut s = program();
        start(&mut s);
        let mut samples = Vec::new();
        let mut feed = |s: &mut Session, ms: u64| {
            let input = Input::Status {
                status: st(60, 60, 60),
                at: Duration::from_millis(ms),
            };
            if let Some(sample) = s.handle(input).sample {
                samples.push(sample.ride_secs);
            }
        };
        // 1.99 s has not reached second 2 yet; 3.01 s reaches 2 (not 3), and so on.
        for ms in [1000, 1990, 3010, 3980, 5020] {
            feed(&mut s, ms);
        }
        assert_eq!(s.view().ride_secs, 4);
        feed(&mut s, 6010);
        assert_eq!(samples, vec![1, 2, 3, 4, 5]);
        assert_eq!(s.view().ride_secs, 5);
    }

    #[test]
    fn free_ride_starts_at_50() {
        let mut s = Session::new(None, DeviceLimits::default());
        assert_eq!(s.view().target_watts, 50);
        assert_eq!(s.handle(status(0, 25, 0, 60)).send_power, Some(50));
        ride(&mut s, 1..=30);
        let v = s.view();
        assert_eq!(v.program_secs, None);
        assert_eq!(v.segment_index, None);
        assert_eq!(v.segment_label, None);
        assert_eq!(v.segment_remaining_secs, None);
        assert_eq!(v.segment_progress, 0.0);
        assert_eq!(v.target_watts, 50);
        assert_eq!(s.handle(Input::Adjust(10)).send_power, Some(60));
        assert!(s.view().overridden);
    }

    #[test]
    fn view_shows_segment_position() {
        let mut s = program();
        start(&mut s);
        ride(&mut s, 1..=650);
        let v = s.view();
        assert_eq!(v.program_secs, Some(2100));
        assert_eq!(v.segment_index, Some(1));
        assert_eq!(v.segment_label.as_deref(), Some("Intervall 1/5"));
        assert_eq!(v.segment_remaining_secs, Some(70));
        assert!((v.segment_progress - 50.0 / 120.0).abs() < 1e-6);
        assert_eq!(v.target_watts, 150);
        assert!(!v.overridden);
    }

    #[test]
    fn program_ramp_is_clamped_to_device_steps() {
        let mut s = program();
        start(&mut s);
        let sends = ride(&mut s, 1..=100);
        // 60 + 40 * t / 600, rounded to 5 W: 65 from t = 38 (62.53 → 63 → 65).
        assert_eq!(sends, vec![(38, 65)]);
    }

    #[test]
    fn segment_change_sends_new_target_and_clears_override() {
        let mut s = program();
        start(&mut s);
        ride(&mut s, 1..=590);
        assert_eq!(s.view().target_watts, 100);
        assert_eq!(s.handle(Input::Adjust(10)).send_power, Some(110));
        assert!(s.view().overridden);
        assert_eq!(s.view().target_watts, 110);
        assert_eq!(ride(&mut s, 591..=599), vec![]);
        assert_eq!(s.view().target_watts, 110);
        assert_eq!(echo(&mut s, 600, 60).send_power, Some(150));
        let v = s.view();
        assert!(!v.overridden);
        assert_eq!(v.target_watts, 150);
        assert_eq!(v.segment_index, Some(1));
    }

    #[test]
    fn adjust_changes_target_by_given_delta_rounded() {
        let mut s = program();
        start(&mut s);
        ride(&mut s, 1..=600);
        assert_eq!(s.view().target_watts, 150);
        assert_eq!(s.handle(Input::Adjust(10)).send_power, Some(160));
        assert_eq!(s.handle(Input::Adjust(-10)).send_power, Some(150));
        assert_eq!(s.handle(Input::Adjust(3)).send_power, Some(155));
        assert_eq!(s.handle(Input::Adjust(-500)).send_power, Some(25));
        assert_eq!(s.handle(Input::Adjust(1000)).send_power, Some(400));
        assert_eq!(s.view().target_watts, 400);
        assert!(s.view().overridden);
    }

    #[test]
    fn cockpit_change_becomes_override_without_sending() {
        let mut s = program();
        start(&mut s);
        assert_eq!(ride(&mut s, 1..=720).last(), Some(&(720, 80)));
        assert_eq!(s.handle(status(721, 80, 80, 60)).send_power, None);
        assert_eq!(s.handle(status(722, 100, 90, 60)).send_power, None);
        let v = s.view();
        assert_eq!(v.target_watts, 100);
        assert!(v.overridden);
        for at in 723..=839 {
            assert_eq!(s.handle(status(at, 100, 100, 60)).send_power, None);
        }
        assert_eq!(s.handle(status(840, 100, 100, 60)).send_power, Some(150));
        assert!(!s.view().overridden);
        assert_eq!(s.view().target_watts, 150);
    }

    #[test]
    fn cockpit_change_is_ignored_while_paused() {
        let mut s = program();
        start(&mut s);
        ride(&mut s, 1..=5);
        assert_eq!(s.handle(Input::Pause).send_power, Some(25));
        assert_eq!(s.handle(status(6, 25, 25, 60)).send_power, None);
        assert_eq!(s.handle(status(7, 40, 25, 60)).send_power, None);
        assert!(!s.view().overridden);
        assert_eq!(s.view().target_watts, 60);
        assert_eq!(s.handle(Input::Resume).send_power, Some(60));
        assert!(!s.view().overridden);
    }

    #[test]
    fn unconfirmed_target_resent_once_then_accepted() {
        let mut s = program();
        start(&mut s);
        ride(&mut s, 1..=337);
        assert_eq!(s.view().target_watts, 80);
        // 60 + 40 * 338 / 600 = 82.53 → 83 → 85
        assert_eq!(s.handle(status(338, 80, 80, 60)).send_power, Some(85));
        for at in 339..=341 {
            assert_eq!(s.handle(status(at, 80, 80, 60)).send_power, None);
        }
        assert_eq!(s.handle(status(342, 80, 80, 60)).send_power, Some(85));
        for at in 343..=345 {
            assert_eq!(s.handle(status(at, 80, 80, 60)).send_power, None);
            assert_eq!(s.view().target_watts, 85);
        }
        // The 4th status after the resend: the device's value is accepted.
        assert_eq!(s.handle(status(346, 80, 80, 60)).send_power, None);
        assert_eq!(s.view().target_watts, 80);
        for at in 347..=360 {
            assert_eq!(s.handle(status(at, 80, 80, 60)).send_power, None, "at {at}");
        }
        let v = s.view();
        assert_eq!(v.target_watts, 80);
        assert!(!v.overridden);
        for at in 361..=412 {
            s.handle(status(at, 80, 80, 60));
        }
        // The ramp goes on: 87.53 → 88 → 90.
        assert_eq!(s.handle(status(413, 80, 80, 60)).send_power, Some(90));
        assert_eq!(s.view().target_watts, 90);
    }

    #[test]
    fn accepted_mismatch_retries_desired_every_15s() {
        let mut s = program();
        start(&mut s);
        ride(&mut s, 1..=337);
        assert_eq!(s.handle(status(338, 80, 80, 60)).send_power, Some(85));
        for at in 339..=345 {
            s.handle(status(at, 80, 80, 60));
        }
        // Accepted at 346, retried 15 s later.
        assert_eq!(s.handle(status(346, 80, 80, 60)).send_power, None);
        for at in 347..=360 {
            assert_eq!(s.handle(status(at, 80, 80, 60)).send_power, None, "at {at}");
        }
        assert_eq!(s.handle(status(361, 80, 80, 60)).send_power, Some(85));
        assert_eq!(s.view().target_watts, 85);
        // Same cycle again: resend at 365, accept at 369, retry at 384.
        for at in 362..=383 {
            let expected = if at == 365 { Some(85) } else { None };
            assert_eq!(
                s.handle(status(at, 80, 80, 60)).send_power,
                expected,
                "at {at}"
            );
        }
        assert_eq!(s.view().target_watts, 80);
        assert_eq!(s.handle(status(384, 80, 80, 60)).send_power, Some(85));
        // The device takes it: no more retries.
        for at in 385..=412 {
            assert_eq!(s.handle(status(at, 85, 85, 60)).send_power, None, "at {at}");
        }
        assert_eq!(s.view().target_watts, 85);
        assert!(!s.view().overridden);
    }

    #[test]
    fn auto_pause_after_three_seconds_without_rpm() {
        let mut s = program();
        start(&mut s);
        ride(&mut s, 1..=9);
        for at in 10..=12 {
            let out = echo(&mut s, at, 0);
            assert_eq!(out.send_power, None);
            assert_eq!(s.view().phase, Phase::Active);
        }
        let out = echo(&mut s, 13, 0);
        assert_eq!(out.send_power, Some(25));
        assert_eq!(out.sample, None);
        assert_eq!(s.view().phase, Phase::AutoPaused);
        assert_eq!(s.view().ride_secs, 12);
        for at in 14..=15 {
            assert_eq!(echo(&mut s, at, 0), Output::default());
        }
        assert_eq!(s.view().ride_secs, 12);
        let out = echo(&mut s, 16, 60);
        assert_eq!(out.send_power, Some(60));
        assert_eq!(out.sample, None);
        assert_eq!(s.view().phase, Phase::Active);
        assert_eq!(echo(&mut s, 17, 60).sample.map(|x| x.ride_secs), Some(13));
    }

    fn recovery(at_secs: u64, target: u16, rpm: u16, remaining: u32) -> Input {
        Input::Status {
            status: Status { recovery: true, elapsed_secs: remaining, pulse: 106, ..st(rpm, target, 0) },
            at: Duration::from_secs(at_secs),
        }
    }

    #[test]
    fn recovery_pauses_ride_without_samples_or_commands() {
        let mut s = program();
        start(&mut s);
        ride(&mut s, 1..=9);
        let target = s.view().target_watts;
        // The cockpit still reports rpm for a moment after the button press.
        for at in 10..=80 {
            let out = s.handle(recovery(at, target, if at < 14 { 40 } else { 0 }, 70 - at.min(70) as u32));
            assert_eq!(out, Output::default(), "at {at}");
            assert_eq!(s.view().phase, Phase::AutoPaused);
        }
        assert_eq!(s.view().ride_secs, 9);
        assert!(s.view().last.is_some_and(|l| l.recovery));
        let out = s.handle(status(81, target, 0, 60));
        assert_eq!(out.send_power, Some(target));
        assert_eq!(s.view().phase, Phase::Active);
        assert_eq!(echo(&mut s, 82, 60).sample.map(|x| x.ride_secs), Some(10));
    }

    fn hold(s: &mut Session, on: bool, at_secs: u64) -> Output {
        s.handle(Input::Hold { on, at: Duration::from_secs(at_secs) })
    }

    #[test]
    fn hold_pauses_active_ride_with_min_load() {
        let mut s = program();
        start(&mut s);
        ride(&mut s, 1..=5);
        assert_eq!(hold(&mut s, true, 6).send_power, Some(25));
        assert_eq!(s.view().phase, Phase::AutoPaused);
        assert!(s.view().held);
    }

    #[test]
    fn pedalling_does_not_resume_while_held() {
        let mut s = program();
        start(&mut s);
        ride(&mut s, 1..=5);
        hold(&mut s, true, 6);
        for at in 7..=16 {
            assert_eq!(echo(&mut s, at, 60).sample, None, "at {at}");
            assert_eq!(s.view().phase, Phase::AutoPaused);
        }
        assert_eq!(hold(&mut s, false, 17), Output::default());
        assert!(!s.view().held);
        assert_eq!(s.view().phase, Phase::AutoPaused);
        echo(&mut s, 18, 60);
        assert_eq!(s.view().phase, Phase::Active);
    }

    #[test]
    fn resume_ignored_while_held() {
        let mut s = program();
        start(&mut s);
        ride(&mut s, 1..=5);
        hold(&mut s, true, 6);
        assert_eq!(s.handle(Input::Pause).send_power, Some(25));
        assert_eq!(s.view().phase, Phase::Paused);
        assert_eq!(s.handle(Input::Resume).send_power, None);
        assert_eq!(s.view().phase, Phase::Paused);
        hold(&mut s, false, 20);
        assert_eq!(s.view().phase, Phase::Paused);
        assert!(s.handle(Input::Resume).send_power.is_some());
        assert_eq!(s.view().phase, Phase::Active);
    }

    #[test]
    fn stop_while_held_finishes() {
        let mut s = program();
        start(&mut s);
        ride(&mut s, 1..=5);
        hold(&mut s, true, 6);
        assert_eq!(s.handle(Input::Stop).send_power, Some(25));
        assert!(s.is_finished());
    }

    #[test]
    fn no_auto_finish_while_held() {
        let mut s = program();
        start(&mut s);
        ride(&mut s, 1..=5);
        hold(&mut s, true, 6);
        let late = 6 + AUTO_FINISH_AFTER.as_secs() + 30;
        s.handle(Input::Tick { at: Duration::from_secs(late) });
        echo(&mut s, late + 1, 0);
        assert_eq!(s.view().phase, Phase::AutoPaused);
    }

    #[test]
    fn held_waiting_does_not_start() {
        let mut s = program();
        hold(&mut s, true, 0);
        echo(&mut s, 1, 60);
        assert_eq!(s.view().phase, Phase::Waiting);
    }

    #[test]
    fn recovery_keeps_manual_pause_and_waiting() {
        let mut s = program();
        assert_eq!(s.handle(recovery(0, 25, 50, 59)), Output::default());
        assert_eq!(s.view().phase, Phase::Waiting);
        start(&mut s);
        ride(&mut s, 1..=5);
        s.handle(Input::Pause);
        for at in 6..=30 {
            assert_eq!(s.handle(recovery(at, 60, 0, 0)), Output::default());
        }
        assert_eq!(s.view().phase, Phase::Paused);
    }

    #[test]
    fn recovery_after_finish_sends_nothing() {
        let mut s = program();
        start(&mut s);
        ride(&mut s, 1..=5);
        assert_eq!(s.handle(Input::Stop).send_power, Some(25));
        // The device still reports the old target during the measurement.
        for at in 6..=40 {
            assert_eq!(s.handle(recovery(at, 60, 0, 30)), Output::default());
        }
        for at in 41..=43 {
            assert_eq!(s.handle(status(at, 60, 0, 0)).send_power, None);
        }
        assert_eq!(s.handle(status(44, 60, 0, 0)).send_power, Some(25));
    }

    #[test]
    fn short_stop_does_not_auto_pause() {
        let mut s = program();
        start(&mut s);
        ride(&mut s, 1..=9);
        echo(&mut s, 10, 0);
        echo(&mut s, 12, 0);
        echo(&mut s, 13, 60);
        echo(&mut s, 14, 0);
        assert_eq!(echo(&mut s, 16, 0).send_power, None);
        assert_eq!(s.view().phase, Phase::Active);
    }

    #[test]
    fn manual_pause_drops_load_and_freezes_time() {
        let mut s = program();
        start(&mut s);
        ride(&mut s, 1..=5);
        assert_eq!(s.handle(Input::Pause).send_power, Some(25));
        assert_eq!(s.view().phase, Phase::Paused);
        for at in 6..=20 {
            assert_eq!(echo(&mut s, at, 60), Output::default());
        }
        assert_eq!(s.view().ride_secs, 5);
        assert_eq!(s.view().last, Some(st(60, 25, 25)));
        assert_eq!(s.handle(Input::Pause).send_power, None);
        assert_eq!(s.handle(Input::Adjust(10)).send_power, None);
        assert_eq!(s.handle(Input::Resume).send_power, Some(60));
        assert_eq!(s.view().phase, Phase::Active);
        assert_eq!(s.handle(Input::Resume).send_power, None);
        assert_eq!(echo(&mut s, 21, 60).sample.map(|x| x.ride_secs), Some(6));
    }

    #[test]
    fn reconnect_resends_target() {
        let mut s = program();
        start(&mut s);
        ride(&mut s, 1..=5);
        assert_eq!(s.handle(Input::Disconnected).send_power, None);
        assert!(!s.view().connected);
        assert_eq!(s.view().phase, Phase::Active);
        assert_eq!(s.handle(Input::Connected).send_power, Some(60));
        assert!(s.view().connected);
        // Confirmation starts over: 3 statuses without it, the 4th resends.
        for at in 6..=8 {
            assert_eq!(s.handle(status(at, 25, 25, 60)).send_power, None);
        }
        assert_eq!(s.handle(status(9, 25, 25, 60)).send_power, Some(60));
    }

    #[test]
    fn reconnect_while_paused_resends_min() {
        let mut s = program();
        start(&mut s);
        ride(&mut s, 1..=5);
        s.handle(Input::Pause);
        s.handle(Input::Disconnected);
        assert_eq!(s.view().phase, Phase::Paused);
        assert_eq!(s.handle(Input::Connected).send_power, Some(25));
    }

    #[test]
    fn finishes_at_program_end() {
        let mut s = program();
        start(&mut s);
        ride(&mut s, 1..=2099);
        assert!(!s.is_finished());
        let out = echo(&mut s, 2100, 60);
        assert_eq!(out.send_power, Some(25));
        assert_eq!(out.sample.map(|x| x.ride_secs), Some(2100));
        assert_eq!(s.view().phase, Phase::Finished);
        assert_eq!(s.view().ride_secs, 2100);
        assert!(s.is_finished());
        assert_eq!(echo(&mut s, 2101, 60), Output::default());
        assert_eq!(s.handle(Input::Adjust(10)), Output::default());
        assert_eq!(s.view().ride_secs, 2100);
    }

    #[test]
    fn stop_finishes_and_drops_load() {
        let mut s = program();
        start(&mut s);
        ride(&mut s, 1..=5);
        assert_eq!(s.handle(Input::Stop).send_power, Some(25));
        assert!(s.is_finished());
        assert_eq!(s.handle(Input::Stop), Output::default());
        assert_eq!(s.handle(Input::Resume), Output::default());
        assert_eq!(s.view().ride_secs, 5);
    }

    #[test]
    fn connected_only_by_connected_input() {
        let mut s = program();
        start(&mut s);
        assert!(!s.view().connected);
        s.handle(Input::Connected);
        assert!(s.view().connected);
        s.handle(Input::Disconnected);
        // A stale status queued before the disconnect.
        echo(&mut s, 1, 60);
        assert!(!s.view().connected);
    }

    #[test]
    fn finished_resends_min_if_unconfirmed() {
        let mut s = program();
        start(&mut s);
        ride(&mut s, 1..=5);
        assert_eq!(s.handle(Input::Stop).send_power, Some(25));
        for at in 6..=8 {
            assert_eq!(s.handle(status(at, 60, 60, 60)).send_power, None);
        }
        assert_eq!(s.handle(status(9, 60, 60, 60)).send_power, Some(25));
        assert_eq!(s.handle(status(10, 25, 25, 60)).send_power, None);
        assert_eq!(s.handle(status(11, 40, 40, 60)).send_power, None);
        assert!(s.is_finished());
        assert!(!s.view().overridden);
        assert_eq!(s.view().last, Some(st(60, 40, 40)));
    }

    #[test]
    fn min_confirmed_only_after_device_reports_min() {
        let mut s = program();
        start(&mut s);
        ride(&mut s, 1..=5);
        assert!(!s.min_confirmed());
        s.handle(Input::Stop);
        assert!(!s.min_confirmed());
        s.handle(status(6, 60, 60, 60));
        assert!(!s.min_confirmed());
        s.handle(status(7, 25, 25, 60));
        assert!(s.min_confirmed());

        // A device value accepted instead of the minimum doesn't count.
        let mut s = program();
        start(&mut s);
        s.handle(Input::Stop);
        for at in 1..=8 {
            s.handle(status(at, 60, 60, 60));
        }
        assert!(!s.min_confirmed());
    }

    #[test]
    fn finished_resends_min_on_reconnect() {
        let mut s = program();
        start(&mut s);
        s.handle(Input::Connected);
        s.handle(Input::Stop);
        s.handle(Input::Disconnected);
        assert!(!s.view().connected);
        assert_eq!(s.handle(Input::Connected).send_power, Some(25));
        assert!(s.view().connected);
        assert!(s.is_finished());
    }

    fn auto_paused() -> Session {
        let mut s = program();
        start(&mut s);
        ride(&mut s, 1..=9);
        for at in 10..=12 {
            echo(&mut s, at, 0);
        }
        assert_eq!(echo(&mut s, 13, 0).send_power, Some(25));
        assert_eq!(s.view().phase, Phase::AutoPaused);
        s
    }

    #[test]
    fn auto_pause_finishes_after_ten_minutes() {
        let mut s = program();
        start(&mut s);
        ride(&mut s, 1..=9);
        for at in 10..=12 {
            echo(&mut s, at, 0);
        }
        let secs = s.view().ride_secs;
        assert_eq!(echo(&mut s, 13, 0).send_power, Some(25));
        assert_eq!(s.view().phase, Phase::AutoPaused);
        for at in 14..=(13 + 599) {
            assert_eq!(echo(&mut s, at, 0), Output::default(), "at {at}");
        }
        assert_eq!(s.view().phase, Phase::AutoPaused);
        let out = echo(&mut s, 13 + 600, 0);
        assert_eq!(out.send_power, Some(25));
        assert_eq!(out.sample, None);
        assert!(s.is_finished());
        assert_eq!(s.view().ride_secs, secs);
    }

    #[test]
    fn manual_pause_does_not_auto_finish() {
        let mut s = program();
        start(&mut s);
        ride(&mut s, 1..=5);
        s.handle(Input::Pause);
        for at in 6..=(6 + 20 * 60) {
            echo(&mut s, at, 0);
        }
        assert_eq!(s.view().phase, Phase::Paused);
    }

    #[test]
    fn resume_before_ten_minutes_continues() {
        let mut s = auto_paused();
        for at in 14..=(13 + 540) {
            echo(&mut s, at, 0);
        }
        assert_eq!(s.view().phase, Phase::AutoPaused);
        assert_eq!(echo(&mut s, 13 + 541, 60).send_power, Some(60));
        assert_eq!(s.view().phase, Phase::Active);
    }

    #[test]
    fn autopaused_resends_min_on_reconnect() {
        let mut s = auto_paused();
        s.handle(Input::Disconnected);
        assert_eq!(s.handle(Input::Connected).send_power, Some(25));
        assert_eq!(s.view().phase, Phase::AutoPaused);
    }

    #[test]
    fn cockpit_change_is_ignored_while_auto_paused() {
        let mut s = auto_paused();
        assert_eq!(s.handle(status(14, 25, 0, 0)).send_power, None);
        assert_eq!(s.handle(status(15, 40, 0, 0)).send_power, None);
        assert!(!s.view().overridden);
        assert_eq!(s.view().target_watts, 60);
        assert_eq!(s.handle(status(16, 40, 0, 60)).send_power, Some(60));
        assert!(!s.view().overridden);
    }

    #[test]
    fn pause_from_auto_pause_holds_until_resume() {
        let mut s = auto_paused();
        assert_eq!(s.handle(Input::Pause).send_power, Some(25));
        assert_eq!(s.view().phase, Phase::Paused);
        assert_eq!(echo(&mut s, 14, 60), Output::default());
        assert_eq!(s.view().phase, Phase::Paused);
        assert_eq!(s.handle(Input::Resume).send_power, Some(60));
        assert_eq!(s.view().phase, Phase::Active);
    }

    #[test]
    fn stop_from_every_phase_drops_load() {
        let mut waiting = program();
        assert_eq!(waiting.handle(Input::Stop).send_power, Some(25));
        assert!(waiting.is_finished());

        let mut paused = program();
        start(&mut paused);
        paused.handle(Input::Pause);
        assert_eq!(paused.handle(Input::Stop).send_power, Some(25));
        assert!(paused.is_finished());

        let mut auto = auto_paused();
        assert_eq!(auto.handle(Input::Stop).send_power, Some(25));
        assert!(auto.is_finished());
    }

    #[test]
    fn override_survives_pause_within_segment() {
        let mut s = program();
        start(&mut s);
        ride(&mut s, 1..=650);
        assert_eq!(s.handle(Input::Adjust(10)).send_power, Some(160));
        s.handle(Input::Pause);
        echo(&mut s, 651, 60);
        assert_eq!(s.handle(Input::Resume).send_power, Some(160));
        assert!(s.view().overridden);
        assert_eq!(echo(&mut s, 652, 60).send_power, None);
        assert_eq!(s.view().target_watts, 160);
    }

    #[test]
    fn accept_while_paused_keeps_target() {
        let mut s = program();
        start(&mut s);
        ride(&mut s, 1..=5);
        assert_eq!(s.handle(Input::Pause).send_power, Some(25));
        for at in 6..=13 {
            let expected = if at == 9 { Some(25) } else { None };
            assert_eq!(
                s.handle(status(at, 40, 40, 60)).send_power,
                expected,
                "at {at}"
            );
        }
        assert_eq!(s.handle(status(14, 40, 40, 60)).send_power, None);
        assert_eq!(s.view().target_watts, 60);
        assert!(!s.view().overridden);
        assert_eq!(s.handle(Input::Resume).send_power, Some(60));
    }

    #[test]
    fn resend_on_auto_pause_trigger_gives_min() {
        let mut s = program();
        start(&mut s);
        assert_eq!(ride(&mut s, 1..=600).last(), Some(&(600, 150)));
        for at in 601..=603 {
            assert_eq!(s.handle(status(at, 80, 0, 0)).send_power, None);
        }
        // 4th unconfirmed status (would resend 150) and 3 s without rpm.
        assert_eq!(s.handle(status(604, 80, 0, 0)).send_power, Some(25));
        assert_eq!(s.view().phase, Phase::AutoPaused);
    }

    #[test]
    fn resend_on_program_end_gives_min() {
        let mut s = program();
        start(&mut s);
        ride(&mut s, 1..=2096);
        assert_eq!(s.handle(Input::Adjust(100)).send_power, Some(160));
        for at in 2097..=2099 {
            assert_eq!(s.handle(status(at, 60, 60, 60)).send_power, None);
        }
        // 4th unconfirmed status (would resend 160) and the program ends.
        assert_eq!(s.handle(status(2100, 60, 60, 60)).send_power, Some(25));
        assert!(s.is_finished());
    }

    fn tick(at_secs: u64) -> Input {
        Input::Tick { at: Duration::from_secs(at_secs) }
    }

    #[test]
    fn tick_finishes_auto_pause_after_ten_minutes_without_statuses() {
        let mut s = auto_paused();
        assert_eq!(s.handle(tick(13 + 599)), Output::default());
        assert_eq!(s.view().phase, Phase::AutoPaused);
        assert_eq!(s.handle(tick(13 + 600)).send_power, Some(25));
        assert!(s.is_finished());
        assert_eq!(s.handle(tick(13 + 601)), Output::default());
    }

    #[test]
    fn silence_while_active_auto_pauses_and_pedalling_continues_ride() {
        let mut s = program();
        start(&mut s);
        ride(&mut s, 1..=9);
        // 4 s and exactly 5 s of silence are still fine.
        for at in 10..=14 {
            assert_eq!(s.handle(tick(at)), Output::default(), "at {at}");
            assert_eq!(s.view().phase, Phase::Active);
        }
        // 6 s without any status.
        assert_eq!(s.handle(tick(15)).send_power, Some(25));
        assert_eq!(s.view().phase, Phase::AutoPaused);
        s.handle(Input::Disconnected);
        assert_eq!(s.handle(tick(30)), Output::default());
        // A 20 s USB blip: the same ride goes on.
        assert_eq!(s.handle(Input::Connected).send_power, Some(25));
        assert_eq!(s.handle(status(33, 25, 25, 60)).send_power, Some(60));
        assert_eq!(s.view().phase, Phase::Active);
        assert_eq!(s.view().ride_secs, 9);
        assert_eq!(echo(&mut s, 34, 60).sample.map(|x| x.ride_secs), Some(10));
    }

    #[test]
    fn silence_after_power_off_finishes_after_ten_minutes() {
        let mut s = program();
        start(&mut s);
        ride(&mut s, 1..=9);
        s.handle(Input::Disconnected);
        assert_eq!(s.handle(tick(15)).send_power, Some(25));
        assert_eq!(s.handle(tick(15 + 599)), Output::default());
        assert_eq!(s.handle(tick(15 + 600)).send_power, Some(25));
        assert!(s.is_finished());
        assert_eq!(s.view().ride_secs, 9);
    }

    #[test]
    fn resume_restarts_silence_clock() {
        let mut s = program();
        start(&mut s);
        ride(&mut s, 1..=5);
        s.handle(Input::Pause);
        assert_eq!(s.handle(tick(100)), Output::default());
        assert_eq!(s.handle(Input::Resume).send_power, Some(60));
        assert_eq!(s.handle(tick(101)), Output::default());
        assert_eq!(s.handle(tick(106)), Output::default());
        assert_eq!(s.view().phase, Phase::Active);
        assert_eq!(s.handle(tick(107)).send_power, Some(25));
        assert_eq!(s.view().phase, Phase::AutoPaused);
    }

    #[test]
    fn manual_pause_ignores_silence() {
        let mut s = program();
        start(&mut s);
        ride(&mut s, 1..=5);
        s.handle(Input::Pause);
        for at in 6..=(6 + 20 * 60) {
            assert_eq!(s.handle(tick(at)), Output::default());
        }
        assert_eq!(s.view().phase, Phase::Paused);
    }

    #[test]
    fn waiting_finishes_after_ten_minutes_without_pedalling() {
        let mut s = program();
        assert_eq!(s.handle(tick(599)), Output::default());
        assert_eq!(s.handle(status(599, 25, 0, 0)), Output::default());
        assert_eq!(s.view().phase, Phase::Waiting);
        assert_eq!(s.handle(tick(600)).send_power, Some(25));
        assert!(s.is_finished());

        let mut s = program();
        assert_eq!(s.handle(status(600, 25, 0, 0)).send_power, Some(25));
        assert!(s.is_finished());
        assert_eq!(s.view().ride_secs, 0);
    }

    /// Feeds statuses with the device target `device` from `from` to `to` and
    /// returns every `(at, send_power)`.
    fn feed_device(s: &mut Session, device: u16, rpm: u16, ats: std::ops::RangeInclusive<u64>) -> Vec<(u64, u16)> {
        ats.filter_map(|at| s.handle(status(at, device, 0, rpm)).send_power.map(|w| (at, w)))
            .collect()
    }

    #[test]
    fn accepted_value_while_paused_retries_min_every_15s() {
        let mut s = program();
        start(&mut s);
        ride(&mut s, 1..=5);
        assert_eq!(s.handle(Input::Pause).send_power, Some(25));
        // Resend at 9, accepted at 13, retried at 28, resend at 32, accepted at 36, retried at 51.
        assert_eq!(feed_device(&mut s, 40, 60, 6..=51), vec![(9, 25), (28, 25), (32, 25), (51, 25)]);
        // The device takes it: no more retries.
        assert_eq!(feed_device(&mut s, 25, 60, 52..=90), vec![]);
        assert_eq!(s.view().target_watts, 60);
        assert_eq!(s.handle(Input::Resume).send_power, Some(60));
    }

    #[test]
    fn accepted_value_while_auto_paused_retries_min_every_15s() {
        let mut s = auto_paused();
        assert_eq!(feed_device(&mut s, 40, 0, 14..=36), vec![(17, 25), (36, 25)]);
        // A cockpit change to the minimum ends the retries.
        assert_eq!(feed_device(&mut s, 25, 0, 37..=80), vec![]);
        assert_eq!(s.view().phase, Phase::AutoPaused);
    }

    #[test]
    fn accepted_value_after_finish_retries_min_every_15s() {
        let mut s = program();
        start(&mut s);
        ride(&mut s, 1..=5);
        assert_eq!(s.handle(Input::Stop).send_power, Some(25));
        assert_eq!(feed_device(&mut s, 60, 60, 6..=28), vec![(9, 25), (28, 25)]);
        assert!(!s.min_confirmed());
        assert_eq!(feed_device(&mut s, 25, 60, 29..=29), vec![]);
        assert!(s.min_confirmed());
    }

    #[test]
    fn integrates_distance_and_energy() {
        let mut s = Session::new(None, DeviceLimits::default());
        let fixed = |at: u64| Input::Status {
            status: Status {
                speed_x10: 300,
                ..st(86, 50, 100)
            },
            at: Duration::from_secs(at),
        };
        s.handle(fixed(0));
        for at in 1..=3600 {
            s.handle(fixed(at));
        }
        let v = s.view();
        assert_eq!(v.ride_secs, 3600);
        assert!((v.distance_m - 30000.0).abs() < 1e-6, "{}", v.distance_m);
        assert!((v.energy_kj - 360.0).abs() < 1e-6, "{}", v.energy_kj);
        assert!(!s.is_finished());
    }
}
