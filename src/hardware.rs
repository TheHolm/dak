//! Shared identifiers for the whole family of Ajazz- and Mirabox-branded "stream
//! controller" HID keypads `mirajazz` targets, and thin enumeration helpers built
//! on top of `mirajazz::device::list_devices`.
//!
//! Ajazz and Mirabox sell the same OEM hardware under different (and sometimes
//! several different) USB vendor/product IDs - see `vendor/mirajazz-freebsd/README.md`
//! and the module doc comment on [`Kind`]. `main.rs` and `map.rs` used to keep their
//! own private copies of a single query/protocol/image-format constant "by
//! convention" rather than sharing this module; now that the family this project
//! recognizes has grown to thirteen VID/PID pairs, keeping that duplicated across
//! three files would just invite the copies drifting out of sync, so this module is
//! now the single canonical source both `main.rs` and `map.rs` import directly, in
//! addition to being where `tests/hardware.rs` gets the same identifiers from and
//! where `is_present` lives to decide whether hardware-gated tests should run at all.
//!
//! Only [`Kind::Akp03ERev2`] (`0300:3002`) has actually been tested against real
//! hardware by this project. See that variant's doc comment and the README's "Help
//! me support more devices" section.

use mirajazz::device::{list_devices, Device, DeviceQuery};
use mirajazz::error::MirajazzError;
use mirajazz::types::{HidDevice, ImageFormat, ImageMirroring, ImageMode, ImageRotation};

/// Ajazz's own USB vendor ID, used by five of the thirteen [`Kind`]s.
pub const AJAZZ_VID: u16 = 0x0300;
/// One of the two USB vendor IDs Mirabox N3 units have been reported under.
pub const MIRABOX_6602_VID: u16 = 0x6602;
/// The other USB vendor ID Mirabox N3 units have been reported under.
pub const MIRABOX_6603_VID: u16 = 0x6603;
/// USB vendor ID used by the Soomfon-branded rebrand.
pub const SOOMFON_VID: u16 = 0x1500;
/// USB vendor ID used by the Mars Gaming-branded rebrand.
pub const MARS_GAMING_VID: u16 = 0x0B00;
/// USB vendor ID used by the TreasLin-branded rebrand.
pub const TREASLIN_VID: u16 = 0x5548;
/// USB vendor ID used by the Redragon-branded rebrand.
pub const REDRAGON_VID: u16 = 0x0200;

/// One recognized member of the Ajazz/Mirabox "stream controller" device family.
///
/// Every variant except [`Kind::Akp03ERev2`] is wired up purely from the public
/// "Supported devices" list and `mappings.rs` of
/// <https://github.com/4ndv/opendeck-akp03> - the reference OpenDeck plugin the
/// `mirajazz` author built for exactly this device family - and has **not** been
/// independently verified against real hardware by this project. `dak`/`dak --map`
/// will attempt real protocol-level communication with any of them; best case that
/// works out of the box, worst case it connects but renders images incorrectly, or
/// fails to connect cleanly (an error, not a crash). See the README's "Help me
/// support more devices" section to report either outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Ajazz AKP03 (`0300:1001`).
    Akp03,
    /// Ajazz AKP03E (`0300:1002`).
    Akp03E,
    /// Ajazz AKP03R (`0300:1003`).
    Akp03R,
    /// Ajazz AKP03E "rev. 2" (`0300:3002`) - this project's one hardware-verified
    /// device. See [`Kind::protocol_version`]'s doc comment.
    Akp03ERev2,
    /// Ajazz AKP03R "rev. 2" (`0300:3003`).
    Akp03RRev2,
    /// Mirabox N3 (`6602:1000`).
    MiraboxN3_6602_1000,
    /// Mirabox N3 (`6602:1002`).
    MiraboxN3_6602_1002,
    /// Mirabox N3 (`6603:1002`).
    MiraboxN3_6603_1002,
    /// Mirabox N3 (`6603:1003`).
    MiraboxN3_6603_1003,
    /// Soomfon Stream Controller SE (`1500:3001`).
    SoomfonSe,
    /// Mars Gaming MSD-TWO (`0B00:1001`).
    MarsGamingMsdTwo,
    /// TreasLin N3 (`5548:1001`).
    TreasLinN3,
    /// Redragon Skyrider SS-551 (`0200:2000`).
    RedragonSs551,
}

