//! One `dak` per keypad: an advisory lock file per physical device.
//!
//! Linux lets any number of processes open the same `hidraw` node, so two `dak`
//! instances (the same user twice, or two users after a fast user switch) would both
//! paint the buttons and both react to every press. FreeBSD's `hidraw(4)` refuses the
//! second open, but only with an opaque `EBUSY`. Before opening a device, `dak` therefore
//! takes an exclusive `flock(2)` on a lock file named after the device
//! ([`DeviceKey::file_name`]) in a directory every user can write to ([`lock_dir`]), and
//! records who holds it ([`Holder`]) so the loser can say which instance is in the way.
//!
//! The lock lives as long as the returned [`DeviceLock`]: `flock` locks belong to the
//! open file, so the kernel releases it when the holder exits or crashes - there is no
//! stale lock to clean up. The file itself is left in place and reused.
//!
//! Lock files are shared between users in a sticky directory, which needs some care:
//! they are opened with `O_NOFOLLOW` (a planted symlink cannot redirect the open) and
//! `O_NONBLOCK` (a planted FIFO cannot hang it), must be regular files, are opened
//! *without* `O_CREAT` first (Linux's `fs.protected_regular` refuses `O_CREAT` opens of
//! another user's file in a sticky directory even when it exists) and are made
//! world-writable (`0666`) when created, so the next user can record themselves in it.
//! Hard links are refused too (a singly linked file only, see `check_lock_file`), since
//! the holder record is written into the file.
//!
//! Because every user can write every lock file, the holder record is only ever a
//! hint for messages: before `--replace` signals the recorded pid, [`holder_is_genuine`]
//! checks that process really is a `dak` of the recorded user.

use std::ffi::CString;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::control::StopSignal;

/// Environment variable overriding [`lock_dir`] (used by the tests, and for setups
/// where the default directory is not shared between the users involved).
pub const LOCK_DIR_ENV: &str = "DAK_LOCK_DIR";

/// How long `--replace` waits for the holder to release the lock after `SIGTERM`.
pub const REPLACE_TIMEOUT: Duration = Duration::from_secs(10);

/// How often `--wait` (and `--replace`, while waiting) retries the lock.
pub const RETRY_INTERVAL: Duration = Duration::from_millis(500);

/// What to do when another `dak` holds a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Conflict {
    /// Skip the device with a message naming the holder (the default).
    #[default]
    Refuse,
    /// Wait until the holder releases it (`--wait`).
    Wait,
    /// Ask the holder to stop (`SIGTERM`) and take over (`--replace`); only for the
    /// holder's own user or root.
    Replace,
}

impl Conflict {
    /// The policy selected by the `--wait`/`--replace` flags (clap keeps them exclusive).
    pub fn from_flags(wait: bool, replace: bool) -> Self {
        match (wait, replace) {
            (_, true) => Conflict::Replace,
            (true, false) => Conflict::Wait,
            (false, false) => Conflict::Refuse,
        }
    }
}

/// The directory lock files live in: `$DAK_LOCK_DIR` when set, else `/run/lock` when it
/// exists (Linux, a tmpfs shared by every user), else `/tmp`.
pub fn lock_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os(LOCK_DIR_ENV).filter(|dir| !dir.is_empty()) {
        return PathBuf::from(dir);
    }
    let run_lock = Path::new("/run/lock");
    if run_lock.is_dir() {
        return run_lock.to_path_buf();
    }
    PathBuf::from("/tmp")
}

/// What identifies one physical keypad across processes and users.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceKey {
    /// USB vendor id.
    pub vendor_id: u16,
    /// USB product id.
    pub product_id: u16,
    /// The serial number, or - for a device without one - its OS device path, so two
    /// serial-less keypads still get distinct locks.
    pub identity: String,
}

impl DeviceKey {
    /// The key for a discovered device: its serial number when it reports a non-empty
    /// one, else `fallback` (the `Debug` text of its OS device id).
    pub fn new(vendor_id: u16, product_id: u16, serial: Option<&str>, fallback: &str) -> Self {
        let identity = match serial.map(str::trim).filter(|s| !s.is_empty()) {
            Some(serial) => serial.to_string(),
            None => format!("path-{fallback}"),
        };
        Self {
            vendor_id,
            product_id,
            identity,
        }
    }

