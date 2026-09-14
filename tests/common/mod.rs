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
