//! A backend for FreeBSD built on the `hidraw(4)` driver.
//!
//! Not part of upstream async-hid: added locally for dak's FreeBSD port (see
//! `AGENTS.md`/the `v0.5.1-FreeBSD` branch history for background, including why an
//! earlier `uhid(4)`-based attempt was replaced by this one). `hidraw(4)` (FreeBSD
//! 13+) is explicitly designed to be Linux-`hidraw`-compatible: `/dev/hidrawN`
//! character devices, `read`/`write` semantics matching Linux's exactly (including
//! the leading report-id byte convention - no byte-shifting translation needed,
//! unlike `uhid(4)`), and a parallel "Linux hidraw-compatible" ioctl set for feature
//! reports. It requires `hw.usb.usbhid.enable=1` (off by default) and the `hidraw`
//! kernel module; without either, `uhid(4)` claims the interface instead. There is no
//! Linux-sysfs equivalent and no netlink, so enumeration and metadata retrieval both
//! go through ioctls instead (see `ioctl.rs`).
//!
//! Unlike Linux's hidraw fd, `/dev/hidrawN` on this FreeBSD (confirmed against real
//! hardware) fails `tokio::io::unix::AsyncFd::new` with EINVAL - the driver doesn't
//! support kqueue registration for read/write readiness, even though plain blocking
//! `read`/`write`/`ioctl` on the fd work fine. Writes and feature reports (bounded -
//! they always complete, successfully or not, without waiting on external input) run
//! as a genuine blocking syscall on a `tokio::task::spawn_blocking` thread each call;
//! HID reports are small (well under a KiB) and infrequent (occasional image
//! uploads, user-driven feature reads), so the thread hop and buffer copy this
//! requires are immaterial.
//!
//! Reads are different, and need a different design: `read_input_report` waits for
//! the *next* HID report, i.e. it can legitimately block indefinitely with no
//! timeout - and dak/mirajazz's own input loop (see `main.rs`'s `tokio::select!`)
//! routinely *abandons* a pending read the moment any other branch (a timer, a
//! click confirmation, ...) resolves first, exactly as `tokio::select!` is
//! documented to do with every non-winning branch. On Linux that's free (dropping a
//! `AsyncFd`-based read future just stops polling, nothing was actually running in
//! the background). Confirmed against real hardware, doing the same
//! `spawn_blocking`-per-call thing this backend does for writes is not free here: a
//! dropped read future leaves its `spawn_blocking` thread parked forever inside the
//! kernel's `read(2)`, since nothing ever tells it to stop and no more data is
//! coming - a new stuck thread accumulates on *every* abandoned read, and separately,
//! tokio's runtime teardown waits for every outstanding blocking task to finish
//! before a `#[tokio::main]` process can exit, which one of these stuck reads will
//! never do on its own - hanging the whole program on shutdown.
//!
//! So each `HidrawDevice` opened for reading instead starts exactly one dedicated,
//! plain `std::thread` (not tracked by tokio's blocking pool at all, so process exit
//! never waits on it) that loops blocking-`read`ing the fd and forwarding each
//! report over a channel; `read_input_report` just awaits the next channel message.
//! Dropping that await is always safe and cheap - it does not touch the background
//! thread, which keeps running (and is reused for every subsequent read on the same
//! handle) regardless of how many reads upstream abandons.
mod ioctl;

use std::fs::{read_dir, OpenOptions};
use std::io::ErrorKind;
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures_lite::stream::{iter, Boxed};
use futures_lite::StreamExt;
use nix::fcntl::OFlag;
use nix::unistd::{read, write};
use tokio::sync::mpsc;

use crate::backend::descriptor::HidrawReportDescriptor;
use crate::backend::hidraw_bsd::ioctl::{
    hidraw_get_deviceinfo, hidraw_get_feature, hidraw_get_report_desc, hidraw_set_feature, HidrawGenDescriptor
};
use crate::backend::{Backend, DeviceInfoStream};
use crate::utils::TryIterExt;
use crate::{ensure, AsyncHidFeatureHandle, AsyncHidRead, AsyncHidWrite, DeviceEvent, DeviceId, DeviceInfo, HidError, HidResult};