impl Kind {
    /// Matches a discovered device's USB vendor/product ID to a known family member.
    ///
    /// Returns `None` for anything not in [`QUERIES`]. That should not happen for a
    /// device [`discover`]/`list_devices` already filtered through [`QUERIES`] (every
    /// pair matched there has a corresponding arm here), but every call site handles
    /// `None` explicitly rather than assuming that invariant holds forever.
    pub const fn from_vid_pid(vendor_id: u16, product_id: u16) -> Option<Kind> {
        match (vendor_id, product_id) {
            (AJAZZ_VID, 0x1001) => Some(Kind::Akp03),
            (AJAZZ_VID, 0x1002) => Some(Kind::Akp03E),
            (AJAZZ_VID, 0x1003) => Some(Kind::Akp03R),
            (AJAZZ_VID, 0x3002) => Some(Kind::Akp03ERev2),
            (AJAZZ_VID, 0x3003) => Some(Kind::Akp03RRev2),
            (MIRABOX_6602_VID, 0x1000) => Some(Kind::MiraboxN3_6602_1000),
            (MIRABOX_6602_VID, 0x1002) => Some(Kind::MiraboxN3_6602_1002),
            (MIRABOX_6603_VID, 0x1002) => Some(Kind::MiraboxN3_6603_1002),
            (MIRABOX_6603_VID, 0x1003) => Some(Kind::MiraboxN3_6603_1003),
            (SOOMFON_VID, 0x3001) => Some(Kind::SoomfonSe),
            (MARS_GAMING_VID, 0x1001) => Some(Kind::MarsGamingMsdTwo),
            (TREASLIN_VID, 0x1001) => Some(Kind::TreasLinN3),
            (REDRAGON_VID, 0x2000) => Some(Kind::RedragonSs551),
            _ => None,
        }
    }