    /// `dak-<vid>-<pid>-<identity>.lock`.
    ///
    /// An identity of at most 96 characters from `[A-Za-z0-9._-]` is used as is.
    /// Anything else (a serial with other characters, a path, an over-long serial) is
    /// shown with every other character replaced by `_`, cut to 64 characters, and
    /// followed by `~` and a 64-bit hash of the exact identity, so two different
    /// identities never share a lock file just because they look alike after
    /// sanitising (`A/B` and `A_B`). `~` cannot appear in a plain identity, so a
    /// hashed name never equals a plain one either.
    pub fn file_name(&self) -> String {
        let plain = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-');
        let identity = if self.identity.len() <= 96 && self.identity.chars().all(plain) {
            self.identity.clone()
        } else {
            let shown: String = self
                .identity
                .chars()
                .map(|c| if plain(c) { c } else { '_' })
                .take(64)
                .collect();
            format!("{shown}~{:016x}", fnv1a(self.identity.as_bytes()))
        };
        format!(
            "dak-{:04x}-{:04x}-{identity}.lock",
            self.vendor_id, self.product_id
        )
    }

    /// A short human name for messages, e.g. `0300:3002 s/n ABCD1234EF56`.
    pub fn describe(&self) -> String {
        match self.identity.strip_prefix("path-") {
            Some(path) => format!("{:04x}:{:04x} at {path}", self.vendor_id, self.product_id),
            None => format!(
                "{:04x}:{:04x} s/n {}",
                self.vendor_id, self.product_id, self.identity
            ),
        }
    }
}

/// The 64-bit FNV-1a hash of `bytes`: stable across versions and platforms, which is all
/// lock file names need (a device could copy another's serial outright anyway).
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// Who holds a lock, as recorded in its file by the holder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    /// Process id of the holding `dak`.
    pub pid: i32,
    /// Its (effective) user id.
    pub uid: u32,
    /// Its user name (the uid as text when it has none).
    pub user: String,
    /// When it took the lock (RFC 3339).
    pub since: String,
}

impl Holder {
    /// The holder record for this process, taking the lock now.
    pub fn current() -> Self {
        // SAFETY: getpid/geteuid cannot fail.
        let (pid, uid) = unsafe { (libc::getpid(), libc::geteuid()) };
        Self {
            pid,
            uid,
            user: user_name(uid),
            since: crate::log::timestamp(),
        }
    }

    /// The `key=value` lines written into the lock file.
    pub fn to_text(&self) -> String {
        format!(
            "pid={}\nuid={}\nuser={}\nsince={}\n",
            self.pid, self.uid, self.user, self.since
        )
    }

    /// Parses [`Holder::to_text`]; `None` when the text is incomplete (e.g. read while
    /// the holder was still writing it) or names a pid that cannot be a `dak` (`0`, `1`
    /// or negative: `kill` would signal a process group, init, or every process).
    pub fn parse(text: &str) -> Option<Self> {
        let mut pid = None;
        let mut uid = None;
        let mut user = None;
        let mut since = None;
        for line in text.lines() {
            match line.split_once('=') {
                Some(("pid", value)) => pid = value.parse().ok(),
                Some(("uid", value)) => uid = value.parse().ok(),
                Some(("user", value)) => user = Some(value.to_string()),
                Some(("since", value)) => since = Some(value.to_string()),
                _ => {}
            }
        }
        let pid: i32 = pid?;
        if pid <= 1 {
            return None;
        }
        Some(Self {
            pid,
            uid: uid?,
            user: user?,
            since: since?,
        })
    }

    /// `dak (user alice, pid 1234, since ...)`.
    pub fn describe(&self) -> String {
        format!(
            "dak (user {}, pid {}, since {})",
            self.user, self.pid, self.since
        )
    }
}

/// The user name of `uid`, or the number itself when it has no passwd entry.
fn user_name(uid: u32) -> String {
    let mut buffer = vec![0 as libc::c_char; 4096];
    // SAFETY: getpwuid_r fills `entry` with pointers into `buffer`, which outlives them.
    unsafe {
        let mut entry: libc::passwd = std::mem::zeroed();
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        let status = libc::getpwuid_r(
            uid,
            &mut entry,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut result,
        );
        if status == 0 && !result.is_null() && !entry.pw_name.is_null() {
            return std::ffi::CStr::from_ptr(entry.pw_name)
                .to_string_lossy()
                .into_owned();
        }
    }
    uid.to_string()
}