/// Input reports read one at a time from the background reader thread; a small bound
/// just provides backpressure (the thread blocks on a full channel instead of
/// growing memory unboundedly) - dak/mirajazz only ever want the latest few reports.
const REPORT_CHANNEL_CAPACITY: usize = 16;

#[derive(Default)]
pub struct HidrawBsdBackend;

impl Backend for HidrawBsdBackend {
    type Reader = HidrawDevice;
    type Writer = HidrawDevice;
    type FeatureHandle = HidrawDevice;

    async fn enumerate(&self) -> HidResult<DeviceInfoStream> {
        let paths: Vec<PathBuf> = read_dir("/dev")?
            .map(|r| r.map(|e| e.path()))
            .try_collect_vec()?
            .into_iter()
            .filter(|p| is_hidraw_path(p))
            .collect();
        let devices = paths.into_iter().map(get_device_info_raw).try_flatten();
        Ok(iter(devices).boxed())
    }

    fn watch(&self) -> HidResult<Boxed<DeviceEvent>> {
        // FreeBSD's hotplug notification mechanism is devd(8), a very different shape
        // from Linux's netlink uevents (a text-protocol Unix socket rather than a
        // kernel socket family), and dak/mirajazz never call `HidBackend::watch`.
        // Rather than silently claim support that was never exercised, this returns a
        // stream that never yields, keeping the trait satisfied without lying.
        Ok(futures_lite::stream::pending().boxed())
    }

    async fn query_info(&self, id: &DeviceId) -> HidResult<Vec<DeviceInfo>> {
        let DeviceId::DevPath(path) = id;
        get_device_info_raw(path.clone())
    }

    async fn open(&self, id: &DeviceId, read: bool, write: bool) -> HidResult<(Option<Self::Reader>, Option<Self::Writer>)> {
        let DeviceId::DevPath(path) = id;

        // No O_NONBLOCK: writes/feature reports run as genuine blocking syscalls on
        // `spawn_blocking` threads (see the module doc comment), and the dedicated
        // reader thread below wants a plain blocking `read(2)` too.
        let fd: OwnedFd = OpenOptions::new()
            .read(read)
            .write(write)
            .custom_flags(OFlag::O_CLOEXEC.bits())
            .open(path)
            .map_err(|err| match err {
                err if err.kind() == ErrorKind::NotFound => HidError::NotConnected,
                err => err.into()
            })?
            .into();

        let fd = Arc::new(fd);
        // The background reader thread (see below) is started lazily, on the first
        // actual `read_input_report` call, not here: it captures its own `Arc`
        // clone of `fd` for as long as it runs, which (confirmed against real
        // hardware) is a real problem for a handle that's never actually read from
        // (`open_feature_handle` below opens read+write but never reads) - the fd
        // would then never truly close when the handle is dropped, and FreeBSD's
        // hidraw(4) refuses a second concurrent open of the same node with EBUSY.
        let reader = read.then(|| HidrawDevice { fd: fd.clone(), reader: None, can_read: true });
        let writer = write.then(|| HidrawDevice { fd: fd.clone(), reader: None, can_read: false });

        Ok((reader, writer))
    }

    async fn open_feature_handle(&self, id: &DeviceId) -> HidResult<Self::FeatureHandle> {
        // Opened for both read and write, matching the Linux backend this is based
        // on: confirmed against real hardware that HIDIOCGFEATURE returns EPERM on a
        // write-only fd, so read permission is required even though this handle
        // itself never calls `read_input_report` (and so, per the comment in `open`,
        // never actually starts a background reader thread for it).
        let (_, writer) = self.open(id, true, true).await?;
        writer.ok_or(HidError::message("Failed to open device for feature report"))
    }
}

/// Starts the dedicated background reader thread for one opened-for-reading
/// `HidrawDevice` (see the module doc comment for why this exists instead of a
/// `spawn_blocking` call per read) and returns the receiving end of the channel it
/// forwards reports through. The thread runs until a `read(2)` call fails (including
/// when the channel's last receiver is dropped and `blocking_send` starts failing -
/// though in practice dak/mirajazz keep a reader alive for the device's whole
/// session, so this normally only happens when the device disconnects or the fd is
/// closed) or the channel fills and its send fails for any other reason.
fn spawn_background_reader(fd: Arc<OwnedFd>) -> mpsc::Receiver<std::io::Result<Vec<u8>>> {
    let (tx, rx) = mpsc::channel(REPORT_CHANNEL_CAPACITY);
    std::thread::spawn(move || {
        loop {
            let mut buf = vec![0u8; 512];
            let outcome = read(fd.as_raw_fd(), &mut buf).map(|n| {
                buf.truncate(n);
                buf
            });
            let stop_on_error = outcome.is_err();
            if tx.blocking_send(outcome.map_err(std::io::Error::from)).is_err() || stop_on_error {
                break;
            }
        }
    });
    rx
}

