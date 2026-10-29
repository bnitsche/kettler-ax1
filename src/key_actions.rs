//! Turns handlebar key events into ride actions with the hold rules.
//!
//! UP and DOWN adjust the target power at once and repeat while held. RIGHT toggles the
//! pause on release. LEFT toggles the view on a short press, shows a stop hint after
//! [`STOP_HINT_AFTER`] and stops the ride after [`STOP_AFTER`]. Pure: time is passed in.

use std::time::Duration;

use serde::Serialize;

use crate::keypad::{Key, KeyEvent};

/// Power change per UP or DOWN step in watts.
pub const STEP_WATTS: i16 = 10;
/// How long UP or DOWN must be held before the first repeat.
pub const REPEAT_DELAY: Duration = Duration::from_millis(600);
/// Interval between repeats while UP or DOWN stays held.
pub const REPEAT_EVERY: Duration = Duration::from_millis(400);
/// How long LEFT must be held before the stop hint shows.
pub const STOP_HINT_AFTER: Duration = Duration::from_millis(500);
/// How long LEFT must be held to stop the ride.
pub const STOP_AFTER: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum KeyInput {
    Event(KeyEvent),
    Connected(bool),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyAction {
    Adjust(i16),
    TogglePause,
    ToggleView,
    Stop,
    StopHint(bool),
}

/// The key currently held down.
struct Held {
    key: Key,
    since: Duration,
    /// UP/DOWN: when the next repeat is due.
    next_repeat: Duration,
    /// LEFT: the stop hint is visible.
    hint_shown: bool,
    /// LEFT: the stop already fired.
    stopped: bool,
}

#[derive(Default)]
pub struct KeyActions {
    held: Option<Held>,
}

fn step(key: Key) -> Option<i16> {
    match key {
        Key::Up => Some(STEP_WATTS),
        Key::Down => Some(-STEP_WATTS),
        _ => None,
    }
}

impl KeyActions {
    pub fn handle(&mut self, input: KeyInput, at: Duration) -> Vec<KeyAction> {
        match input {
            KeyInput::Event(KeyEvent::Press(key)) => {
                let mut out = Vec::new();
                if self.held.as_ref().is_some_and(|h| h.hint_shown) {
                    out.push(KeyAction::StopHint(false));
                }
                self.held = Some(Held {
                    key,
                    since: at,
                    next_repeat: at + REPEAT_DELAY,
                    hint_shown: false,
                    stopped: false,
                });
                out.extend(step(key).map(KeyAction::Adjust));
                out
            }
            KeyInput::Event(KeyEvent::Release(key)) => {
                let Some(held) = self.held.take_if(|h| h.key == key) else {
                    return Vec::new();
                };
                match key {
                    Key::Right => vec![KeyAction::TogglePause],
                    Key::Left if held.stopped => Vec::new(),
                    Key::Left if held.hint_shown => {
                        vec![KeyAction::ToggleView, KeyAction::StopHint(false)]
                    }
                    Key::Left => vec![KeyAction::ToggleView],
                    Key::Up | Key::Down => Vec::new(),
                }
            }
            KeyInput::Connected(true) => Vec::new(),
            KeyInput::Connected(false) => match self.held.take() {
                Some(h) if h.hint_shown => vec![KeyAction::StopHint(false)],
                _ => Vec::new(),
            },
        }
    }

    /// Fires the timed actions that are due at `at` (at most one repeat per call).
    pub fn tick(&mut self, at: Duration) -> Vec<KeyAction> {
        let Some(h) = self.held.as_mut() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        match h.key {
            Key::Up | Key::Down => {
                if at >= h.next_repeat {
                    out.extend(step(h.key).map(KeyAction::Adjust));
                    h.next_repeat += REPEAT_EVERY;
                }
            }
            Key::Left if !h.stopped => {
                if at >= h.since + STOP_AFTER {
                    h.stopped = true;
                    h.hint_shown = false;
                    out.push(KeyAction::Stop);
                    out.push(KeyAction::StopHint(false));
                } else if !h.hint_shown && at >= h.since + STOP_HINT_AFTER {
                    h.hint_shown = true;
                    out.push(KeyAction::StopHint(true));
                }
            }
            _ => {}
        }
        out
    }

    /// Absolute time of the next pending timed action, if any.
    pub fn next_deadline(&self) -> Option<Duration> {
        let h = self.held.as_ref()?;
        match h.key {
            Key::Up | Key::Down => Some(h.next_repeat),
            Key::Left if h.stopped => None,
            Key::Left if h.hint_shown => Some(h.since + STOP_AFTER),
            Key::Left => Some(h.since + STOP_HINT_AFTER),
            Key::Right => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }
    fn press(k: Key) -> KeyInput {
        KeyInput::Event(KeyEvent::Press(k))
    }
    fn release(k: Key) -> KeyInput {
        KeyInput::Event(KeyEvent::Release(k))
    }

    #[test]
    fn up_adjusts_and_repeats_while_held() {
        let mut k = KeyActions::default();
        assert_eq!(k.handle(press(Key::Up), ms(0)), vec![KeyAction::Adjust(10)]);
        assert!(k.tick(ms(599)).is_empty());
        assert_eq!(k.tick(ms(600)), vec![KeyAction::Adjust(10)]);
        assert!(k.tick(ms(999)).is_empty());
        assert_eq!(k.tick(ms(1000)), vec![KeyAction::Adjust(10)]);
        assert_eq!(k.tick(ms(1400)), vec![KeyAction::Adjust(10)]);
        assert!(k.handle(release(Key::Up), ms(1500)).is_empty());
        assert!(k.tick(ms(2000)).is_empty());
    }

    #[test]
    fn late_tick_emits_one_adjust_and_keeps_schedule() {
        let mut k = KeyActions::default();
        k.handle(press(Key::Up), ms(0));
        assert_eq!(k.tick(ms(1400)), vec![KeyAction::Adjust(10)]);
        assert_eq!(k.next_deadline(), Some(ms(1000)));
    }

    #[test]
    fn down_adjusts_minus_ten() {
        let mut k = KeyActions::default();
        assert_eq!(k.handle(press(Key::Down), ms(0)), vec![KeyAction::Adjust(-10)]);
        assert_eq!(k.tick(ms(600)), vec![KeyAction::Adjust(-10)]);
    }

    #[test]
    fn right_toggles_pause_on_release_only() {
        let mut k = KeyActions::default();
        assert!(k.handle(press(Key::Right), ms(0)).is_empty());
        assert!(k.tick(ms(3000)).is_empty());
        assert_eq!(k.handle(release(Key::Right), ms(3100)), vec![KeyAction::TogglePause]);
    }

    #[test]
    fn upgraded_right_does_not_toggle_pause() {
        let mut k = KeyActions::default();
        k.handle(press(Key::Right), ms(0));
        assert_eq!(k.handle(press(Key::Up), ms(100)), vec![KeyAction::Adjust(10)]);
        assert!(k.handle(release(Key::Up), ms(200)).is_empty());
    }

    #[test]
    fn left_short_toggles_view() {
        let mut k = KeyActions::default();
        k.handle(press(Key::Left), ms(0));
        assert!(k.tick(ms(300)).is_empty());
        assert_eq!(k.handle(release(Key::Left), ms(300)), vec![KeyAction::ToggleView]);
    }

    #[test]
    fn left_hint_then_release_toggles_and_hides() {
        let mut k = KeyActions::default();
        k.handle(press(Key::Left), ms(0));
        assert_eq!(k.tick(ms(500)), vec![KeyAction::StopHint(true)]);
        assert_eq!(
            k.handle(release(Key::Left), ms(1200)),
            vec![KeyAction::ToggleView, KeyAction::StopHint(false)]
        );
    }

    #[test]
    fn left_held_stops_once_and_release_does_nothing() {
        let mut k = KeyActions::default();
        k.handle(press(Key::Left), ms(0));
        assert_eq!(k.tick(ms(2000)), vec![KeyAction::Stop, KeyAction::StopHint(false)]);
        assert!(k.tick(ms(2500)).is_empty());
        assert!(k.handle(release(Key::Left), ms(3000)).is_empty());
    }

    #[test]
    fn connection_loss_releases_without_action() {
        let mut k = KeyActions::default();
        k.handle(press(Key::Up), ms(0));
        assert!(k.handle(KeyInput::Connected(false), ms(100)).is_empty());
        assert!(k.tick(ms(2000)).is_empty());

        k.handle(press(Key::Left), ms(3000));
        k.tick(ms(3500));
        assert_eq!(
            k.handle(KeyInput::Connected(false), ms(3600)),
            vec![KeyAction::StopHint(false)]
        );
        assert!(k.tick(ms(6000)).is_empty());
    }

    #[test]
    fn press_replacing_hinted_left_hides_hint() {
        let mut k = KeyActions::default();
        k.handle(press(Key::Left), ms(0));
        assert_eq!(k.tick(ms(500)), vec![KeyAction::StopHint(true)]);
        assert_eq!(
            k.handle(press(Key::Up), ms(600)),
            vec![KeyAction::StopHint(false), KeyAction::Adjust(10)]
        );
    }

    #[test]
    fn next_deadline_is_stop_time_once_hint_shown() {
        let mut k = KeyActions::default();
        k.handle(press(Key::Left), ms(100));
        k.tick(ms(600));
        assert_eq!(k.next_deadline(), Some(ms(2100)));
    }

    #[test]
    fn next_deadline_tracks_hold() {
        let mut k = KeyActions::default();
        assert_eq!(k.next_deadline(), None);
        k.handle(press(Key::Up), ms(1000));
        assert_eq!(k.next_deadline(), Some(ms(1600)));
        k.handle(release(Key::Up), ms(1100));
        k.handle(press(Key::Left), ms(2000));
        assert_eq!(k.next_deadline(), Some(ms(2500)));
        k.tick(ms(4000));
        assert_eq!(k.next_deadline(), None);
    }
}