    /// Human-readable name for logs and the `--map` device picker.
    ///
    /// Not the name reported by the USB stack: several of these devices report
    /// near-useless generic strings (this project's own tested unit reports
    /// "HOTSPOTEKUSB HID DEMO"). These names are DAK's own, matching the names
    /// `opendeck-akp03` uses for the same kinds.
    pub const fn human_name(&self) -> &'static str {
        match self {
            Kind::Akp03 => "Ajazz AKP03",
            Kind::Akp03E => "Ajazz AKP03E",
            Kind::Akp03R => "Ajazz AKP03R",
            Kind::Akp03ERev2 => "Ajazz AKP03E (rev. 2)",
            Kind::Akp03RRev2 => "Ajazz AKP03R (rev. 2)",
            Kind::MiraboxN3_6602_1000 => "Mirabox N3 (6602:1000)",
            Kind::MiraboxN3_6602_1002 => "Mirabox N3 (6602:1002)",
            Kind::MiraboxN3_6603_1002 => "Mirabox N3 (6603:1002)",
            Kind::MiraboxN3_6603_1003 => "Mirabox N3 (6603:1003)",
            Kind::SoomfonSe => "Soomfon Stream Controller SE",
            Kind::MarsGamingMsdTwo => "Mars Gaming MSD-TWO",
            Kind::TreasLinN3 => "TreasLin N3",
            Kind::RedragonSs551 => "Redragon Skyrider SS-551",
        }
    }

    /// Protocol version to connect with (see `mirajazz::device::Device::connect`).
    ///
    /// For every kind except [`Kind::Akp03ERev2`] this is copied as-is from
    /// `opendeck-akp03`'s own `Kind::protocol_version` - unverified by this project,
    /// see [`Kind`]'s doc comment.
    ///
    /// [`Kind::Akp03ERev2`] (`0300:3002`) is this project's one real, hardware-tested
    /// device, and deliberately does **not** match that same upstream table (which
    /// claims protocol version 3 for this exact ID, paired with a 64x64 image format -
    /// see [`Kind::image_format`]): `2` (with a 60x60 image) is what DAK has actually
    /// shipped and had a human look at and confirm correct against a physical unit,
    /// across multiple releases, including the raw button/encoder input path
    /// (`tests/hardware_read_loop.rs`), with zero observed instability across every
    /// test run while making this change too (including deliberately repeated,
    /// sustained real button/encoder activity meant to reproduce protocol version
    /// 3's failures below). Protocol version 3/64x64 was tried against that same
    /// real unit: static image rendering looked correct, and short (~15s) raw-input
    /// reads correctly captured button presses - but on two separate longer (~45s)
    /// raw-input reads with real, sustained button/encoder activity, the device
    /// dropped off the USB bus entirely partway through (screen went blank, gone
    /// from `lsusb`), both times shortly after a burst of encoder-related activity;
    /// one needed a physical unplug/replug to recover, the other re-enumerated on
    /// its own. So this is not merely "less verified": protocol version 3 has
    /// reproducibly demonstrated real instability under sustained use on this exact
    /// device (twice out of three extended attempts), on top of not matching what
    /// has actually shipped. `2`/60x60 stays authoritative here for both reasons.
    pub const fn protocol_version(&self) -> usize {
        match self {
            Kind::Akp03ERev2 => 2,
            Kind::Akp03RRev2
            | Kind::MiraboxN3_6603_1002
            | Kind::MiraboxN3_6603_1003
            | Kind::SoomfonSe
            | Kind::TreasLinN3
            | Kind::RedragonSs551 => 3,
            Kind::Akp03
            | Kind::Akp03E
            | Kind::Akp03R
            | Kind::MiraboxN3_6602_1000
            | Kind::MiraboxN3_6602_1002
            | Kind::MarsGamingMsdTwo => 2,
        }
    }

    /// Image format for this kind's button screens.
    ///
    /// See [`Kind::protocol_version`]'s doc comment for why [`Kind::Akp03ERev2`]
    /// deliberately does not follow the same protocol-version-keyed split
    /// `opendeck-akp03` uses for every other kind here.
    pub const fn image_format(&self) -> ImageFormat {
        if matches!(self, Kind::Akp03ERev2) {
            return ImageFormat {
                mode: ImageMode::JPEG,
                size: (60, 60),
                // The device's LCD displays images rotated 90 degrees clockwise.
                rotation: ImageRotation::Rot90,
                mirror: ImageMirroring::None,
            };
        }

        if self.protocol_version() == 3 {
            ImageFormat {
                mode: ImageMode::JPEG,
                size: (64, 64),
                rotation: ImageRotation::Rot90,
                mirror: ImageMirroring::None,
            }
        } else {
            ImageFormat {
                mode: ImageMode::JPEG,
                size: (60, 60),
                rotation: ImageRotation::Rot0,
                mirror: ImageMirroring::None,
            }
        }
    }
}

/// USB HID queries matching every recognized [`Kind`], in the same order as the
/// enum's variants.
pub const QUERIES: [DeviceQuery; 13] = [
    DeviceQuery::new(65440, 1, AJAZZ_VID, 0x1001),
    DeviceQuery::new(65440, 1, AJAZZ_VID, 0x1002),
    DeviceQuery::new(65440, 1, AJAZZ_VID, 0x1003),
    DeviceQuery::new(65440, 1, AJAZZ_VID, 0x3002),
    DeviceQuery::new(65440, 1, AJAZZ_VID, 0x3003),
    DeviceQuery::new(65440, 1, MIRABOX_6602_VID, 0x1000),
    DeviceQuery::new(65440, 1, MIRABOX_6602_VID, 0x1002),
    DeviceQuery::new(65440, 1, MIRABOX_6603_VID, 0x1002),
    DeviceQuery::new(65440, 1, MIRABOX_6603_VID, 0x1003),
    DeviceQuery::new(65440, 1, SOOMFON_VID, 0x3001),
    DeviceQuery::new(65440, 1, MARS_GAMING_VID, 0x1001),
    DeviceQuery::new(65440, 1, TREASLIN_VID, 0x1001),
    DeviceQuery::new(65440, 1, REDRAGON_VID, 0x2000),
];