/// Whether `path` looks like a `hidraw(4)` device node (`/dev/hidraw0`,
/// `/dev/hidraw12`, ...).
fn is_hidraw_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("hidraw"))
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
}

/// Turns a NUL-terminated (or fully-used) fixed-size C string buffer, as returned in
/// `struct hidraw_device_info`, into a `String`. FreeBSD populates these from the
/// device's USB string descriptors, converted from UTF-16LE by the kernel, so this is
/// a best-effort, lossy conversion, matching how the Linux backend treats sysfs
/// strings.
fn cstr_buf_to_string(buf: &[u8]) -> String {
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

fn get_device_info_raw(path: PathBuf) -> HidResult<Vec<DeviceInfo>> {
    let fd = OpenOptions::new()
        .read(true)
        .custom_flags((OFlag::O_CLOEXEC | OFlag::O_NONBLOCK).bits())
        .open(&path)
        .map_err(|err| match err {
            err if err.kind() == ErrorKind::NotFound => HidError::NotConnected,
            err => err.into()
        })?;

    let mut info = MaybeUninit::<ioctl::HidrawDeviceInfo>::zeroed();
    unsafe { hidraw_get_deviceinfo(fd.as_raw_fd(), info.as_mut_ptr()) }
        .map_err(|e| HidError::message(format!("ioctl(HIDRAW_GET_DEVICEINFO) error for {path:?}: {e}")))?;
    // SAFETY: the ioctl call above succeeded, so the kernel has fully initialized
    // `info` (it's an `_IOR` ioctl: data flows kernel -> userspace only).
    let info = unsafe { info.assume_init() };

    let name = cstr_buf_to_string(&info.hdi_name);
    let manufacturer = None;
    let serial_number = Some(cstr_buf_to_string(&info.hdi_uniq)).filter(|s| !s.is_empty());

    let base = DeviceInfo {
        id: DeviceId::DevPath(path.clone()),
        name,
        manufacturer,
        product_id: info.hdi_product,
        vendor_id: info.hdi_vendor,
        usage_id: 0,
        usage_page: 0,
        serial_number
    };

    // HID report descriptors are always small in practice (the USB HID spec's own
    // examples top out in the low hundreds of bytes; this device keypad's is under
    // 100). 4KiB comfortably covers real devices without a two-call
    // size-then-fetch dance.
    let mut buf = vec![0u8; 4096];
    let mut hgd = HidrawGenDescriptor {
        hgd_data: buf.as_mut_ptr(),
        hgd_lang_id: 0,
        hgd_maxlen: buf.len() as u16,
        hgd_actlen: 0,
        hgd_offset: 0,
        hgd_config_index: 0,
        hgd_string_index: 0,
        hgd_iface_index: 0,
        hgd_altif_index: 0,
        hgd_endpt_index: 0,
        hgd_report_type: 0,
        reserved: [0; 8]
    };

    let usages: Vec<DeviceInfo> = match unsafe { hidraw_get_report_desc(fd.as_raw_fd(), &mut hgd) } {
        Ok(_) => {
            let len = (hgd.hgd_actlen as usize).min(buf.len());
            HidrawReportDescriptor::from_slice(&buf[..len])
                .map(|descriptor| {
                    descriptor
                        .usages()
                        .map(|(usage_page, usage_id)| DeviceInfo {
                            usage_page,
                            usage_id,
                            ..base.clone()
                        })
                        .collect()
                })
                .unwrap_or_default()
        }
        // A device without a report descriptor (or one this driver can't fetch it
        // for) still gets a single DeviceInfo entry below, just without usage
        // page/id filtering - matching how the Linux backend handles the same case.
        Err(_) => Vec::new()
    };

    Ok(if usages.is_empty() { vec![base] } else { usages })
}

/// Runs a blocking closure over an owned fd on the blocking thread pool, unwrapping
/// the `JoinHandle`'s outer panic-propagation `Result` (a panic inside `f` truly is a
/// bug, not a condition callers should have to handle) to leave just `f`'s own
/// `Result`. Only used for writes and feature reports, which always complete on
/// their own - see the module doc comment for why reads use a different design.
async fn blocking<T, E>(fd: Arc<OwnedFd>, f: impl FnOnce(&OwnedFd) -> Result<T, E> + Send + 'static) -> Result<T, E>
where
    T: Send + 'static,
    E: Send + 'static
{
    tokio::task::spawn_blocking(move || f(&fd))
        .await
        .expect("blocking HID I/O task panicked")
}

#[derive(Debug)]
pub struct HidrawDevice {
    fd: Arc<OwnedFd>,
    /// Lazily populated by the first `read_input_report` call (see the module doc
    /// comment for why this can't just be a `spawn_blocking` call per read, and why
    /// it must not be started eagerly in `Backend::open` either): `None` both for a
    /// handle not opened for reading at all, and for one that is but hasn't had its
    /// first read yet.
    reader: Option<mpsc::Receiver<std::io::Result<Vec<u8>>>>,
    /// Whether this handle was opened with read permission at all, so a caller
    /// mistake (calling `read_input_report` on a write/feature-only handle) panics
    /// with a clear message instead of silently starting a reader thread on a fd
    /// that may not even have `O_RDONLY`/`O_RDWR` access.
    can_read: bool
}

impl AsyncHidRead for HidrawDevice {
    async fn read_input_report<'a>(&'a mut self, buf: &'a mut [u8]) -> HidResult<usize> {
        assert!(self.can_read, "read_input_report called on a handle that wasn't opened for reading");
        let fd = self.fd.clone();
        let reader = self.reader.get_or_insert_with(|| spawn_background_reader(fd));
        let data = reader
            .recv()
            .await
            .ok_or(HidError::Disconnected)?
            .map_err(|err| match err.raw_os_error() {
                Some(nix::libc::EIO) => HidError::Disconnected,
                _ => HidError::from_backend(err)
            })?;
        let n = data.len().min(buf.len());
        buf[..n].copy_from_slice(&data[..n]);
        Ok(n)
    }
}

impl AsyncHidWrite for HidrawDevice {
    async fn write_output_report<'a>(&'a mut self, buf: &'a [u8]) -> HidResult<()> {
        // Unlike `uhid(4)`, `hidraw(4)`'s write(2) matches Linux's hidraw exactly: the
        // buffer (report id included, per the `AsyncHidWrite` contract in traits.rs)
        // is passed straight through, no translation needed.
        let data = buf.to_vec();
        blocking(self.fd.clone(), move |fd| write(fd, &data))
            .await
            .map_err(|err| match err {
                nix::Error::EIO => HidError::Disconnected,
                err => HidError::from_backend(err)
            })
            .map(|_| ())
    }
}

