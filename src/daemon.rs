//! Running as a background service: detaching from the terminal (`--detach`), the pid
//! file (`--pid-file`) and systemd's readiness protocol (`sd_notify`).
//!
//! [`detach`] is the classic double fork, with one twist: the original process does not
//! exit straight away but waits on a pipe until the daemon reports how startup went
//! ([`Readiness`]). So `dak --detach` still fails visibly, with the daemon's exit status
//! and last error, when e.g. no keypad is attached - instead of exiting 0 and leaving the
//! user to find out from the log. It must run before any threads exist (before the
//! tokio runtime is built), since only the forking thread survives a `fork`.
//!
//! [`notify`] sends `sd_notify(3)` messages (`READY=1`, `STATUS=...`, `STOPPING=1`) to
//! the socket systemd names in `NOTIFY_SOCKET`, which is what a `Type=notify` unit waits
//! for. Without the variable it does nothing, so it is called unconditionally.

use std::ffi::CString;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The daemon side of the startup pipe: tells the waiting original process how startup
/// went, exactly once.
#[derive(Debug)]
pub struct Readiness {
    /// The pipe's write end; `None` once a report was sent.
    pipe: Mutex<Option<File>>,
}

/// What the daemon reports through the startup pipe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupReport {
    /// Startup went fine; the text is shown to the user (e.g. what was connected).
    Ready(String),
    /// The daemon is exiting with this status; the text is its last error.
    Failed(u8, String),
}

impl StartupReport {
    /// The wire form: `R<text>` or `F<status> <text>`.
    pub fn encode(&self) -> Vec<u8> {
        match self {
            StartupReport::Ready(text) => format!("R{text}").into_bytes(),
            StartupReport::Failed(status, text) => format!("F{status} {text}").into_bytes(),
        }
    }

    /// Parses [`StartupReport::encode`]'s output; anything else (including nothing at
    /// all: the daemon died without reporting) is `None`.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let text = String::from_utf8_lossy(bytes);
        if let Some(rest) = text.strip_prefix('R') {
            return Some(StartupReport::Ready(rest.to_string()));
        }
        let rest = text.strip_prefix('F')?;
        let (status, message) = rest.split_once(' ').unwrap_or((rest, ""));
        Some(StartupReport::Failed(
            status.parse().ok()?,
            message.to_string(),
        ))
    }
}

impl Readiness {
    /// Wraps the write end of a startup pipe.
    pub fn from_file(file: File) -> Self {
        Self {
            pipe: Mutex::new(Some(file)),
        }
    }

    /// Sends `report` and closes the pipe, releasing the waiting process. Later calls
    /// do nothing: only the first report counts.
    pub fn report(&self, report: StartupReport) {
        if let Some(mut pipe) = self.pipe.lock().expect("readiness lock poisoned").take() {
            let _ = pipe.write_all(&report.encode());
        }
    }

    /// Whether a report was already sent.
    pub fn reported(&self) -> bool {
        self.pipe.lock().expect("readiness lock poisoned").is_none()
    }
}

/// Where [`detach`] returns.
#[derive(Debug)]
pub enum Detached {
    /// In the daemon: carry on, and report startup through this.
    Daemon(Readiness),
    /// In the original process, once the daemon reported (or died): exit with this
    /// status after showing the message.
    Parent(u8, String),
}

/// The last OS error, with `what` failed.
fn os_error(what: &str) -> String {
    format!("{what} failed: {}", std::io::Error::last_os_error())
}