/// A held device lock; dropping it releases the lock.
#[derive(Debug)]
pub struct DeviceLock {
    /// The open, `flock`ed lock file. Closing it releases the lock.
    _file: File,
    /// Where the lock file is.
    path: PathBuf,
}

impl DeviceLock {
    /// The lock file's path.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Why a lock could not be taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockError {
    /// Another process holds it; its record, when it could be read.
    Busy(Option<Holder>),
    /// `--replace` may not stop this holder: it belongs to another user and we are not
    /// root.
    NotPermitted(Holder),
    /// `--replace` refused to signal the recorded holder: that process is not a `dak`
    /// running as the recorded user (the record is forged or stale - lock files can be
    /// written by every user).
    NotVerified(Holder),
    /// `--replace` sent `SIGTERM` but the holder did not let go in time.
    Timeout(Option<Holder>),
    /// The wait was ended by a stop signal.
    Cancelled,
    /// The lock file could not be opened or locked.
    Io(String),
}

impl LockError {
    /// A message for the log, naming the device and the holder.
    pub fn describe(&self, key: &DeviceKey) -> String {
        let device = key.describe();
        let holder = |holder: &Option<Holder>| match holder {
            Some(holder) => holder.describe(),
            None => "another dak".to_string(),
        };
        match self {
            LockError::Busy(h) => format!(
                "device {device} is in use by {}; skipping it (use --wait to wait for it or \
                 --replace to take it over)",
                holder(h)
            ),
            LockError::NotPermitted(h) => format!(
                "device {device} is in use by {}, which belongs to another user; --replace \
                 only stops your own instances (or any, as root)",
                h.describe()
            ),
            LockError::NotVerified(h) => format!(
                "device {device} is locked, but its holder record names {}, which is not a \
                 dak running as that user; not signalling it (the lock file can be written by \
                 any user)",
                h.describe()
            ),
            LockError::Timeout(h) => format!(
                "device {device}: {} did not release it within {}s of SIGTERM",
                holder(h),
                REPLACE_TIMEOUT.as_secs()
            ),
            LockError::Cancelled => format!("device {device}: stopped while waiting for its lock"),
            LockError::Io(error) => format!("device {device}: cannot lock it: {error}"),
        }
    }
}

/// The last OS error as text.
fn os_error() -> std::io::Error {
    std::io::Error::last_os_error()
}

/// Opens `path` with `flags` (plus `O_NOFOLLOW | O_NONBLOCK | O_CLOEXEC`) and `mode`.
fn open_raw(path: &Path, flags: libc::c_int, mode: libc::mode_t) -> std::io::Result<File> {
    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::other("lock path contains a NUL byte"))?;
    // SAFETY: a valid C path; the returned fd is owned by the File from here on.
    let fd = unsafe {
        libc::open(
            c_path.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            mode as libc::c_uint,
        )
    };
    if fd < 0 {
        return Err(os_error());
    }
    // SAFETY: fd is a fresh, owned descriptor.
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// Opens (creating when missing) the lock file at `path`, returning the file and
/// whether it is writable. See the module docs for why it is opened the way it is.
fn open_lock_file(path: &Path) -> std::io::Result<(File, bool)> {
    let file = loop {
        match open_raw(path, libc::O_RDWR, 0) {
            Ok(file) => break (file, true),
            Err(error) if error.raw_os_error() == Some(libc::EACCES) => {
                // Another user's file we may not write: still lockable read-only.
                break (open_raw(path, libc::O_RDONLY, 0)?, false);
            }
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {
                match open_raw(path, libc::O_RDWR | libc::O_CREAT | libc::O_EXCL, 0o666) {
                    Ok(file) => {
                        // The umask trimmed the mode; the next user must be able to
                        // write their record too.
                        // SAFETY: fchmod on an fd we own.
                        unsafe { libc::fchmod(file.as_raw_fd(), 0o666) };
                        break (file, true);
                    }
                    // Someone created it in between: open theirs.
                    Err(error) if error.raw_os_error() == Some(libc::EEXIST) => continue,
                    Err(error) => return Err(error),
                }
            }
            Err(error) => return Err(error),
        }
    };
    check_lock_file(&file.0.metadata()?)?;
    Ok(file)
}

