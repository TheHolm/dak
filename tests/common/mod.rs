//! Shared test utilities for config integration tests.
//! Each test crate compiles this module as its own; not every crate uses every helper,
//! so unused-item warnings are expected and suppressed.

#![allow(dead_code)]

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use dak::actions::load_config_from_path;

static TMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Writes `contents` to a unique temp file and returns its path.
pub fn write_temp_config(contents: &str) -> PathBuf {
    let unique = TMP_COUNTER.fetch_add(1, Ordering::SeqCst);
    let path = format!("/tmp/dak_config_{}.json", unique);
    fs::write(&path, contents).unwrap();
    PathBuf::from(path)
}

/// Wraps `scenes_json` (a scenes object) in the top-level config structure and writes it
/// to a unique temp file, returning its path. The `devices` section is left empty.
pub fn write_scenes_config(scenes_json: &str) -> PathBuf {
    write_temp_config(&format!("{{\"scenes\": {scenes_json}, \"devices\": {{}}}}"))
}

/// Wraps `scenes_json` and `defaults_json` in the top-level config structure and writes it
/// to a unique temp file, returning its path. The `devices` section is left empty.
pub fn write_config_with_defaults(defaults_json: &str, scenes_json: &str) -> PathBuf {
    write_temp_config(&format!(
        "{{\"scenes\": {scenes_json}, \"devices\": {{}}, \"defaults\": {defaults_json}}}"
    ))
}

/// Wraps `variables_json` and `scenes_json` in the top-level config structure and writes it
/// to a unique temp file, returning its path. The `devices` section is left empty.
pub fn write_variables_config(variables_json: &str, scenes_json: &str) -> PathBuf {
    write_temp_config(&format!(
        "{{\"scenes\": {scenes_json}, \"devices\": {{}}, \"variables\": {variables_json}}}"
    ))
}

/// Loads `scenes_json` (wrapped into the top-level config structure) and asserts the
/// returned errors contain `expected`.
pub fn assert_validation_error(scenes_json: &str, expected: &str) {
    let path = write_scenes_config(scenes_json);
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = fs::remove_file(&path);
    let errors = error_texts(config.unwrap_err());
    assert!(
        errors.contains(expected),
        "expected errors to contain \"{expected}\", got: {errors}"
    );
}

/// Joins loader error messages into a single searchable string.
pub fn error_texts(err: Vec<String>) -> String {
    err.join("\n")
}

/// Serialises the tests that mutate `HOME`, so their environment changes never
/// interleave with each other while the shared process-global variable is in use.
///
/// Mirrors `src/actions.rs`'s own unit-test-only `ENV_LOCK`/`SetHome` pair: each
/// integration test binary compiles as its own crate, so this module needs its own
/// copy rather than sharing that `#[cfg(test)]`-only one (which isn't reachable from
/// outside the `dak` crate anyway).
pub static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Temporarily sets `HOME` to `home`, restoring the previous value when dropped.
/// Callers must hold [`ENV_LOCK`] for the guard's entire lifetime.
pub struct SetHome(Option<std::ffi::OsString>);

impl SetHome {
    pub fn new(home: &std::path::Path) -> Self {
        let old = std::env::var_os("HOME");
        std::env::set_var("HOME", home);
        SetHome(old)
    }
}

impl Drop for SetHome {
    fn drop(&mut self) {
        match &self.0 {
            Some(old) => std::env::set_var("HOME", old),
            None => std::env::remove_var("HOME"),
        }
    }
}

/// Creates a unique empty temp directory, e.g. for use as a fake `$HOME`.
pub fn temp_dir() -> PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = format!("/tmp/dak_test_home_{}_{n}", std::process::id());
    fs::create_dir_all(&dir).unwrap();
    PathBuf::from(dir)
}
