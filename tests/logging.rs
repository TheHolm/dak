//! The `logging` config section and the log outputs, end to end: config loading
//! validates the section, and the real binary writes to the journal format, a log file
//! and the level filter as configured. Every run fails before a device is opened (the
//! configs define no devices), so no hardware is needed.

mod common;

use std::os::unix::fs::MetadataExt;
use std::process::{Command, Stdio};

use dak::actions::load_config_from_path;
use dak::log::{Level, Output};

/// A config with the given `logging` section and no devices.
fn logging_config(logging: &str) -> std::path::PathBuf {
    common::write_temp_config(&format!(
        r#"{{"logging": {logging}, "scenes": {{"on_start": {{}}}}, "devices": {{}}}}"#
    ))
}

/// A valid section is loaded into `LoadedConfig::logging`.
#[test]
fn logging_section_is_loaded() {
    let path = logging_config(r#"{"output": ["syslog"], "level": "warning"}"#);
    let config = load_config_from_path(path.to_str().unwrap()).unwrap();
    let _ = std::fs::remove_file(&path);
    assert_eq!(config.logging.outputs, vec![Output::Syslog]);
    assert_eq!(config.logging.level, Level::Warning);
}

/// A config without the section gets the defaults.
#[test]
fn missing_logging_section_uses_defaults() {
    let path = common::write_scenes_config(r#"{"on_start": {}}"#);
    let config = load_config_from_path(path.to_str().unwrap()).unwrap();
    let _ = std::fs::remove_file(&path);
    assert_eq!(config.logging, dak::log::LoggingConfig::default());
}

/// A bad section fails loading like any other config error.
#[test]
fn invalid_logging_section_fails_loading() {
    let path = logging_config(r#"{"level": "chatty"}"#);
    let errors = load_config_from_path(path.to_str().unwrap()).unwrap_err();
    let _ = std::fs::remove_file(&path);
    assert!(common::error_texts(errors).contains("unknown level \"chatty\""));
}

/// When stderr is the stream `JOURNAL_STREAM` names, `auto` writes journal lines:
/// everything on stderr with `<N>` priority prefixes (and nothing on stdout).
#[test]
fn auto_output_writes_journal_lines_under_systemd() {
    let dir = common::temp_dir();
    let stderr_path = dir.join("stderr");
    let stderr = std::fs::File::create(&stderr_path).unwrap();
    let meta = stderr.metadata().unwrap();
    let config = logging_config("{}");
    let output = Command::new(env!("CARGO_BIN_EXE_dak"))
        .args(["-c", config.to_str().unwrap()])
        .env("JOURNAL_STREAM", format!("{}:{}", meta.dev(), meta.ino()))
        .stdout(Stdio::piped())
        .stderr(stderr)
        .output()
        .unwrap();
    let text = std::fs::read_to_string(&stderr_path).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_file(&config);
    assert!(output.stdout.is_empty(), "stdout: {:?}", output.stdout);
    assert!(text.contains("<6>DAK (Dynamic Ajazz Keyboard)"), "{text}");
    assert!(
        text.contains("<3>error: no device defined in config was found"),
        "{text}"
    );
}

/// A stale `JOURNAL_STREAM` (inherited, but stderr is somewhere else) is ignored.
#[test]
fn mismatched_journal_stream_keeps_console_output() {
    let config = logging_config("{}");
    let output = Command::new(env!("CARGO_BIN_EXE_dak"))
        .args(["-c", config.to_str().unwrap()])
        .env("JOURNAL_STREAM", "1:1")
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&config);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.starts_with("DAK (Dynamic Ajazz Keyboard)"),
        "{stdout}"
    );
}

/// `--log-file` adds a timestamped file next to the console, and `--log-level error`
/// hides everything but errors on both.
#[test]
fn log_file_and_level_from_the_command_line() {
    let dir = common::temp_dir();
    let log_path = dir.join("nested").join("dak.log");
    let config = logging_config("{}");
    let output = Command::new(env!("CARGO_BIN_EXE_dak"))
        .args(["-c", config.to_str().unwrap(), "--log-level", "error"])
        .arg("--log-file")
        .arg(&log_path)
        .output()
        .unwrap();
    let text = std::fs::read_to_string(&log_path).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_file(&config);
    assert!(output.stdout.is_empty(), "info lines are filtered out");
    assert!(String::from_utf8_lossy(&output.stderr).contains("error: no device"));
    assert_eq!(text.lines().count(), 1, "{text}");
    assert!(text.contains(" error: no device defined in config was found"));
    assert!(
        text.starts_with("20"),
        "file lines start with a timestamp: {text}"
    );
}

/// A log file that cannot be opened is a configuration error (status 3).
#[test]
fn unopenable_log_file_is_a_config_error() {
    let config = logging_config(r#"{"output": ["file"], "file": "/proc/no/such/dir/dak.log"}"#);
    let output = Command::new(env!("CARGO_BIN_EXE_dak"))
        .args(["-c", config.to_str().unwrap()])
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&config);
    assert_eq!(output.status.code(), Some(dak::exit::CONFIG as i32));
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot create log directory"));
}

/// An unknown `--log-level` is a usage error.
#[test]
fn unknown_log_level_is_a_usage_error() {
    let output = Command::new(env!("CARGO_BIN_EXE_dak"))
        .args(["--log-level", "chatty"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(dak::exit::USAGE as i32));
}

/// What dak prints never carries a raw escape sequence, even when the text comes from
/// outside (here the config path given on the command line): the terminal gets a
/// visible `\u{1b}` instead of a colour change.
#[test]
fn printed_lines_escape_control_characters() {
    let output = Command::new(env!("CARGO_BIN_EXE_dak"))
        .args(["-c", "/nonexistent/dak\u{1b}[31mred.json"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let all = [output.stdout, output.stderr].concat();
    let text = String::from_utf8_lossy(&all);
    assert!(!all.contains(&0x1b), "raw ESC printed: {text}");
    assert!(text.contains("dak\\u{1b}[31mred.json"), "{text}");
}
