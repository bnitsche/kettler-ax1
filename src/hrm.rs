//! The Bluetooth Heart Rate Measurement characteristic (0x2A37) of a chest strap.
// The ids are used by the BlueZ backend, which only exists on Linux.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

pub const HEART_RATE_SERVICE: u16 = 0x180D;
pub const HEART_RATE_MEASUREMENT: u16 = 0x2A37;
pub const BATTERY_SERVICE: u16 = 0x180F;
pub const BATTERY_LEVEL: u16 = 0x2A19;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeartRate {
    pub bpm: u16,
    /// `None`: the strap doesn't report skin contact.
    pub contact: Option<bool>,
}

impl HeartRate {
    /// The bpm if they count: above 0 and contact not reported missing.
    pub fn pulse(&self) -> Option<u16> {
        (self.bpm > 0 && self.contact != Some(false)).then_some(self.bpm)
    }
}

/// `None` for a value too short for its flags. Energy and RR fields follow the bpm and
/// are not read.
pub fn parse_measurement(value: &[u8]) -> Option<HeartRate> {
    let (&flags, rest) = value.split_first()?;
    let bpm = if flags & 0x01 != 0 {
        u16::from_le_bytes([*rest.first()?, *rest.get(1)?])
    } else {
        u16::from(*rest.first()?)
    };
    let contact = (flags & 0x04 != 0).then_some(flags & 0x02 != 0);
    Some(HeartRate { bpm, contact })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_recorded_hrm_dual_values() {
        // Recorded from a Garmin HRM-Dual: flags 0x10 (RR intervals follow), 72 bpm.
        assert_eq!(parse_measurement(&[0x10, 0x48, 0x33, 0x0b]), Some(HeartRate { bpm: 72, contact: None }));
        assert_eq!(parse_measurement(&[0x00, 0x44]).map(|h| h.bpm), Some(68));
    }

    #[test]
    fn parses_16_bit_value() {
        assert_eq!(parse_measurement(&[0x01, 0x2c, 0x01]).map(|h| h.bpm), Some(300));
    }

    #[test]
    fn contact_flags() {
        let on = parse_measurement(&[0x06, 0x50]).unwrap();
        assert_eq!((on.contact, on.pulse()), (Some(true), Some(80)));
        let off = parse_measurement(&[0x04, 0x50]).unwrap();
        assert_eq!((off.contact, off.pulse()), (Some(false), None));
    }

    #[test]
    fn zero_is_no_pulse() {
        assert_eq!(parse_measurement(&[0x00, 0x00]).unwrap().pulse(), None);
    }

    #[test]
    fn too_short_is_none() {
        assert_eq!(parse_measurement(&[]), None);
        assert_eq!(parse_measurement(&[0x00]), None);
        assert_eq!(parse_measurement(&[0x01, 0x2c]), None);
    }
}
