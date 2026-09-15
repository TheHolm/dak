//! The IOCTL calls we need for the FreeBSD `hidraw(4)` backend.
//!
//! Not part of upstream async-hid: added locally for dak's FreeBSD port (see
//! `../mod.rs`). Struct layouts and ioctl numbers are transcribed from
//! `/usr/include/dev/hid/hidraw.h` (FreeBSD 14.5); the `const_assert_eq!`s below pin
//! down the sizes a C compiler on the same system actually produced, so a layout
//! mismatch fails the build instead of corrupting memory at runtime.
//!
//! `hidraw.h` documents two parallel ioctl sets on the same device node: a
//! "FreeBSD uhid(4)-compatible" one (fixed `hidraw_gen_descriptor` struct, magic
//! `'U'`, numbers 21-26) and a "Linux hidraw-compatible" one (variable-length
//! buffers sized into the ioctl request itself, matching Linux's own
//! `<linux/hidraw.h>` convention, numbers 30-41). This backend uses the fixed-struct
//! form for the report descriptor and device info (`GET_REPORT_DESC`/`GET_DEVICEINFO`,
//! whose numbers happen to be identical between `/dev/uhidN` and `/dev/hidrawN`), but
//! the Linux-compatible `HIDIOCGFEATURE`/`HIDIOCSFEATURE` for feature reports: the
//! fixed-struct `GET_REPORT`/`SET_REPORT` equivalent, tried first against real
//! hardware (see the `uhid_bsd`-era branch history), returned ENXIO for every report
//! id on this device; `HIDIOCGFEATURE`/`HIDIOCSFEATURE` (matching Linux's own
//! `read_feature_report`/`write_feature_report` implementation almost verbatim)
//! worked.

use nix::ioctl_read;
use nix::ioctl_readwrite;
use nix::ioctl_readwrite_buf;
use nix::ioctl_write_buf;
use static_assertions::const_assert_eq;

const HIDRAW_IOC_MAGIC: u8 = b'U';
/// `HIDRAW_GET_REPORT_DESC`: `_IOWR('U', 21, struct hidraw_gen_descriptor)`.
const HIDRAW_GET_REPORT_DESC_NR: u8 = 21;
/// `HIDRAW_GET_DEVICEINFO`: `_IOR('U', 112, struct hidraw_device_info)`.
const HIDRAW_GET_DEVICEINFO_NR: u8 = 112;
/// `HIDIOCSFEATURE(len)`: `_IOC(IOC_IN, 'U', 35, len)` - Linux-compatible variable
/// length buffer, first byte is the report id (0 for unnumbered reports).
const HIDIOCSFEATURE_NR: u8 = 35;
/// `HIDIOCGFEATURE(len)`: `_IOC(IOC_INOUT, 'U', 36, len)`.
const HIDIOCGFEATURE_NR: u8 = 36;

/// Mirrors `struct hidraw_gen_descriptor` from `dev/hid/hidraw.h` (itself declared
/// "Compatible with usb_gen_descriptor structure").
#[repr(C)]
pub struct HidrawGenDescriptor {
    pub hgd_data: *mut u8,
    pub hgd_lang_id: u16,
    pub hgd_maxlen: u16,
    pub hgd_actlen: u16,
    pub hgd_offset: u16,
    pub hgd_config_index: u8,
    pub hgd_string_index: u8,
    pub hgd_iface_index: u8,
    pub hgd_altif_index: u8,
    pub hgd_endpt_index: u8,
    pub hgd_report_type: u8,
    pub reserved: [u8; 8]
}
const_assert_eq!(size_of::<HidrawGenDescriptor>(), 32);

// SAFETY: `hgd_data` is only dereferenced by the kernel for the duration of the
// single ioctl(2) call that consumes this struct (see mod.rs's `read_with`/
// `write_with` usage: the struct is built, passed to one ioctl, and its result read
// back, all without any other thread touching the pointee). No aliasing across
// threads ever actually happens; this unblocks holding the struct across an `.await`
// point, which the report-descriptor fetch needs to do.
unsafe impl Send for HidrawGenDescriptor {}
unsafe impl Sync for HidrawGenDescriptor {}

/// Mirrors `struct hidraw_device_info` from `dev/hid/hidraw.h` (itself declared
/// "Compatible with usb_device_info structure" - the `occupied`/`reserved` gaps are
/// exactly where that wider struct's extra fields live, kept only for layout parity).
#[repr(C)]
#[derive(Clone)]
pub struct HidrawDeviceInfo {
    pub hdi_product: u16,
    pub hdi_vendor: u16,
    pub hdi_version: u16,
    pub occupied: [u8; 18],
    pub hdi_bustype: u16,
    pub reserved: [u8; 14],
    pub hdi_name: [u8; 128],
    pub hdi_phys: [u8; 128],
    pub hdi_uniq: [u8; 64],
    pub hdi_release: [u8; 8]
}
const_assert_eq!(size_of::<HidrawDeviceInfo>(), 368);

ioctl_readwrite!(hidraw_get_report_desc, HIDRAW_IOC_MAGIC, HIDRAW_GET_REPORT_DESC_NR, HidrawGenDescriptor);
ioctl_read!(hidraw_get_deviceinfo, HIDRAW_IOC_MAGIC, HIDRAW_GET_DEVICEINFO_NR, HidrawDeviceInfo);
ioctl_write_buf!(hidraw_set_feature, HIDRAW_IOC_MAGIC, HIDIOCSFEATURE_NR, u8);
ioctl_readwrite_buf!(hidraw_get_feature, HIDRAW_IOC_MAGIC, HIDIOCGFEATURE_NR, u8);
