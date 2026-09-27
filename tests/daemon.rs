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

/// A daemon that fails during startup hands its status and last error back to the
/// terminal it was started from.
#[test]
fn detach_reports_startup_failure_with_its_status() {
    let config = common::write_temp_config(r#"{"scenes": {"on_start": {}}, "devices": {}}"#);
    let output = Command::new(env!("CARGO_BIN_EXE_dak"))
        .args([
            "-c",
            config.to_str().unwrap(),
            "--detach",
            "--log-level",
            "error",
        ])
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&config);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(exit::NO_DEVICE as i32),
        "{stderr}"
    );
    assert!(
        stderr.contains("failed to start in the background (exit status 4): no device defined"),
        "{stderr}"
    );
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
