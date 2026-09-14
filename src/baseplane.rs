//! Device addressing for the base plane.
//!
//! The config addresses controls through *references* naming a logical device and a
//! control on it, e.g. `"1b01"` (device 1, button 1) or `"2e01"` (device 2, encoder 1).
//! A [Reference] is the single addressing unit the scene setup/actions keys use.
//!
//! [Baseplane] is the registry of which logical device numbers are actually present.
//! The config's `devices` section declares the devices by keying each definition with its
//! logical number; the runtime matches those definitions against the discovered hardware
//! (by serial, falling back to VID:PID) and registers the numbers of the matches here.

use std::collections::BTreeSet;
use std::fmt;

/// Highest device number a reference may name.
pub const MAX_DEVICES: u8 = 9;

/// Highest button/encoder number a reference may name per device.
pub const MAX_NUMBER: u8 = 99;

/// What control a reference addresses on its device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Kind {
    /// A keypad button.
    Button,
    /// A rotary encoder.
    Encoder,
}

impl Kind {
    /// The one-letter code used inside a reference (`b` or `e`).
    pub fn code(self) -> char {
        match self {
            Kind::Button => 'b',
            Kind::Encoder => 'e',
        }
    }

    /// Human-readable plural name of the control kind, for messages.
    pub fn label(self) -> &'static str {
        match self {
            Kind::Button => "button",
            Kind::Encoder => "encoder",
        }
    }
}

/// A reference to one control on one device, formatted `<device><b|e><number>`,
/// e.g. `1b01` for device 1 button 1, `2e01` for device 2 encoder 1.
///
/// The device number is 1..=`MAX_DEVICES`, the control number 1..=`MAX_NUMBER` and
/// is always written with two digits. Devices are numbered logically: each number names
/// one device definition from the config's `devices` section.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Reference {
    /// Logical device number (`1..=MAX_DEVICES`).
    pub device: u8,
    /// Whether the reference names a button or an encoder.
    pub kind: Kind,
    /// Control number on the device (`1..=MAX_NUMBER`), 1-based.
    pub number: u8,
}

impl Reference {
    /// A button reference, e.g. `Reference::button(1, 1)` renders as `1b01`.
    pub fn button(device: u8, number: u8) -> Reference {
        Reference {
            device,
            kind: Kind::Button,
            number,
        }
    }

    /// An encoder reference, e.g. `Reference::encoder(2, 1)` renders as `2e01`.
    pub fn encoder(device: u8, number: u8) -> Reference {
        Reference {
            device,
            kind: Kind::Encoder,
            number,
        }
    }

    /// Parses a control reference like `1b01`.
    ///
    /// The input must be exactly a device digit (`1`..=MAX_DEVICES), the control
    /// letter `b`/`e`, and a two-digit zero-padded number from `01` to `MAX_NUMBER`.
    /// The error text does not repeat the offending input: callers already quote the
    /// config key the reference came from, e.g. `key "0b01" {error}`.
    pub fn parse(input: &str) -> Result<Reference, String> {
        let bytes = input.as_bytes();
        if bytes.len() != 4 {
            return Err(
                "is not a valid control reference: expected the form <device><b|e><number>, e.g. \"1b01\"".to_string(),
            );
        }
        let device = match bytes[0] {
            b'1'..=b'9' => bytes[0] - b'0',
            _ => {
                return Err(format!(
                    "is not a valid control reference: the device must be a single digit from 1 to {MAX_DEVICES}"
                ))
            }
        };
        let kind = match bytes[1] {
            b'b' => Kind::Button,
            b'e' => Kind::Encoder,
            _ => {
                return Err(
                    "is not a valid control reference: the control must be \"b\" (button) or \"e\" (encoder)".to_string(),
                )
            }
        };
        if !bytes[2].is_ascii_digit() || !bytes[3].is_ascii_digit() {
            return Err(
                "is not a valid control reference: the number must be exactly two digits"
                    .to_string(),
            );
        }
        let number = (bytes[2] - b'0') * 10 + (bytes[3] - b'0');
        if !(1..=MAX_NUMBER).contains(&number) {
            return Err(format!(
                "is not a valid control reference: the number must be from 01 to {MAX_NUMBER}"
            ));
        }
        Ok(Reference {
            device,
            kind,
            number,
        })
    }
}

impl fmt::Display for Reference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}{:02}", self.device, self.kind.code(), self.number)
    }
}

/// The set of devices the program talks to, keyed by their config numbers.
///
/// A number is present when the device definition it names in the config `devices`
/// section was matched to discovered hardware at startup. The present-set is stored
/// generically so sparse, non-sequential numberings (e.g. only devices 2, 5 and 9)
/// work unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Baseplane {
    present: BTreeSet<u8>,
}

impl Baseplane {
    /// A baseplane holding the given numbers as the present devices, e.g. the config
    /// device ids whose definitions matched the discovered hardware.
    pub fn from_present<I>(numbers: I) -> Baseplane
    where
        I: IntoIterator<Item = u8>,
    {
        Baseplane {
            present: numbers.into_iter().collect(),
        }
    }