/// Refuses a lock file that is not a plain, singly linked regular file. A hard link
/// someone planted under the lock file's name would otherwise make `dak` truncate and
/// overwrite the file it points to (`O_NOFOLLOW` only stops symlinks).
fn check_lock_file(metadata: &std::fs::Metadata) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    if !metadata.file_type().is_file() {
        return Err(std::io::Error::other("not a regular file"));
    }
    if metadata.nlink() != 1 {
        return Err(std::io::Error::other(format!(
            "has {} hard links; refusing to use it (someone may have linked it to another \
             file)",
            metadata.nlink()
        )));
    }
    Ok(())
}

/// The effective uid and command name of the running process `pid`, or `None` when
/// there is no such process (or it cannot be inspected).
#[cfg(target_os = "linux")]
pub fn process_identity(pid: i32) -> Option<(u32, String)> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    // "Uid:\treal\teffective\tsaved\tfs"
    let uid = status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()?;
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    Some((uid, comm.trim_end_matches('\n').to_string()))
}

/// The effective uid and command name of the running process `pid`, or `None` when
/// there is no such process (or it cannot be inspected).
#[cfg(target_os = "freebsd")]
pub fn process_identity(pid: i32) -> Option<(u32, String)> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_PID, pid];
    // SAFETY: kinfo_proc is plain old data; sysctl fills at most `size` bytes of it.
    let mut info: libc::kinfo_proc = unsafe { std::mem::zeroed() };
    let mut size = std::mem::size_of::<libc::kinfo_proc>();
    // SAFETY: mib, info and size are valid for the duration of the call.
    let status = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            &mut info as *mut libc::kinfo_proc as *mut libc::c_void,
            &mut size,
            std::ptr::null(),
            0,
        )
    };
    if status != 0 || size < std::mem::size_of::<libc::kinfo_proc>() || info.ki_pid != pid {
        return None;
    }
    // SAFETY: ki_comm is a NUL-terminated array inside `info`.
    let comm = unsafe { std::ffi::CStr::from_ptr(info.ki_comm.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    Some((info.ki_uid, comm))
}

/// Whether the process a holder record names really is a `dak` holder that `--replace`
/// may signal: it exists, runs as the recorded uid, and runs the same program as this
/// process (compared by command name, so the test binaries, which re-run themselves as
/// the other holder, pass too). Lock files are writable by every user, so the record
/// alone proves nothing.
pub fn holder_is_genuine(holder: &Holder) -> bool {
    // SAFETY: getpid cannot fail.
    let own_pid = unsafe { libc::getpid() };
    match (process_identity(holder.pid), process_identity(own_pid)) {
        (Some((uid, name)), Some((_, own_name))) => uid == holder.uid && name == own_name,
        _ => false,
    }
}

/// Reads the holder record from an open lock file.
fn read_holder(file: &mut File) -> Option<Holder> {
    let mut text = String::new();
    file.seek(SeekFrom::Start(0)).ok()?;
    file.take(4096).read_to_string(&mut text).ok()?;
    Holder::parse(&text)
}

/// Reads the holder record of the lock file at `path`, if it has one.
pub fn holder_of(path: &Path) -> Option<Holder> {
    let mut file = open_raw(path, libc::O_RDONLY, 0).ok()?;
    read_holder(&mut file)
}

/// Tries once to take the lock for `key` in `dir`, without waiting.
pub fn try_lock(dir: &Path, key: &DeviceKey) -> Result<DeviceLock, LockError> {
    let path = dir.join(key.file_name());
    let (mut file, writable) = open_lock_file(&path)
        .map_err(|error| LockError::Io(format!("{}: {error}", path.display())))?;
    // SAFETY: flock on an fd we own.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = os_error();
        if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
            return Err(LockError::Busy(read_holder(&mut file)));
        }
        return Err(LockError::Io(format!("{}: {error}", path.display())));
    }
    if writable {
        let record = Holder::current().to_text();
        let _ = file.set_len(0);
        let _ = file.seek(SeekFrom::Start(0));
        let _ = file.write_all(record.as_bytes());
        let _ = file.flush();
    }
    Ok(DeviceLock { _file: file, path })
}

