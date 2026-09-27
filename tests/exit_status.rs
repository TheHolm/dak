//! Exit statuses of the real `dak` binary, run as a child process.
//!
//! Each failure class has its own status (see `src/exit.rs`) so service managers can
//! tell them apart - e.g. systemd's `RestartPreventExitStatus=` must see a broken config
//! as 3, not as the generic 1 every failure used to produce. These tests run the built
//! binary with configs that fail before any device is opened, so they need no hardware.

mod common;

use std::process::{Command, Output};

use dak::exit;

/// Runs the `dak` binary with `args` and returns its output.
fn run_dak(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_dak"))
        .args(args)
        .output()
        .expect("the dak binary runs")
}

/// The exit status of a finished `dak` run, panicking (with its output) if it was killed
/// by a signal instead.
fn status_of(output: &Output) -> u8 {
    output.status.code().unwrap_or_else(|| {
        panic!(
            "dak was killed by a signal: {output:?}\nstderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    }) as u8
}

/// An unknown flag is a usage error (2), reported before the banner's work begins.
#[test]
fn unknown_flag_exits_with_usage_status() {
    let output = run_dak(&["--no-such-flag"]);
    assert_eq!(status_of(&output), exit::USAGE);
}

/// A config file that does not exist is a configuration error (3).
#[test]
fn missing_config_exits_with_config_status() {
    let output = run_dak(&["-c", "/nonexistent/dak/config.json"]);
    assert_eq!(status_of(&output), exit::CONFIG);
    assert!(String::from_utf8_lossy(&output.stderr).contains("Couldn't open"));
}

/// A config that is not valid JSON is a configuration error (3), not a generic failure.
#[test]
fn invalid_config_exits_with_config_status() {
    let path = common::write_temp_config("{ not json");
    let output = run_dak(&["-c", path.to_str().unwrap()]);
    let _ = std::fs::remove_file(&path);
    assert_eq!(status_of(&output), exit::CONFIG);
}

/// A config that defines no devices finds no device to drive (4), whether or not a
/// keypad is attached (an attached one is only reported as undefined).
#[test]
fn config_without_devices_exits_with_no_device_status() {
    let path = common::write_temp_config(r#"{"scenes": {"on_start": {}}, "devices": {}}"#);
    let output = run_dak(&["-c", path.to_str().unwrap()]);
    let _ = std::fs::remove_file(&path);
    assert_eq!(status_of(&output), exit::NO_DEVICE);
    assert!(String::from_utf8_lossy(&output.stderr).contains("no device defined in config"));
}

/// A relative `-c` path is reported (and loaded) as an absolute one, so it keeps naming
/// the same file whatever the working directory later becomes.
#[test]
fn relative_config_path_is_made_absolute() {
    let dir = common::temp_dir();
    std::fs::write(
        dir.join("rel.json"),
        r#"{"scenes": {"on_start": {}}, "devices": {}}"#,
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_dak"))
        .args(["-c", "rel.json"])
        .current_dir(&dir)
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let expected = format!("Using config: {}", dir.join("rel.json").display());
    assert!(stdout.contains(&expected), "stdout: {stdout}");
}

/// A configured keypad held by another dak is skipped with a message naming the
/// holder, and with nothing left to drive the program exits with status 5.
#[test]
fn device_held_by_another_instance_exits_with_busy_status() {
    let Some((config, key)) = common::config_for_attached_device() else {
        eprintln!("skipping: no keypad attached");
        return;
    };
    let dir = common::temp_dir();
    let _held = dak::lock::try_lock(&dir, &key).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_dak"))
        .args(["-c", config.to_str().unwrap()])
        .env(dak::lock::LOCK_DIR_ENV, &dir)
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&config);
    let _ = std::fs::remove_dir_all(&dir);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(status_of(&output), exit::DEVICE_BUSY, "{stderr}");
    assert!(stderr.contains("is in use by dak (user"), "{stderr}");
    assert!(stderr.contains("--wait"), "{stderr}");
}

/// With `--wait` the program waits for the held keypad instead, and `SIGTERM` ends
/// that wait cleanly (status 0).
#[test]
fn waiting_for_a_held_device_stops_cleanly_on_sigterm() {
    let Some((config, key)) = common::config_for_attached_device() else {
        eprintln!("skipping: no keypad attached");
        return;
    };
    let dir = common::temp_dir();
    let _held = dak::lock::try_lock(&dir, &key).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_dak"))
        .args(["-c", config.to_str().unwrap(), "--wait"])
        .env(dak::lock::LOCK_DIR_ENV, &dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(700));
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let output = child.wait_with_output().unwrap();
    let _ = std::fs::remove_file(&config);
    let _ = std::fs::remove_dir_all(&dir);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(status_of(&output), exit::SUCCESS, "{stdout}");
    assert!(stdout.contains("waiting for it to be released"), "{stdout}");
    assert!(stdout.contains("received SIGTERM"), "{stdout}");
}

/// `--wait` and `--replace` exclude each other (a usage error).
#[test]
fn wait_and_replace_conflict() {
    let output = run_dak(&["--wait", "--replace"]);
    assert_eq!(status_of(&output), exit::USAGE);
}
