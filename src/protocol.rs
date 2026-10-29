use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Maximum number of bytes buffered while waiting for a line ending.
const MAX_LINE_BYTES: usize = 256;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParseError {
    #[error("expected 8 tab-separated fields, got {0}")]
    FieldCount(usize),
    #[error("invalid number in field {field}: {value:?}")]
    InvalidNumber { field: &'static str, value: String },
    #[error("invalid elapsed time: {0:?}")]
    InvalidTime(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Status {
    pub pulse: u16,
    pub rpm: u16,
    pub speed_x10: u16,
    pub distance: u16,
    pub target_watts: u16,
    pub energy: u16,
    /// Training time, or the remaining time of a recovery measurement.
    pub elapsed_secs: u32,
    pub actual_watts: u16,
    /// The cockpit runs its recovery (heart rate recovery) measurement: it then writes
    /// the time as `mm;ss`, counting down from 00;59; at 00;00 `pulse` holds its result.
    pub recovery: bool,
}

fn number(field: &'static str, value: &str) -> Result<u16, ParseError> {
    value.trim_matches(' ').parse().map_err(|_| ParseError::InvalidNumber {
        field,
        value: value.to_string(),
    })
}

/// `mm:ss`, or `mm;ss` during a recovery measurement (then `true`).
fn elapsed(value: &str) -> Result<(u32, bool), ParseError> {
    let err = || ParseError::InvalidTime(value.to_string());
    let trimmed = value.trim_matches(' ');
    let (recovery, (min, sec)) = match trimmed.split_once(':') {
        Some(parts) => (false, parts),
        None => (true, trimmed.split_once(';').ok_or_else(err)?),
    };
    let min: u32 = min.parse().map_err(|_| err())?;
    let sec: u32 = sec.parse().map_err(|_| err())?;
    if sec >= 60 {
        return Err(err());
    }
    let secs = min.checked_mul(60).and_then(|m| m.checked_add(sec)).ok_or_else(err)?;
    Ok((secs, recovery))
}

pub fn parse_status(line: &str) -> Result<Status, ParseError> {
    let f: Vec<&str> = line
        .trim_matches(|c| c == '\r' || c == '\n' || c == ' ')
        .split('\t')
        .collect();
    if f.len() != 8 {
        return Err(ParseError::FieldCount(f.len()));
    }
    let (elapsed_secs, recovery) = elapsed(f[6])?;
    Ok(Status {
        pulse: number("pulse", f[0])?,
        rpm: number("rpm", f[1])?,
        speed_x10: number("speed", f[2])?,
        distance: number("distance", f[3])?,
        target_watts: number("target_watts", f[4])?,
        energy: number("energy", f[5])?,
        elapsed_secs,
        actual_watts: number("actual_watts", f[7])?,
        recovery,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    #[allow(dead_code)] // part of the AX1 command set, not used by the server
    Reset,
    PcMode,
    Id,
    Version,
    Status,
    SetPower(u16),
}

impl Command {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Command::Reset => b"RS\r\n".to_vec(),
            Command::PcMode => b"CM\r\n".to_vec(),
            Command::Id => b"ID\r\n".to_vec(),
            Command::Version => b"VE\r\n".to_vec(),
            Command::Status => b"ST\r\n".to_vec(),
            Command::SetPower(w) => format!("PW {w}\r\n").into_bytes(),
        }
    }

    /// The command as written, without the line end.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.encode()).trim_end().to_string()
    }
}

/// Splits a byte stream into lines terminated by `\n` (a preceding `\r` is dropped).
#[derive(Debug, Default)]
pub struct LineReader {
    buf: Vec<u8>,
}

impl LineReader {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        let mut lines = Vec::new();
        for &b in bytes {
            if b == b'\n' {
                if self.buf.last() == Some(&b'\r') {
                    self.buf.pop();
                }
                lines.push(String::from_utf8_lossy(&self.buf).into_owned());
                self.buf.clear();
            } else {
                self.buf.push(b);
            }
        }
        // Overflow without a line ending: discard the buffer, start empty.
        if self.buf.len() > MAX_LINE_BYTES {
            self.buf.clear();
        }
        lines
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DeviceLimits {
    pub min: u16,
    pub max: u16,
    pub step: u16,
}

impl Default for DeviceLimits {
    fn default() -> Self {
        Self { min: 25, max: 400, step: 5 }
    }
}

impl DeviceLimits {
    /// Rounds to the nearest multiple of `step` (ties up), then clamps to `min..=max`.
    pub fn clamp(&self, watts: i32) -> u16 {
        let step = i32::from(self.step.max(1));
        let rounded = watts.saturating_add(step / 2).div_euclid(step).saturating_mul(step);
        rounded.max(i32::from(self.min)).min(i32::from(self.max)) as u16
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_status_line() {
        let s = parse_status("000\t066\t234\t001\t080\t0004\t00:13\t080").unwrap();
        assert_eq!(
            s,
            Status {
                pulse: 0,
                rpm: 66,
                speed_x10: 234,
                distance: 1,
                target_watts: 80,
                energy: 4,
                elapsed_secs: 13,
                actual_watts: 80,
                recovery: false,
            }
        );
    }
    #[test]
    fn parses_recovery_countdown() {
        // Recorded on the AX1 during a recovery measurement.
        let s = parse_status("106	000	198	005	100	0028	00;55	000").unwrap();
        assert!(s.recovery);
        assert_eq!((s.pulse, s.elapsed_secs), (106, 55));
        let s = parse_status("006	000	198	005	100	0028	00;00	000").unwrap();
        assert!(s.recovery);
        assert_eq!((s.pulse, s.elapsed_secs), (6, 0));
        assert!(!parse_status("000	0	0	0	25	0	01:34	0").unwrap().recovery);
        assert!(parse_status("0	0	0	0	0	0	00;75	0").is_err());
        assert!(parse_status("0	0	0	0	0	0	00,10	0").is_err());
    }
    #[test]
    fn parses_minutes_in_elapsed() {
        assert_eq!(
            parse_status("000\t0\t0\t0\t25\t0\t12:05\t0").unwrap().elapsed_secs,
            725
        );
    }
    #[test]
    fn rejects_empty_leading_field() {
        assert!(parse_status("	066	234	001	080	0004	00:13	080").is_err());
        assert!(parse_status("000	066	234	001	080	0004	00:13	").is_err());
    }
    #[test]
    fn rejects_bad_elapsed() {
        assert!(parse_status("0	0	0	0	0	0	99999999:00	0").is_err());
        assert!(parse_status("0	0	0	0	0	0	00:75	0").is_err());
    }
    #[test]
    fn clamp_extremes() {
        let l = DeviceLimits::default();
        assert_eq!(l.clamp(i32::MAX), 400);
        assert_eq!(l.clamp(i32::MIN), 25);
        assert_eq!(DeviceLimits { min: 100, max: 50, step: 5 }.clamp(70), 50);
    }
    #[test]
    fn rejects_wrong_field_count() {
        assert!(parse_status("ACK").is_err());
        assert!(parse_status("000\t066").is_err());
    }
    #[test]
    fn rejects_garbage_fields() {
        assert!(parse_status("0x0\t066\t234\t001\t080\t0004\t00:13\t080").is_err());
    }
    #[test]
    fn command_text_has_no_line_end() {
        assert_eq!(Command::SetPower(160).text(), "PW 160");
        assert_eq!(Command::Version.text(), "VE");
    }

    #[test]
    fn encodes_commands() {
        assert_eq!(Command::SetPower(80).encode(), b"PW 80\r\n");
        assert_eq!(Command::PcMode.encode(), b"CM\r\n");
        assert_eq!(Command::Status.encode(), b"ST\r\n");
    }
    #[test]
    fn line_reader_handles_split_and_noise() {
        let mut r = LineReader::new();
        assert!(r.push(b"AC").is_empty());
        assert_eq!(r.push(b"K\r\nAX1S\n"), vec!["ACK", "AX1S"]);
        assert!(r.push(&[0xff; 300]).is_empty()); // overflow discards
        assert_eq!(r.push(b"018\r\n"), vec!["018"]);
    }
    #[test]
    fn limits_round_and_clamp() {
        let l = DeviceLimits::default();
        assert_eq!(l.clamp(10), 25);
        assert_eq!(l.clamp(83), 85);
        assert_eq!(l.clamp(82), 80);
        assert_eq!(l.clamp(999), 400);
    }
}