/// Key count used to connect before a device's real key count is confirmed, shared
/// by every [`Kind`] (matching `opendeck-akp03`'s own fixed `KEY_COUNT`). `dak --map`
/// then asks the user to confirm (or manually correct) the real count for the
/// mapping itself; `run_device` uses each config device definition's own count once
/// `--map` has recorded it.
pub const DEFAULT_KEY_COUNT: usize = 9;

/// Encoder count used to connect before a device's real encoder count is confirmed,
/// shared by every [`Kind`]. See [`DEFAULT_KEY_COUNT`]'s doc comment.
pub const DEFAULT_ENCODER_COUNT: usize = 3;

/// Lists every attached device matching [`QUERIES`]: the whole family of Ajazz/
/// Mirabox keypads DAK recognizes (see [`Kind`]'s doc comment on how much of that
/// family is actually verified to work). Returns an empty vector (not an error)
/// when nothing is attached; only returns `Err` when the HID backend itself fails
/// to enumerate.
pub async fn discover() -> Result<Vec<HidDevice>, MirajazzError> {
    Ok(list_devices(&QUERIES).await?.into_iter().collect())
}

/// Whether at least one supported device is currently attached *and actually
/// connectable* - not merely visible to enumeration.
///
/// A device can be listed by [`discover`] without being usable: enumeration on Linux
/// reads device identity purely from `/sys/class/hidraw/` sysfs metadata, which needs
/// no access to the matching `/dev/hidrawN` node at all - so it can succeed even in a
/// sandbox that shares the host's `/sys` (kernel-level, hence visible) but does not
/// expose that specific device node into `/dev` (e.g. a container that was not started
/// with it explicitly passed through). Actually opening the device is a separate step
/// that fails in exactly that case (`HidError::NotConnected`). Treating "enumerable" as
/// "usable" would make every hardware-gated test panic on that connection error instead
/// of skipping cleanly, defeating the entire point of gating them on this check - so
/// this attempts a real connect (side-effect-free: [`Device::connect`] only opens the
/// handle and reads the firmware version, never writes anything) and treats any
/// failure, whether enumeration or the connect itself, the same as "not present".
pub async fn is_present() -> bool {
    let Ok(devices) = discover().await else {
        return false;
    };
    let Some(device) = devices.first() else {
        return false;
    };
    // Every device discover() can return already matched one of QUERIES, so this
    // should always resolve; `unwrap_or` just avoids a false "not present" report
    // over a defensive fallback protocol version if that invariant is ever broken.
    let protocol_version = Kind::from_vid_pid(device.vendor_id, device.product_id)
        .map(|kind| kind.protocol_version())
        .unwrap_or(2);
    Device::connect(
        device,
        protocol_version,
        DEFAULT_KEY_COUNT,
        DEFAULT_ENCODER_COUNT,
    )
    .await
    .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every [`Kind`] variant must round-trip through [`Kind::from_vid_pid`] using
    /// the same vendor/product ID [`QUERIES`] matches it with, and every entry in
    /// [`QUERIES`] must resolve to a `Kind` (no query silently matching nothing).
    #[test]
    fn every_query_resolves_to_a_kind_and_back() {
        let kinds = [
            Kind::Akp03,
            Kind::Akp03E,
            Kind::Akp03R,
            Kind::Akp03ERev2,
            Kind::Akp03RRev2,
            Kind::MiraboxN3_6602_1000,
            Kind::MiraboxN3_6602_1002,
            Kind::MiraboxN3_6603_1002,
            Kind::MiraboxN3_6603_1003,
            Kind::SoomfonSe,
            Kind::MarsGamingMsdTwo,
            Kind::TreasLinN3,
            Kind::RedragonSs551,
        ];
        assert_eq!(kinds.len(), QUERIES.len());

        for kind in kinds {
            // DeviceQuery's fields are private, so recover the vendor/product ID
            // indirectly via `Kind`'s own tables instead of reading `QUERIES`
            // directly - this still confirms every kind is reachable and that
            // `human_name`/`protocol_version`/`image_format` don't panic for it.
            let name = kind.human_name();
            assert!(!name.is_empty());
            let version = kind.protocol_version();
            assert!(version == 2 || version == 3);
            let format = kind.image_format();
            assert!(format.size.0 > 0 && format.size.1 > 0);
        }
    }

    /// [`Kind::from_vid_pid`] must reject vendor/product ID pairs that don't belong
    /// to this family, so [`is_present`]'s defensive fallback never masks a real
    /// mismatch between [`QUERIES`] and this function during development.
    #[test]
    fn from_vid_pid_rejects_unrelated_hardware() {
        assert_eq!(Kind::from_vid_pid(0x046d, 0xc52b), None); // an unrelated Logitech receiver
        assert_eq!(Kind::from_vid_pid(AJAZZ_VID, 0xffff), None); // Ajazz VID, unknown PID
    }

    /// Every vendor/product ID pair [`QUERIES`] matches must resolve, through
    /// [`Kind::from_vid_pid`] itself, to the exact `Kind` that pair is documented as
    /// belonging to. `every_query_resolves_to_a_kind_and_back` above deliberately
    /// avoids calling `from_vid_pid` (its own doc comment explains why) so that test
    /// alone never actually exercises `from_vid_pid`'s match arms; this test drives
    /// the function directly with the same thirteen pairs `QUERIES` is built from,
    /// so every arm (and its round trip back through the pair it was matched on) is
    /// covered.
    #[test]
    fn from_vid_pid_resolves_every_known_pair() {
        let pairs = [
            (AJAZZ_VID, 0x1001, Kind::Akp03),
            (AJAZZ_VID, 0x1002, Kind::Akp03E),
            (AJAZZ_VID, 0x1003, Kind::Akp03R),
            (AJAZZ_VID, 0x3002, Kind::Akp03ERev2),
            (AJAZZ_VID, 0x3003, Kind::Akp03RRev2),
            (MIRABOX_6602_VID, 0x1000, Kind::MiraboxN3_6602_1000),
            (MIRABOX_6602_VID, 0x1002, Kind::MiraboxN3_6602_1002),
            (MIRABOX_6603_VID, 0x1002, Kind::MiraboxN3_6603_1002),
            (MIRABOX_6603_VID, 0x1003, Kind::MiraboxN3_6603_1003),
            (SOOMFON_VID, 0x3001, Kind::SoomfonSe),
            (MARS_GAMING_VID, 0x1001, Kind::MarsGamingMsdTwo),
            (TREASLIN_VID, 0x1001, Kind::TreasLinN3),
            (REDRAGON_VID, 0x2000, Kind::RedragonSs551),
        ];
        assert_eq!(pairs.len(), QUERIES.len());

        for (vendor_id, product_id, expected) in pairs {
            assert_eq!(
                Kind::from_vid_pid(vendor_id, product_id),
                Some(expected),
                "vendor {vendor_id:04x} product {product_id:04x} should resolve to {expected:?}"
            );
        }
    }

    /// The one hardware-verified [`Kind`] keeps the values this project has
    /// actually shipped and tested, regardless of what the rest of this module's
    /// data-driven table would otherwise compute for it.
    #[test]
    fn akp03e_rev2_keeps_its_verified_values() {
        assert_eq!(Kind::Akp03ERev2.protocol_version(), 2);
        let format = Kind::Akp03ERev2.image_format();
        assert_eq!(format.size, (60, 60));
        assert!(matches!(format.rotation, ImageRotation::Rot90));
    }
}