/// Forks into the background: the original process waits for the daemon's startup
/// report; an intermediate child starts a new session and forks again (so the daemon is
/// not a session leader and can never reacquire a controlling terminal) and exits; the
/// daemon changes to `/`, points stdin, stdout and stderr at `/dev/null`, and writes
/// `pid_file` when given.
///
/// # Safety
///
/// Must be called while the process is single-threaded (before the async runtime is
/// built): `fork` only duplicates the calling thread.
pub unsafe fn detach(pid_file: Option<&Path>) -> Result<Detached, String> {
    let mut fds = [0 as libc::c_int; 2];
    if libc::pipe(fds.as_mut_ptr()) != 0 {
        return Err(os_error("pipe"));
    }
    let (read_end, write_end) = (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1]));
    // The daemon must not leak the pipe into the programs it runs.
    libc::fcntl(write_end.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);

    match libc::fork() {
        -1 => return Err(os_error("fork")),
        0 => {}
        child => {
            drop(write_end);
            let mut reader = File::from(read_end);
            let mut bytes = Vec::new();
            let _ = reader.read_to_end(&mut bytes);
            let mut status = 0;
            // Reap the intermediate child (it exits right after the second fork).
            libc::waitpid(child, &mut status, 0);
            return Ok(match StartupReport::decode(&bytes) {
                Some(StartupReport::Ready(text)) => Detached::Parent(crate::exit::SUCCESS, text),
                Some(StartupReport::Failed(status, text)) => Detached::Parent(status, text),
                None => Detached::Parent(
                    crate::exit::FAILURE,
                    "the daemon ended during startup without reporting why".to_string(),
                ),
            });
        }
    }

    // Intermediate child.
    drop(read_end);
    if libc::setsid() < 0 {
        report_and_exit(write_end, &os_error("setsid"));
    }
    match libc::fork() {
        -1 => report_and_exit(write_end, &os_error("fork")),
        0 => {}
        _ => libc::_exit(0),
    }

    // The daemon.
    let readiness = Readiness::from_file(File::from(write_end));
    if libc::chdir(c"/".as_ptr()) != 0 {
        let error = os_error("chdir /");
        readiness.report(StartupReport::Failed(crate::exit::FAILURE, error.clone()));
        return Err(error);
    }
    if let Err(error) = redirect_stdio_to_null() {
        readiness.report(StartupReport::Failed(crate::exit::FAILURE, error.clone()));
        return Err(error);
    }
    if let Some(path) = pid_file {
        if let Err(error) = write_pid_file(path, std::process::id()) {
            readiness.report(StartupReport::Failed(crate::exit::FAILURE, error.clone()));
            return Err(error);
        }
    }
    Ok(Detached::Daemon(readiness))
}

/// Reports `error` through the startup pipe and ends the (intermediate) process.
fn report_and_exit(write_end: OwnedFd, error: &str) -> ! {
    Readiness::from_file(File::from(write_end)).report(StartupReport::Failed(
        crate::exit::FAILURE,
        error.to_string(),
    ));
    // SAFETY: _exit skips atexit handlers and buffered I/O the parent still owns.
    unsafe { libc::_exit(crate::exit::FAILURE as i32) }
}

/// Points fds 0, 1 and 2 at `/dev/null`.
fn redirect_stdio_to_null() -> Result<(), String> {
    let null = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")
        .map_err(|error| format!("cannot open /dev/null: {error}"))?;
    for fd in 0..=2 {
        // SAFETY: dup2 onto the standard descriptors.
        if unsafe { libc::dup2(null.as_raw_fd(), fd) } < 0 {
            return Err(os_error("dup2"));
        }
    }
    Ok(())
}