    /// Whether a device with this config number is currently connected.
    pub fn is_present(&self, device: u8) -> bool {
        self.present.contains(&device)
    }

    /// The config numbers of the currently present devices, in ascending order.
    pub fn present_numbers(&self) -> Vec<u8> {
        self.present.iter().copied().collect()
    }

    /// The lowest present device number, e.g. the single device's `1`.
    pub fn first_present_number(&self) -> Option<u8> {
        self.present.first().copied()
    }
}

#[cfg(test)]
mod tests {
    use super::{Baseplane, Kind, Reference, MAX_DEVICES, MAX_NUMBER};

    /// Every valid reference shape parses into the expected device/kind/number.
    #[test]
    fn parse_accepts_valid_references() {
        assert_eq!(Reference::parse("1b01").unwrap(), Reference::button(1, 1));
        assert_eq!(Reference::parse("2e01").unwrap(), Reference::encoder(2, 1));
        assert_eq!(Reference::parse("9b99").unwrap(), Reference::button(9, 99));
        assert_eq!(Reference::parse("5b12").unwrap(), Reference::button(5, 12));
    }

    /// The limits named by the format are the ones the parser enforces: device 9 and
    /// control 99 parse, device 0, control 0 and control 100 do not.
    #[test]
    fn parse_enforces_format_limits() {
        assert_eq!(
            Reference::parse(format!("{MAX_DEVICES}b{MAX_NUMBER:02}").as_str()).unwrap(),
            Reference::button(MAX_DEVICES, MAX_NUMBER)
        );
        assert_eq!(
            Reference::parse("0b01").unwrap_err(),
            "is not a valid control reference: the device must be a single digit from 1 to 9"
        );
        assert_eq!(
            Reference::parse("1b00").unwrap_err(),
            "is not a valid control reference: the number must be from 01 to 99"
        );
        assert_eq!(
            Reference::parse("1b100").unwrap_err(),
            "is not a valid control reference: expected the form <device><b|e><number>, e.g. \"1b01\""
        );
    }

    /// Wrong length, a non-\"b\"/\"e\" control letter and non-digit number bytes are
    /// each reported with their own reason.
    #[test]
    fn parse_rejects_malformed_references() {
        assert_eq!(
            Reference::parse("1b01extra").unwrap_err(),
            "is not a valid control reference: expected the form <device><b|e><number>, e.g. \"1b01\""
        );
        assert_eq!(
            Reference::parse("a1").unwrap_err(),
            "is not a valid control reference: expected the form <device><b|e><number>, e.g. \"1b01\""
        );
        assert_eq!(
            Reference::parse("1x01").unwrap_err(),
            "is not a valid control reference: the control must be \"b\" (button) or \"e\" (encoder)"
        );
        assert_eq!(
            Reference::parse("1ba1").unwrap_err(),
            "is not a valid control reference: the number must be exactly two digits"
        );
        assert_eq!(
            Reference::parse("1e1").unwrap_err(),
            "is not a valid control reference: expected the form <device><b|e><number>, e.g. \"1b01\""
        );
    }

    /// The `b`/`e` code letters and labels describe the two control kinds.
    #[test]
    fn kind_code_and_label_describe_control() {
        assert_eq!(Kind::Button.code(), 'b');
        assert_eq!(Kind::Encoder.code(), 'e');
        assert_eq!(Kind::Button.label(), "button");
        assert_eq!(Kind::Encoder.label(), "encoder");
    }

    /// A reference renders as exactly the zero-padded config spelling.
    #[test]
    fn reference_displays_zero_padded() {
        assert_eq!(Reference::button(1, 1).to_string(), "1b01");
        assert_eq!(Reference::encoder(2, 1).to_string(), "2e01");
        assert_eq!(Reference::button(9, 99).to_string(), "9b99");
    }

    /// A baseplane built from a set of numbers reports exactly those present.
    #[test]
    fn from_present_presents_only_given_numbers() {
        let baseplane = Baseplane::from_present([1]);
        assert!(baseplane.is_present(1));
        assert!(!baseplane.is_present(2));
        assert!(!baseplane.is_present(9));
        assert_eq!(baseplane.present_numbers(), vec![1]);
        assert_eq!(baseplane.first_present_number(), Some(1));
    }

    /// Sparse, non-sequential numberings keep only the given devices present.
    #[test]
    fn from_present_keeps_sparse_numbering() {
        let baseplane = Baseplane::from_present([9, 2]);
        assert!(baseplane.is_present(2));
        assert!(baseplane.is_present(9));
        assert!(!baseplane.is_present(1));
        assert_eq!(baseplane.present_numbers(), vec![2, 9]);
        assert_eq!(baseplane.first_present_number(), Some(2));
    }

    /// A default baseplane is empty: no device is present.
    #[test]
    fn default_baseplane_is_empty() {
        let baseplane = Baseplane::default();
        assert!(!baseplane.is_present(1));
        assert_eq!(baseplane.first_present_number(), None);
    }
}