impl AsyncHidFeatureHandle for HidrawDevice {
    async fn read_feature_report<'a>(&'a mut self, buf: &'a mut [u8]) -> HidResult<usize> {
        ensure!(!buf.is_empty(), HidError::message("Buffer cannot be empty"));

        let len = buf.len();
        let report_id = buf[0];
        let (n, data) = blocking(self.fd.clone(), move |fd| {
            let mut tmp = vec![0u8; len];
            tmp[0] = report_id;
            let n = unsafe { hidraw_get_feature(fd.as_raw_fd(), &mut tmp) }?;
            Ok::<_, nix::Error>((n, tmp))
        })
        .await
        .map_err(|e| HidError::message(format!("ioctl(HIDIOCGFEATURE) error: {e}")))?;
        buf[..n as usize].copy_from_slice(&data[..n as usize]);
        Ok(n as usize)
    }

    async fn write_feature_report<'a>(&'a mut self, buf: &'a [u8]) -> HidResult<()> {
        ensure!(!buf.is_empty(), HidError::message("Buffer cannot be empty"));

        let data = buf.to_vec();
        blocking(self.fd.clone(), move |fd| unsafe { hidraw_set_feature(fd.as_raw_fd(), &data) })
            .await
            .map_err(|e| HidError::message(format!("ioctl(HIDIOCSFEATURE) error: {e}")))?;

        Ok(())
    }
}

