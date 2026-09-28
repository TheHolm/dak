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

/// A config written for a newer major schema version is a configuration error (3), and
/// the message says a newer dak is needed.
#[test]
fn newer_major_config_version_exits_with_config_status() {
    let path = common::write_temp_config(r#"{"version": "2.0", "scenes": {}, "devices": {}}"#);
    let output = run_dak(&["-c", path.to_str().unwrap()]);
    let _ = std::fs::remove_file(&path);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(status_of(&output), exit::CONFIG, "{stderr}");
    assert!(stderr.contains("needs a newer dak"), "{stderr}");
}

/// A newer minor schema version still starts (here reaching "no device", 4), printing
/// the version warning on the way.
#[test]
fn newer_minor_config_version_only_warns() {
    let path = common::write_temp_config(
        r#"{"version": "1.9", "scenes": {"on_start": {}}, "devices": {}}"#,
    );
    let output = run_dak(&["-c", path.to_str().unwrap()]);
    let _ = std::fs::remove_file(&path);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(status_of(&output), exit::NO_DEVICE, "{stderr}");
    assert!(stderr.contains("config version 1.9 is newer"), "{stderr}");
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

/// Started in a directory any user can write to (like `/tmp`), dak does not load the
/// `config.json` there - someone else may have put it there to run their commands - and
/// says so; with no other config it then fails to find one.
#[test]
fn config_in_a_shared_directory_is_ignored() {
    use std::os::unix::fs::PermissionsExt;
    let dir = common::temp_dir();
    let home = common::temp_dir();
    std::fs::write(
        dir.join("config.json"),
        r#"{"scenes": {"on_start": {}}, "devices": {}}"#,
    )
    .unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o1777)).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_dak"))
        .current_dir(&dir)
        .env("HOME", &home)
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&home);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("ignoring config"), "stderr: {stderr}");
    assert_eq!(status_of(&output), exit::CONFIG, "stderr: {stderr}");
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

/// A device definition no attached keypad can match (its serial is made up), so a run
/// with it finds nothing to drive, whatever is plugged in.
const ABSENT_DEVICE: &str = r#"{"1": {
    "device_id": "0300:3002", "device_name": "absent", "serial": "DAK-TEST-ABSENT",
    "key_count": 9, "encoder_count": 3, "screens": 6,
    "buttons": [{"number": 1, "press": 1, "release": 1, "screen": true, "draw_id": 1}],
    "encoders": []
}}"#;

/// `dak --map` with its standard input closed never spins: with no keypad attached it
/// exits with the no-device status, and with one attached the first question sees end
/// of input and the wizard stops with a failure instead of re-asking forever.
#[test]
fn map_with_closed_stdin_ends() {
    let dir = common::temp_dir();
    let mut child = Command::new(env!("CARGO_BIN_EXE_dak"))
        .arg("--map")
        .env(dak::lock::LOCK_DIR_ENV, &dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // A spinning wizard never exits; give it ten seconds before calling it a hang.
    let mut finished = None;
    for _ in 0..1000 {
        if let Some(status) = child.try_wait().unwrap() {
            finished = Some(status);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    if finished.is_none() {
        let _ = child.kill();
    }
    let output = child.wait_with_output().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(finished.is_some(), "--map kept running with stdin closed");
    match status_of(&output) {
        exit::NO_DEVICE => assert!(stderr.contains("no compatible devices found"), "{stderr}"),
        exit::FAILURE => assert!(
            stderr.contains("input ended before the mapping"),
            "{stderr}"
        ),
        other => panic!("unexpected status {other}: {stderr}"),
    }
}

/// Config warnings are printed at startup (before the run finds no device), and with
/// `-d fonts` so is the lookup order of the configured fonts.
#[test]
fn startup_prints_config_warnings_and_font_details() {
    let font = format!("{}/fonts/DejaVuSansMono.ttf", env!("CARGO_MANIFEST_DIR"));
    let config = common::write_temp_config(&format!(
        r#"{{"defaults": {{"fonts": {{"regular": "{font}"}}}},
            "scenes": {{"on_start": {{"actions": {{"1b01": {{}}}}}}}}, "devices": {{}}}}"#
    ));
    let output = run_dak(&["-c", config.to_str().unwrap(), "-d", "fonts"]);
    let _ = std::fs::remove_file(&config);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(status_of(&output), exit::NO_DEVICE, "{stderr}");
    assert!(stderr.contains("warning: "), "{stderr}");
    assert!(
        stdout.contains("DejaVuSansMono.ttf") || stderr.contains("DejaVuSansMono.ttf"),
        "the font lookup order names the configured font:\n{stdout}\n{stderr}"
    );
}

/// A file log output with neither `logging.file` nor a default location (`HOME` and
/// `XDG_STATE_HOME` unset) is a config error.
#[test]
fn file_output_without_a_location_is_a_config_error() {
    let config = common::write_temp_config(
        r#"{"logging": {"output": "file"}, "scenes": {"on_start": {}}, "devices": {}}"#,
    );
    let output = Command::new(env!("CARGO_BIN_EXE_dak"))
        .args(["-c", config.to_str().unwrap()])
        .env_remove("HOME")
        .env_remove("XDG_STATE_HOME")
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&config);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(status_of(&output), exit::CONFIG, "{stderr}");
    assert!(stderr.contains("no default location"), "{stderr}");
}

/// A configured device that is not attached is reported, and with nothing else to
/// run a foreground dak exits with the no-device status.
#[test]
fn configured_but_absent_device_exits_with_no_device_status() {
    let config = common::write_temp_config(&format!(
        r#"{{"scenes": {{"on_start": {{}}}}, "devices": {ABSENT_DEVICE}}}"#
    ));
    let output = run_dak(&["-c", config.to_str().unwrap()]);
    let _ = std::fs::remove_file(&config);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(status_of(&output), exit::NO_DEVICE, "{stderr}");
    assert!(
        stderr.contains("DAK-TEST-ABSENT") && stderr.contains("was not found"),
        "{stderr}"
    );
}
