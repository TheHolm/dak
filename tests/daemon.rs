//! `--detach`, `--pid-file` and the systemd notifications, running the real binary.
//!
//! A daemon that starts successfully needs a configured keypad; these tests use one
//! that is merely enumerable and hold its lock themselves, so the daemon starts up
//! "waiting for another dak" without ever opening the device (skipped when no keypad is
//! attached at all). The failure paths need no hardware.

mod common;

use std::os::unix::net::UnixDatagram;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use dak::exit;

/// A spawned `dak` that is killed when dropped if it is still running, so a failing
/// test never leaves its daemon behind.
struct Running(std::process::Child);

impl Drop for Running {
    /// Kills and reaps the child unless it already exited.
    fn drop(&mut self) {
        if let Ok(None) = self.0.try_wait() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

impl std::ops::Deref for Running {
    type Target = std::process::Child;

    /// The child process.
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for Running {
    /// The child process.
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

/// Whether process `pid` still exists.
fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Waits up to five seconds for `pid` to go away.
fn wait_gone(pid: i32) -> bool {
    for _ in 0..500 {
        if !alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

/// A config error is reported in the terminal before forking (status 3).
#[test]
fn detach_reports_config_errors_before_forking() {
    let output = Command::new(env!("CARGO_BIN_EXE_dak"))
        .args(["-c", "/nonexistent/dak.json", "--detach"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(exit::CONFIG as i32));
    assert!(String::from_utf8_lossy(&output.stderr).contains("Couldn't open"));
}

/// A daemon that fails after the fork (here: its pid file cannot be written) hands its
/// status and error back to the terminal it was started from.
#[test]
fn detach_reports_startup_failure_with_its_status() {
    let config = common::write_temp_config(r#"{"scenes": {"on_start": {}}, "devices": {}}"#);
    let output = Command::new(env!("CARGO_BIN_EXE_dak"))
        .args(["-c", config.to_str().unwrap(), "--detach"])
        .args(["--pid-file", "/nonexistent/dir/dak.pid"])
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&config);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(exit::FAILURE as i32), "{stderr}");
    assert!(
        stderr.contains("failed to start in the background (exit status 1): cannot write pid file"),
        "{stderr}"
    );
}

/// A detached daemon with no configured device attached keeps running (so a plug-in
/// hook or SIGUSR1 can bring it to work later); the terminal gets status 0 and a
/// warning saying so.
#[test]
fn detach_without_devices_keeps_running_and_warns() {
    let dir = common::temp_dir();
    let pid_file = dir.join("dak.pid");
    let config = common::write_temp_config(r#"{"scenes": {"on_start": {}}, "devices": {}}"#);
    let output = Command::new(env!("CARGO_BIN_EXE_dak"))
        .args([
            "-c",
            config.to_str().unwrap(),
            "--detach",
            "--log-level",
            "warning",
        ])
        .arg("--pid-file")
        .arg(&pid_file)
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&config);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    assert!(
        stderr.contains("running in the background, but no configured device"),
        "{stderr}"
    );
    let pid: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(alive(pid));
    unsafe { libc::kill(pid, libc::SIGTERM) };
    assert!(wait_gone(pid));
    let _ = std::fs::remove_dir_all(&dir);
}

/// `--detach` and `--map` exclude each other.
#[test]
fn detach_conflicts_with_map() {
    let output = Command::new(env!("CARGO_BIN_EXE_dak"))
        .args(["--detach", "--map"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(exit::USAGE as i32));
}

/// Without `--detach` the pid file is written too (replacing a stale one) and removed
/// on exit - it only is when it names this process, so gone means it was replaced.
#[test]
fn pid_file_without_detach_is_written_and_removed() {
    let dir = common::temp_dir();
    let pid_file = dir.join("dak.pid");
    std::fs::write(&pid_file, "999999\n").unwrap();
    let config = common::write_temp_config(r#"{"scenes": {"on_start": {}}, "devices": {}}"#);
    let output = Command::new(env!("CARGO_BIN_EXE_dak"))
        .args(["-c", config.to_str().unwrap(), "--pid-file"])
        .arg(&pid_file)
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&config);
    assert_eq!(output.status.code(), Some(exit::NO_DEVICE as i32));
    assert!(!pid_file.exists(), "the pid file is removed on exit");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A pid file that cannot be written fails the start.
#[test]
fn unwritable_pid_file_fails() {
    let config = common::write_temp_config(r#"{"scenes": {"on_start": {}}, "devices": {}}"#);
    let output = Command::new(env!("CARGO_BIN_EXE_dak"))
        .args([
            "-c",
            config.to_str().unwrap(),
            "--pid-file",
            "/nonexistent/dir/dak.pid",
        ])
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&config);
    assert_eq!(output.status.code(), Some(exit::FAILURE as i32));
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot write pid file"));
}

/// The full daemon life cycle: `--detach` returns 0 once the daemon is up, the daemon
/// runs in its own session without a terminal, systemd hears READY and STOPPING, the
/// log file gets its lines, and SIGTERM stops it and removes its pid file.
#[test]
fn detached_daemon_life_cycle() {
    let Some((config, key)) = common::config_for_attached_device() else {
        eprintln!("skipping: no keypad attached");
        return;
    };
    let dir = common::temp_dir();
    let _held = dak::lock::try_lock(&dir, &key).unwrap();
    let socket_path = dir.join("notify.sock");
    let socket = UnixDatagram::bind(&socket_path).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let pid_file = dir.join("dak.pid");
    let log_file = dir.join("dak.log");

    let output = Command::new(env!("CARGO_BIN_EXE_dak"))
        .args(["-c", config.to_str().unwrap(), "--wait", "--detach"])
        .arg("--pid-file")
        .arg(&pid_file)
        .arg("--log-file")
        .arg(&log_file)
        .env(dak::lock::LOCK_DIR_ENV, &dir)
        .env("NOTIFY_SOCKET", &socket_path)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("running in the background: 1 device waiting for another dak"),
        "{stdout}"
    );

    let mut buffer = [0u8; 512];
    let read = socket.recv(&mut buffer).unwrap();
    assert!(buffer[..read].starts_with(b"READY=1\nSTATUS="));

    let pid: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(alive(pid));
    // Its own session (but not its leader), no controlling terminal, working dir `/`.
    let sid = unsafe { libc::getsid(pid) };
    assert!(sid > 0 && sid != pid, "sid {sid} pid {pid}");
    assert_ne!(sid, unsafe { libc::getsid(0) });
    if Path::new("/proc/self/cwd").exists() {
        assert_eq!(
            std::fs::read_link(format!("/proc/{pid}/cwd")).unwrap(),
            Path::new("/")
        );
    }

    unsafe { libc::kill(pid, libc::SIGTERM) };
    let read = socket.recv(&mut buffer).unwrap();
    assert_eq!(&buffer[..read], b"STOPPING=1");
    assert!(wait_gone(pid), "the daemon stops on SIGTERM");
    assert!(!pid_file.exists(), "the pid file is removed");
    let log = std::fs::read_to_string(&log_file).unwrap();
    assert!(log.contains("waiting for it to be released"), "{log}");
    assert!(log.contains("received SIGTERM"), "{log}");

    let _ = std::fs::remove_file(&config);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Reads datagrams from `socket` until one starts with `prefix`, returning it.
fn recv_until(socket: &UnixDatagram, prefix: &str) -> String {
    let mut buffer = [0u8; 1024];
    loop {
        let read = socket
            .recv(&mut buffer)
            .expect("a notification arrives in time");
        let text = String::from_utf8_lossy(&buffer[..read]).into_owned();
        if text.starts_with(prefix) {
            return text;
        }
    }
}

/// Waits up to five seconds for `path` to contain `needle`, returning its content.
fn wait_for_log(path: &Path, needle: &str) -> String {
    for _ in 0..500 {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        if text.contains(needle) {
            return text;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!(
        "{} never contained {needle:?}:\n{}",
        path.display(),
        std::fs::read_to_string(path).unwrap_or_default()
    );
}

/// Under systemd (NOTIFY_SOCKET set) dak stays up without any device, reporting how to
/// rescan; SIGUSR1 rescans, SIGHUP with a broken config keeps the running one (and
/// tells systemd RELOADING then READY), SIGHUP with a valid one reloads, and SIGTERM
/// still ends it with status 0.
#[test]
fn service_without_devices_waits_and_handles_reload_and_rescan() {
    let dir = common::temp_dir();
    let config = dir.join("config.json");
    std::fs::write(&config, r#"{"scenes": {"on_start": {}}, "devices": {}}"#).unwrap();
    let log_file = dir.join("dak.log");
    let socket_path = dir.join("notify.sock");
    let socket = UnixDatagram::bind(&socket_path).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();

    let mut child = Running(
        Command::new(env!("CARGO_BIN_EXE_dak"))
            .arg("-c")
            .arg(&config)
            .arg("--log-file")
            .arg(&log_file)
            .env("NOTIFY_SOCKET", &socket_path)
            .env(dak::lock::LOCK_DIR_ENV, &dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let pid = child.id() as i32;

    let ready = recv_until(&socket, "READY=1");
    assert!(ready.contains("send SIGUSR1 to rescan"), "{ready}");

    unsafe { libc::kill(pid, libc::SIGUSR1) };
    wait_for_log(&log_file, "received SIGUSR1; looking for missing devices");

    std::fs::write(&config, "{ broken").unwrap();
    unsafe { libc::kill(pid, libc::SIGHUP) };
    assert!(recv_until(&socket, "RELOADING=1").contains("MONOTONIC_USEC="));
    recv_until(&socket, "READY=1");
    wait_for_log(
        &log_file,
        "configuration not reloaded; still running the previous one",
    );

    std::fs::write(
        &config,
        r#"{"logging": {"level": "debug"}, "scenes": {"on_start": {}}, "devices": {}}"#,
    )
    .unwrap();
    unsafe { libc::kill(pid, libc::SIGHUP) };
    recv_until(&socket, "RELOADING=1");
    recv_until(&socket, "READY=1");
    wait_for_log(&log_file, "configuration reloaded");

    assert!(child.try_wait().unwrap().is_none(), "still running");
    unsafe { libc::kill(pid, libc::SIGTERM) };
    recv_until(&socket, "STOPPING=1");
    assert_eq!(child.wait().unwrap().code(), Some(0));
    let _ = std::fs::remove_dir_all(&dir);
}

/// SIGHUP reopens the log file, so logrotate can move it away.
#[test]
fn sighup_reopens_the_log_file() {
    let dir = common::temp_dir();
    let config = dir.join("config.json");
    std::fs::write(&config, r#"{"scenes": {"on_start": {}}, "devices": {}}"#).unwrap();
    let log_file = dir.join("dak.log");
    let socket_path = dir.join("notify.sock");
    let socket = UnixDatagram::bind(&socket_path).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut child = Running(
        Command::new(env!("CARGO_BIN_EXE_dak"))
            .arg("-c")
            .arg(&config)
            .arg("--log-file")
            .arg(&log_file)
            .env("NOTIFY_SOCKET", &socket_path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let pid = child.id() as i32;
    recv_until(&socket, "READY=1");
    let rotated = dir.join("dak.log.1");
    std::fs::rename(&log_file, &rotated).unwrap();
    unsafe { libc::kill(pid, libc::SIGHUP) };
    recv_until(&socket, "READY=1");
    let fresh = wait_for_log(&log_file, "configuration reloaded");
    assert!(!fresh.contains("DAK (Dynamic Ajazz Keyboard)"), "{fresh}");
    assert!(std::fs::read_to_string(&rotated)
        .unwrap()
        .contains("received SIGHUP"));
    unsafe { libc::kill(pid, libc::SIGTERM) };
    assert_eq!(child.wait().unwrap().code(), Some(0));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A device definition no attached keypad can match (its serial is made up).
const ABSENT_DEVICE: &str = r#"{"1": {
    "device_id": "0300:3002", "device_name": "absent", "serial": "DAK-TEST-ABSENT",
    "key_count": 9, "encoder_count": 3, "screens": 6,
    "buttons": [{"number": 1, "press": 1, "release": 1, "screen": true, "draw_id": 1}],
    "encoders": []
}}"#;

/// A service whose configured device is absent: SIGUSR1 looks for it and says it is
/// still missing; a reload prints the new config's warnings; a reload whose log file
/// cannot be opened keeps logging to the previous one.
#[test]
fn service_rescans_for_absent_devices_and_reports_reload_problems() {
    let dir = common::temp_dir();
    let log_file = dir.join("dak.log");
    let config = dir.join("config.json");
    let write_config = |logging_file: &Path, scenes: &str| {
        std::fs::write(
            &config,
            format!(
                r#"{{"logging": {{"output": "file", "file": "{}"}},
                    "scenes": {scenes}, "devices": {ABSENT_DEVICE}}}"#,
                logging_file.display()
            ),
        )
        .unwrap();
    };
    write_config(&log_file, r#"{"on_start": {}}"#);
    let socket_path = dir.join("notify.sock");
    let socket = UnixDatagram::bind(&socket_path).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut child = Running(
        Command::new(env!("CARGO_BIN_EXE_dak"))
            .arg("-c")
            .arg(&config)
            .env("NOTIFY_SOCKET", &socket_path)
            .env(dak::lock::LOCK_DIR_ENV, &dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let pid = child.id() as i32;
    recv_until(&socket, "READY=1");
    wait_for_log(&log_file, "DAK-TEST-ABSENT");

    unsafe { libc::kill(pid, libc::SIGUSR1) };
    wait_for_log(&log_file, "rescan: no missing device was found");

    write_config(&log_file, r#"{"on_start": {"actions": {"1b01": {}}}}"#);
    unsafe { libc::kill(pid, libc::SIGHUP) };
    recv_until(&socket, "READY=1");
    let text = wait_for_log(&log_file, "configuration reloaded");
    assert!(text.contains("warning: "), "{text}");

    write_config(
        Path::new("/proc/dak-no-such-dir/dak.log"),
        r#"{"on_start": {}}"#,
    );
    unsafe { libc::kill(pid, libc::SIGHUP) };
    recv_until(&socket, "READY=1");
    wait_for_log(&log_file, "keeping the previous log outputs");

    unsafe { libc::kill(pid, libc::SIGTERM) };
    assert_eq!(child.wait().unwrap().code(), Some(0));
    let _ = std::fs::remove_dir_all(&dir);
}