/// Whether this process may send `SIGTERM` to a `dak` run by `holder_uid`: its own
/// user's, or anybody's as root.
pub fn may_replace(own_uid: u32, holder_uid: u32) -> bool {
    own_uid == 0 || own_uid == holder_uid
}

/// Takes the lock for `key` in `dir`, handling a held lock per `conflict`:
/// [`Conflict::Refuse`] fails at once with [`LockError::Busy`]; [`Conflict::Wait`]
/// retries every [`RETRY_INTERVAL`] until it is free or `stop` fires;
/// [`Conflict::Replace`] sends the holder `SIGTERM` (when [`may_replace`]) and waits
/// up to [`REPLACE_TIMEOUT`] for it to let go. `on_wait` is called once when a wait
/// begins, with the holder, so the caller can say what it is waiting for.
pub async fn acquire(
    dir: &Path,
    key: &DeviceKey,
    conflict: Conflict,
    stop: &StopSignal,
    on_wait: impl FnOnce(Option<&Holder>),
) -> Result<DeviceLock, LockError> {
    acquire_with_timeout(dir, key, conflict, REPLACE_TIMEOUT, stop, on_wait).await
}

/// [`acquire`] with `--replace`'s wait for the holder to let go bounded by
/// `replace_timeout` instead of [`REPLACE_TIMEOUT`] (so tests need not wait 10 s).
pub async fn acquire_with_timeout(
    dir: &Path,
    key: &DeviceKey,
    conflict: Conflict,
    replace_timeout: Duration,
    stop: &StopSignal,
    on_wait: impl FnOnce(Option<&Holder>),
) -> Result<DeviceLock, LockError> {
    let holder = match try_lock(dir, key) {
        Ok(lock) => return Ok(lock),
        Err(LockError::Busy(holder)) => holder,
        Err(other) => return Err(other),
    };
    let deadline = match conflict {
        Conflict::Refuse => return Err(LockError::Busy(holder)),
        Conflict::Wait => None,
        Conflict::Replace => {
            let Some(target) = holder.clone() else {
                // Nothing to signal yet (the holder is still writing its record):
                // treat as busy, the caller may retry.
                return Err(LockError::Busy(None));
            };
            // SAFETY: geteuid cannot fail.
            let own_uid = unsafe { libc::geteuid() };
            if !may_replace(own_uid, target.uid) {
                return Err(LockError::NotPermitted(target));
            }
            if !holder_is_genuine(&target) {
                return Err(LockError::NotVerified(target));
            }
            // SAFETY: plain kill(2) of a verified holder (a dak of the recorded user).
            if unsafe { libc::kill(target.pid, libc::SIGTERM) } != 0 {
                let error = os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    return Err(LockError::Io(format!(
                        "cannot signal pid {}: {error}",
                        target.pid
                    )));
                }
            }
            Some(tokio::time::Instant::now() + replace_timeout)
        }
    };
    on_wait(holder.as_ref());
    let mut last = holder;
    loop {
        tokio::select! {
            _ = tokio::time::sleep(RETRY_INTERVAL) => {}
            _ = stop.stopped() => return Err(LockError::Cancelled),
        }
        match try_lock(dir, key) {
            Ok(lock) => return Ok(lock),
            Err(LockError::Busy(holder)) => {
                if holder.is_some() {
                    last = holder;
                }
            }
            Err(other) => return Err(other),
        }
        if deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline) {
            return Err(LockError::Timeout(last));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lock file names keep only safe characters and include vid/pid in hex.
    #[test]
    fn file_names_are_sanitized() {
        let key = DeviceKey::new(0x0300, 0x3002, Some("ABCD1234EF56"), "x");
        assert_eq!(key.file_name(), "dak-0300-3002-ABCD1234EF56.lock");
        let key = DeviceKey::new(0x0300, 0x3002, Some("../a b/c"), "x");
        let name = key.file_name();
        assert!(name.starts_with("dak-0300-3002-.._a_b_c~"), "{name}");
        assert!(!name.contains('/'));
        let long = "A".repeat(300);
        let key = DeviceKey::new(1, 2, Some(&long), "x");
        assert_eq!(
            key.file_name().len(),
            "dak-0001-0002-".len() + 64 + 1 + 16 + ".lock".len()
        );
        let exact = "B".repeat(96);
        let key = DeviceKey::new(1, 2, Some(&exact), "x");
        assert_eq!(key.file_name(), format!("dak-0001-0002-{exact}.lock"));
    }

    /// Identities that look alike once sanitised or cut still get distinct lock files,
    /// and the names are stable (other dak versions must compute the same ones).
    #[test]
    fn file_names_do_not_collide() {
        let name = |serial: &str| DeviceKey::new(1, 2, Some(serial), "x").file_name();
        assert_ne!(name("A/B"), name("A_B"));
        assert_ne!(name("A/B"), name("A B"));
        assert_ne!(
            name(&format!("{}1", "C".repeat(100))),
            name(&format!("{}2", "C".repeat(100)))
        );
        assert_eq!(name("A/B"), "dak-0001-0002-A_B~fa76c319a0b010e1.lock");
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
    }

    /// A device without a (non-blank) serial is keyed by its OS path instead.
    #[test]
    fn missing_serial_falls_back_to_the_path() {
        for serial in [None, Some(""), Some("  ")] {
            let key = DeviceKey::new(0x0300, 0x3002, serial, "/dev/hidraw3");
            assert_eq!(key.identity, "path-/dev/hidraw3");
            assert!(key
                .file_name()
                .starts_with("dak-0300-3002-path-_dev_hidraw3~"));
            assert_eq!(key.describe(), "0300:3002 at /dev/hidraw3");
        }
        let key = DeviceKey::new(0x0300, 0x3002, Some("S1"), "/dev/hidraw3");
        assert_eq!(key.describe(), "0300:3002 s/n S1");
    }

    /// The holder record round-trips; an incomplete one is not a record.
    #[test]
    fn holder_records_round_trip() {
        let holder = Holder {
            pid: 42,
            uid: 1000,
            user: "alice".to_string(),
            since: "2026-09-27T10:00:00.000+10:00".to_string(),
        };
        assert_eq!(Holder::parse(&holder.to_text()), Some(holder.clone()));
        assert_eq!(Holder::parse("pid=42\nuid=1000\n"), None);
        assert_eq!(Holder::parse(""), None);
        assert_eq!(
            holder.describe(),
            "dak (user alice, pid 42, since 2026-09-27T10:00:00.000+10:00)"
        );
    }

    /// This process's record names itself.
    #[test]
    fn current_holder_is_this_process() {
        let holder = Holder::current();
        assert_eq!(holder.pid, std::process::id() as i32);
        assert_eq!(holder.uid, unsafe { libc::geteuid() });
        assert!(!holder.user.is_empty());
    }

    /// uid 0 has a passwd entry named root everywhere dak runs; an unknown uid is shown
    /// as its number.
    #[test]
    fn user_names_resolve() {
        assert_eq!(user_name(0), "root");
        assert_eq!(user_name(4_000_000_000), "4000000000");
    }

    /// Only your own instances may be replaced, or anyone's as root.
    #[test]
    fn replace_permission() {
        assert!(may_replace(1000, 1000));
        assert!(may_replace(0, 1000));
        assert!(!may_replace(1000, 1001));
    }

    /// The flags select the policy, `--replace` winning (clap forbids both anyway).
    #[test]
    fn conflict_from_flags() {
        assert_eq!(Conflict::from_flags(false, false), Conflict::Refuse);
        assert_eq!(Conflict::from_flags(true, false), Conflict::Wait);
        assert_eq!(Conflict::from_flags(false, true), Conflict::Replace);
        assert_eq!(Conflict::from_flags(true, true), Conflict::Replace);
    }

    /// Messages name the device and the holder, and hint at the flags.
    #[test]
    fn error_messages() {
        let key = DeviceKey::new(0x0300, 0x3002, Some("S1"), "");
        let holder = Holder {
            pid: 7,
            uid: 1,
            user: "bob".into(),
            since: "t".into(),
        };
        let busy = LockError::Busy(Some(holder.clone())).describe(&key);
        assert!(busy.contains("0300:3002 s/n S1") && busy.contains("user bob, pid 7"));
        assert!(busy.contains("--wait") && busy.contains("--replace"));
        assert!(LockError::Busy(None).describe(&key).contains("another dak"));
        assert!(LockError::NotPermitted(holder)
            .describe(&key)
            .contains("another user"));
        assert!(LockError::Timeout(None).describe(&key).contains("10s"));
        assert_eq!(
            LockError::Io("boom".into()).describe(&key),
            "device 0300:3002 s/n S1: cannot lock it: boom"
        );
    }

    /// Unknown lines in a holder record (from a newer dak, say) are ignored.
    #[test]
    fn holder_records_ignore_unknown_lines() {
        let text = "pid=2\nfuture=yes\nuid=3\nno separator\nuser=u\nsince=s\n";
        let holder = Holder::parse(text).unwrap();
        assert_eq!((holder.pid, holder.uid), (2, 3));
    }

    /// A record naming pid 1 or below is not a record: signalling it would hit init, the
    /// own process group or every process.
    #[test]
    fn holder_records_reject_impossible_pids() {
        for pid in ["1", "0", "-1", "-1234"] {
            let text = format!("pid={pid}\nuid=0\nuser=u\nsince=s\n");
            assert_eq!(Holder::parse(&text), None, "pid {pid}");
        }
    }

    /// The not-verified message names the holder and says why it is not signalled.
    #[test]
    fn not_verified_message() {
        let key = DeviceKey::new(0x0300, 0x3002, Some("S1"), "");
        let holder = Holder {
            pid: 7,
            uid: 1,
            user: "bob".into(),
            since: "t".into(),
        };
        let text = LockError::NotVerified(holder).describe(&key);
        assert!(
            text.contains("pid 7") && text.contains("not signalling"),
            "{text}"
        );
    }

    /// Only a singly linked regular file is accepted as a lock file.
    #[test]
    fn lock_file_check_refuses_links_and_non_files() {
        let dir = scratch_dir("check");
        let file = dir.join("f");
        std::fs::write(&file, "").unwrap();
        assert!(check_lock_file(&std::fs::metadata(&file).unwrap()).is_ok());
        std::fs::hard_link(&file, dir.join("g")).unwrap();
        let error = check_lock_file(&std::fs::metadata(&file).unwrap()).unwrap_err();
        assert!(error.to_string().contains("2 hard links"), "{error}");
        let error = check_lock_file(&std::fs::metadata(&dir).unwrap()).unwrap_err();
        assert!(error.to_string().contains("not a regular file"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// This process can be inspected; a pid that does not exist cannot.
    #[test]
    fn process_identity_reads_uid_and_name() {
        let (uid, name) = process_identity(std::process::id() as i32).unwrap();
        assert_eq!(uid, unsafe { libc::geteuid() });
        assert!(!name.is_empty());
        assert_eq!(process_identity(i32::MAX), None);
    }

    /// A fresh scratch directory for the acquire tests.
    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dak_lockunit_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// An I/O error taking the lock (a missing lock directory) is passed through as is,
    /// whatever the conflict policy.
    #[tokio::test]
    async fn acquire_passes_io_errors_through() {
        let key = DeviceKey::new(1, 2, Some("IO"), "");
        let stop = crate::control::StopSource::new();
        let missing = std::env::temp_dir().join("dak_lockunit_does_not_exist/deeper");
        for conflict in [Conflict::Refuse, Conflict::Wait, Conflict::Replace] {
            let result = acquire_with_timeout(
                &missing,
                &key,
                conflict,
                Duration::ZERO,
                &stop.signal(),
                |_| panic!("no wait on an I/O error"),
            )
            .await;
            assert!(matches!(result, Err(LockError::Io(_))), "{conflict:?}");
        }
    }

    /// `--replace` against a holder that has not written its record yet has nobody to
    /// signal, so it reports the device busy (the caller may retry) instead of waiting.
    #[tokio::test]
    async fn replace_without_a_holder_record_is_busy() {
        let dir = scratch_dir("norecord");
        let key = DeviceKey::new(1, 2, Some("NOREC"), "");
        let held = try_lock(&dir, &key).unwrap();
        std::fs::write(held.path(), "").unwrap();
        let stop = crate::control::StopSource::new();
        let result = acquire_with_timeout(
            &dir,
            &key,
            Conflict::Replace,
            Duration::ZERO,
            &stop.signal(),
            |_| panic!("no wait without a holder"),
        )
        .await;
        assert!(matches!(result, Err(LockError::Busy(None))), "{result:?}");
        drop(held);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
