//! Decodes the handlebar keypad from the RS232 modem lines.
//!
//! The four keys pull the input lines of the serial port high in combinations measured on
//! the keypad: UP = CTS+RI, DOWN = DSR, LEFT = DCD, RIGHT = CTS alone. A line state only
//! counts once it has been stable for [`STABLE`] (the lines bounce while a key is pressed
//! or released). During one press the key with the most lines wins, so RIGHT may upgrade to
//! UP (CTS comes before RI) but nothing else changes the key until all lines are off.

use std::time::Duration;

use serde::Serialize;

/// How often the bridge samples the modem lines.
pub const POLL: Duration = Duration::from_millis(20);
/// How long a line state must stay unchanged before it counts.
pub const STABLE: Duration = Duration::from_millis(50);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ModemLines {
    pub cts: bool,
    pub dsr: bool,
    pub ri: bool,
    pub dcd: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Key {
    Up,
    Down,
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum KeyEvent {
    Press(Key),
    Release(Key),
}

/// The key for exactly this line combination; all off and unknown combinations give `None`.
pub fn key_of(lines: ModemLines) -> Option<Key> {
    match (lines.cts, lines.dsr, lines.ri, lines.dcd) {
        (true, false, true, false) => Some(Key::Up),
        (false, true, false, false) => Some(Key::Down),
        (false, false, false, true) => Some(Key::Left),
        (true, false, false, false) => Some(Key::Right),
        _ => None,
    }
}

/// Turns timed line samples into press and release events.
pub struct KeyDecoder {
    /// Latest sample and when it was first seen.
    candidate: ModemLines,
    since: Duration,
    /// Last state that was stable long enough.
    stable: ModemLines,
    /// Key of the running press.
    pressed: Option<Key>,
}

impl KeyDecoder {
    pub fn new() -> Self {
        Self {
            candidate: ModemLines::default(),
            since: Duration::ZERO,
            stable: ModemLines::default(),
            pressed: None,
        }
    }

    /// Feeds one sample taken at `at` (monotonic time).
    pub fn update(&mut self, lines: ModemLines, at: Duration) -> Option<KeyEvent> {
        if lines != self.candidate {
            self.candidate = lines;
            self.since = at;
        }
        if at.saturating_sub(self.since) < STABLE || self.candidate == self.stable {
            return None;
        }
        self.stable = self.candidate;
        if self.stable == ModemLines::default() {
            return self.pressed.take().map(KeyEvent::Release);
        }
        match (key_of(self.stable), self.pressed) {
            (Some(key), None) => {
                self.pressed = Some(key);
                Some(KeyEvent::Press(key))
            }
            (Some(Key::Up), Some(Key::Right)) => {
                self.pressed = Some(Key::Up);
                Some(KeyEvent::Press(Key::Up))
            }
            _ => None,
        }
    }

    /// Ends a running press, e.g. when the line source is lost.
    pub fn reset(&mut self) -> Option<KeyEvent> {
        self.candidate = ModemLines::default();
        self.stable = ModemLines::default();
        self.pressed.take().map(KeyEvent::Release)
    }
}

impl Default for KeyDecoder {
    fn default() -> Self {
        Self::new()
    }
}

fn key_name(key: Key) -> &'static str {
    match key {
        Key::Up => "UP",
        Key::Down => "DOWN",
        Key::Left => "LEFT",
        Key::Right => "RIGHT",
    }
}

/// Wire format between bridge and server, one line per event.
pub fn encode(ev: KeyEvent) -> String {
    match ev {
        KeyEvent::Press(k) => format!("press {}\n", key_name(k)),
        KeyEvent::Release(k) => format!("release {}\n", key_name(k)),
    }
}

/// Inverse of [`encode`]; surrounding whitespace is ignored.
pub fn parse(line: &str) -> Option<KeyEvent> {
    let mut parts = line.split_whitespace();
    let action = parts.next()?;
    let key = match parts.next()? {
        "UP" => Key::Up,
        "DOWN" => Key::Down,
        "LEFT" => Key::Left,
        "RIGHT" => Key::Right,
        _ => return None,
    };
    if parts.next().is_some() {
        return None;
    }
    match action {
        "press" => Some(KeyEvent::Press(key)),
        "release" => Some(KeyEvent::Release(key)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OFF: ModemLines = ModemLines { cts: false, dsr: false, ri: false, dcd: false };
    const UP: ModemLines = ModemLines { cts: true, dsr: false, ri: true, dcd: false };
    const RIGHT: ModemLines = ModemLines { cts: true, dsr: false, ri: false, dcd: false };
    const LEFT: ModemLines = ModemLines { cts: false, dsr: false, ri: false, dcd: true };

    /// Feeds `lines` every 20 ms from `*t` for `ms` and advances `*t`.
    fn feed(
        d: &mut KeyDecoder,
        t: &mut u64,
        lines: ModemLines,
        ms: u64,
    ) -> Vec<(u64, KeyEvent)> {
        let mut out = Vec::new();
        let end = *t + ms;
        while *t < end {
            if let Some(ev) = d.update(lines, Duration::from_millis(*t)) {
                out.push((*t, ev));
            }
            *t += 20;
        }
        out
    }

    fn events(v: &[(u64, KeyEvent)]) -> Vec<KeyEvent> {
        v.iter().map(|(_, e)| *e).collect()
    }

    #[test]
    fn maps_the_measured_lines() {
        assert_eq!(key_of(UP), Some(Key::Up));
        assert_eq!(key_of(ModemLines { dsr: true, ..OFF }), Some(Key::Down));
        assert_eq!(key_of(LEFT), Some(Key::Left));
        assert_eq!(key_of(RIGHT), Some(Key::Right));
        assert_eq!(key_of(OFF), None);
        let all = ModemLines { cts: true, dsr: true, ri: true, dcd: true };
        assert_eq!(key_of(all), None);
        assert_eq!(key_of(ModemLines { dsr: true, cts: true, ..OFF }), None);
    }

    #[test]
    fn up_press_and_release() {
        let mut d = KeyDecoder::new();
        let mut t = 0;
        assert!(feed(&mut d, &mut t, OFF, 100).is_empty());
        let pressed = feed(&mut d, &mut t, UP, 200);
        assert_eq!(events(&pressed), vec![KeyEvent::Press(Key::Up)]);
        assert!(pressed[0].0 >= 150);
        let released = feed(&mut d, &mut t, OFF, 200);
        assert_eq!(events(&released), vec![KeyEvent::Release(Key::Up)]);
        assert!(released[0].0 >= 300 + 50 && released[0].0 <= 300 + 80);
    }

    #[test]
    fn short_glitches_are_ignored() {
        let mut d = KeyDecoder::new();
        let mut t = 0;
        let mut all = feed(&mut d, &mut t, RIGHT, 100);
        let dsr_cts = ModemLines { cts: true, dsr: true, ..OFF };
        all.extend(feed(&mut d, &mut t, dsr_cts, 20));
        all.extend(feed(&mut d, &mut t, RIGHT, 100));
        assert_eq!(events(&all), vec![KeyEvent::Press(Key::Right)]);

        let mut d = KeyDecoder::new();
        let mut t = 0;
        let four = ModemLines { cts: true, dsr: true, ri: true, dcd: true };
        let mut all = feed(&mut d, &mut t, OFF, 100);
        all.extend(feed(&mut d, &mut t, four, 20));
        all.extend(feed(&mut d, &mut t, OFF, 100));
        assert!(all.is_empty());
    }

    #[test]
    fn right_upgrades_to_up_and_never_pauses() {
        let mut d = KeyDecoder::new();
        let mut t = 0;
        let mut all = feed(&mut d, &mut t, RIGHT, 100);
        all.extend(feed(&mut d, &mut t, UP, 200));
        all.extend(feed(&mut d, &mut t, OFF, 100));
        assert_eq!(
            events(&all),
            vec![
                KeyEvent::Press(Key::Right),
                KeyEvent::Press(Key::Up),
                KeyEvent::Release(Key::Up)
            ]
        );
    }

    #[test]
    fn up_release_with_ri_dropping_first_stays_up() {
        let mut d = KeyDecoder::new();
        let mut t = 0;
        let mut all = feed(&mut d, &mut t, UP, 200);
        all.extend(feed(&mut d, &mut t, RIGHT, 80));
        all.extend(feed(&mut d, &mut t, OFF, 100));
        assert_eq!(
            events(&all),
            vec![KeyEvent::Press(Key::Up), KeyEvent::Release(Key::Up)]
        );
    }

    #[test]
    fn reset_releases_running_press() {
        let mut d = KeyDecoder::new();
        let mut t = 0;
        let all = feed(&mut d, &mut t, LEFT, 200);
        assert_eq!(events(&all), vec![KeyEvent::Press(Key::Left)]);
        assert_eq!(d.reset(), Some(KeyEvent::Release(Key::Left)));
        assert_eq!(d.reset(), None);
    }

    #[test]
    fn wire_roundtrip() {
        for key in [Key::Up, Key::Down, Key::Left, Key::Right] {
            for ev in [KeyEvent::Press(key), KeyEvent::Release(key)] {
                assert_eq!(parse(&encode(ev)), Some(ev));
            }
        }
        assert_eq!(encode(KeyEvent::Press(Key::Up)), "press UP\n");
        assert_eq!(parse("press SIDEWAYS"), None);
        assert_eq!(parse("  release DOWN\r\n"), Some(KeyEvent::Release(Key::Down)));
    }
}