/// Writes `pid` (and a newline) to `path`, replacing an old file but refusing to follow
/// a symlink there.
pub fn write_pid_file(path: &Path, pid: u32) -> Result<(), String> {
    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| format!("pid file {} contains a NUL byte", path.display()))?;
    // SAFETY: a valid C path; the fd is owned by the File from here on.
    let fd = unsafe {
        libc::open(
            c_path.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o644 as libc::c_uint,
        )
    };
    if fd < 0 {
        return Err(format!(
            "cannot write pid file {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: fd is a fresh, owned descriptor.
    let mut file = unsafe { File::from_raw_fd(fd) };
    writeln!(file, "{pid}")
        .map_err(|error| format!("cannot write pid file {}: {error}", path.display()))
}

/// Removes the pid file at exit, but only if it still names this process (a newer
/// instance may have replaced it).
pub fn remove_pid_file(path: &Path) {
    let ours = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| text.trim().parse::<u32>().ok())
        == Some(std::process::id());
    if ours {
        let _ = std::fs::remove_file(path);
    }
}

/// The socket named by `NOTIFY_SOCKET`, if set.
pub fn notify_socket() -> Option<PathBuf> {
    std::env::var_os("NOTIFY_SOCKET")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// Sends an `sd_notify(3)` message (newline-separated `KEY=value` assignments) to
/// systemd, when it asked for them (`NOTIFY_SOCKET`). Failures are ignored: a service
/// must work the same with or without a listener.
pub fn notify(message: &str) {
    if let Some(socket) = notify_socket() {
        let _ = notify_to(&socket, message);
    }
}

/// Sends `message` as one datagram to the notification socket `socket`: a filesystem
/// path, or on Linux an abstract socket written with a leading `@`.
pub fn notify_to(socket: &Path, message: &str) -> std::io::Result<()> {
    let sender = std::os::unix::net::UnixDatagram::unbound()?;
    let bytes = socket.as_os_str().as_bytes();
    if let Some(name) = bytes.strip_prefix(b"@") {
        #[cfg(target_os = "linux")]
        {
            use std::os::linux::net::SocketAddrExt;
            let address = std::os::unix::net::SocketAddr::from_abstract_name(name)?;
            sender.send_to_addr(message.as_bytes(), &address)?;
            return Ok(());
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = name;
            return Err(std::io::Error::other("abstract sockets need Linux"));
        }
    }
    sender.send_to(message.as_bytes(), socket)?;
    Ok(())
}

/// A `STATUS=` value on one line (sd_notify assignments end at a newline).
pub fn status_line(text: &str) -> String {
    format!("STATUS={}", text.replace('\n', " "))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reports survive the wire format, including spaces and an empty message.
    #[test]
    fn startup_reports_round_trip() {
        for report in [
            StartupReport::Ready("1 device connected".into()),
            StartupReport::Ready(String::new()),
            StartupReport::Failed(4, "no device defined in config was found".into()),
            StartupReport::Failed(3, String::new()),
        ] {
            assert_eq!(StartupReport::decode(&report.encode()), Some(report));
        }
        assert_eq!(StartupReport::decode(b""), None);
        assert_eq!(StartupReport::decode(b"Fx oops"), None);
        assert_eq!(StartupReport::decode(b"?"), None);
    }

    /// Only the first report goes out; the pipe closes with it.
    #[test]
    fn readiness_reports_once() {
        let mut fds = [0; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let mut reader = unsafe { File::from_raw_fd(fds[0]) };
        let readiness = Readiness::from_file(unsafe { File::from_raw_fd(fds[1]) });
        assert!(!readiness.reported());
        readiness.report(StartupReport::Ready("up".into()));
        readiness.report(StartupReport::Failed(1, "ignored".into()));
        assert!(readiness.reported());
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"Rup");
    }

    /// A scratch path unique to one test.
    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("dak_daemon_{name}_{}", std::process::id()))
    }

    /// The pid file holds the pid; removal only deletes a file naming this process.
    #[test]
    fn pid_file_write_and_remove() {
        let path = scratch("pid");
        write_pid_file(&path, std::process::id()).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("{}\n", std::process::id())
        );
        remove_pid_file(&path);
        assert!(!path.exists());

        write_pid_file(&path, 1).unwrap();
        remove_pid_file(&path);
        assert!(path.exists(), "someone else's pid file is left alone");
        let _ = std::fs::remove_file(&path);
    }

    /// The pid file never follows a symlink.
    #[test]
    fn pid_file_refuses_symlinks() {
        let target = scratch("pid_target");
        let link = scratch("pid_link");
        std::fs::write(&target, "keep").unwrap();
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(write_pid_file(&link, 42).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep");
        let _ = std::fs::remove_file(&link);
        let _ = std::fs::remove_file(&target);
    }

    /// Notifications arrive as one datagram each on a path socket.
    #[test]
    fn notify_to_a_path_socket() {
        let path = scratch("notify.sock");
        let _ = std::fs::remove_file(&path);
        let listener = std::os::unix::net::UnixDatagram::bind(&path).unwrap();
        notify_to(&path, "READY=1\nSTATUS=running").unwrap();
        let mut buffer = [0u8; 256];
        let read = listener.recv(&mut buffer).unwrap();
        assert_eq!(&buffer[..read], b"READY=1\nSTATUS=running");
        let _ = std::fs::remove_file(&path);
    }

    /// Linux abstract sockets (`@name`) work too.
    #[cfg(target_os = "linux")]
    #[test]
    fn notify_to_an_abstract_socket() {
        use std::os::linux::net::SocketAddrExt;
        let name = format!("dak-test-notify-{}", std::process::id());
        let address = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
        let listener = std::os::unix::net::UnixDatagram::bind_addr(&address).unwrap();
        notify_to(Path::new(&format!("@{name}")), "STOPPING=1").unwrap();
        let mut buffer = [0u8; 64];
        let read = listener.recv(&mut buffer).unwrap();
        assert_eq!(&buffer[..read], b"STOPPING=1");
    }

    /// A missing listener is an error for `notify_to` (and silently ignored by `notify`).
    #[test]
    fn notify_to_a_missing_socket_fails() {
        assert!(notify_to(Path::new("/nonexistent/notify.sock"), "READY=1").is_err());
    }

    /// Status text is kept on one line.
    #[test]
    fn status_lines_are_single_line() {
        assert_eq!(status_line("a\nb"), "STATUS=a b");
    }
}
