//! Shared identifiers for the Ajazz AKP03E / AKP03R device family and thin
//! enumeration helpers built on top of `mirajazz::device::list_devices`.
//!
//! `main.rs` and `map.rs` each connect to hardware directly and keep their own
//! copies of the query/protocol constants (matching values, kept in sync by
//! convention rather than by sharing this module, to avoid coupling the binary's
//! and the wizard's connection setup to each other). This module exists so
//! `tests/hardware.rs` has one place to get the same identifiers from, and a
//! `is_present` check to decide whether hardware-gated tests should run at all,
//! without needing a fourth private copy of the same constants.

use mirajazz::device::{list_devices, DeviceQuery};
use mirajazz::error::MirajazzError;
use mirajazz::types::{HidDevice, ImageFormat, ImageMirroring, ImageMode, ImageRotation};

/// USB HID query matching every supported Ajazz AKP03E / AKP03R keypad.
///
/// Mirrors the private `QUERY` constants in `main.rs` and `map.rs`.
pub const QUERY: DeviceQuery = DeviceQuery::new(65440, 1, 0x0300, 0x3002);

/// Protocol version used to connect, identical to `main.rs` and `map.rs`.
pub const PROTOCOL_VERSION: usize = 2;

/// Key count used to connect before a device's real key count is confirmed,
/// identical to `map.rs`'s `DEFAULT_KEY_COUNT`. Hardware tests that only need
/// *a* connection (not an accurate key count) reuse this rather than reading
/// a config.
pub const DEFAULT_KEY_COUNT: usize = 9;

/// Encoder count used to connect before a device's real encoder count is
/// confirmed, identical to `map.rs`'s `DEFAULT_ENCODER_COUNT`.
pub const DEFAULT_ENCODER_COUNT: usize = 3;

/// The image format every button's 60x60 JPEG display expects, identical to
/// the `IMAGE_FORMAT` constants in `main.rs` and `map.rs`.
pub const IMAGE_FORMAT: ImageFormat = ImageFormat {
    mode: ImageMode::JPEG,
    size: (60, 60),
    // The device's LCDs display images rotated 90 degrees clockwise.
    rotation: ImageRotation::Rot90,
    mirror: ImageMirroring::None,
};

/// Lists every attached device matching [`QUERY`]: the family of Ajazz keypads
/// DAK supports. Returns an empty vector (not an error) when nothing is
/// attached; only returns `Err` when the HID backend itself fails to enumerate.
pub async fn discover() -> Result<Vec<HidDevice>, MirajazzError> {
    Ok(list_devices(&[QUERY]).await?.into_iter().collect())
}

/// Whether at least one supported device is currently attached.
///
/// Used by hardware-gated tests to decide whether to run or skip themselves.
/// Treats an enumeration error the same as "not present": callers only need a
/// yes/no answer to decide whether to attempt anything hardware-related, not
/// the failure reason (which, if it matters, `discover` itself still reports).
pub async fn is_present() -> bool {
    matches!(discover().await, Ok(devices) if !devices.is_empty())
}
