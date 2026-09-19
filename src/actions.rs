use serde_json::{self, Value};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::baseplane::{Kind, Reference};
use crate::log::{Log, Subsystem};
use crate::map::Mapping;
use crate::press::Defaults;
use image::DynamicImage;
use mirajazz::device::Device;
use mirajazz::error::MirajazzError;
use mirajazz::types::ImageFormat;
use std::process::Command as StdCommand;
use tokio::sync::mpsc;

use std::os::unix::process::CommandExt as _;

/// Parsed and validated config plus any non-fatal warnings collected while validating it.
#[derive(Debug)]
pub struct LoadedConfig {
    /// The optional top-level `version` string, defaulting to [`DEFAULT_CONFIG_VERSION`]
    /// when absent. Not currently interpreted (no version-specific parsing exists yet) -
    /// just carried through and printed on startup, so future config schema changes have
    /// somewhere to record which shape a file was written for.
    pub version: String,
    /// The validated `scenes` section: a dictionary whose keys are scene names.
    pub scenes: Value,
    /// The validated `devices` section, keyed by logical device id.
    pub devices: ConfiguredDevices,
    /// The settings from the `defaults` section (press-detection timing knobs plus
    /// connect-time brightness), with built-in defaults applied.
    pub defaults: Defaults,
    /// Non-fatal warnings collected while validating the config.
    pub warnings: Vec<String>,
}

/// The config `version` assumed when the top-level `version` key is absent.
pub const DEFAULT_CONFIG_VERSION: &str = "1.0";

/// Device definitions from the config `devices` section, keyed by logical device id.
///
/// The ids are the digits control references use (`1b01` addresses device `1`). Each
/// definition describes one physical device in exactly the shape `dak --map` prints it;
/// the runtime matches these definitions against the discovered hardware and drives every
/// matched device.
#[derive(Debug, Default)]
pub struct ConfiguredDevices {
    /// The device definitions, keyed by logical device id in ascending order.
    pub by_id: BTreeMap<u8, Mapping>,
}

/// Loads and validates `config.json` from the current working directory.
pub fn load_config() -> Result<LoadedConfig, Vec<String>> {
    load_config_from_path("config.json")
}

/// The name every search location is probed for when no explicit config path is given.
const DEFAULT_CONFIG_FILE: &str = "config.json";

/// The directories searched for the default `config.json`, in priority order:
/// `~/.config/dak/`, the current working directory and the directory holding the
/// running binary. Locations that cannot be determined are skipped.
fn config_search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join(".config").join("dak"));
    }
    if let Ok(cwd) = std::env::current_dir() {
        dirs.push(cwd);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            dirs.push(dir.to_path_buf());
        }
    }
    dirs
}

/// Picks the config file to load.
///
/// An explicitly provided `explicit` path wins as-is. Otherwise the first existing
/// `DEFAULT_CONFIG_FILE` is returned from `dirs` (in priority order); when none of
/// them contains a config, the first directory's candidate is returned so loading
/// reports the missing file instead of guessing a location, and an empty directory
/// list falls back to a bare `config.json` in the current directory.
pub fn pick_config_path(explicit: Option<&Path>, dirs: &[PathBuf]) -> PathBuf {
    if let Some(path) = explicit {
        return path.to_path_buf();
    }
    for dir in dirs {
        let candidate = dir.join(DEFAULT_CONFIG_FILE);
        if candidate.exists() {
            return candidate;
        }
    }
    dirs.first()
        .map(|dir| dir.join(DEFAULT_CONFIG_FILE))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_FILE))
}

/// Resolves the config file the program should load.
///
/// An explicitly passed path (from `-c`/`--config`) is used as-is; without one the
/// first existing `config.json` wins from the [`config_search_dirs`] order —
/// `~/.config/dak/`, then the current directory, then the binary's directory. See
/// [`pick_config_path`] for the no-match behaviour.
pub fn resolve_config_path(explicit: Option<&Path>) -> PathBuf {
    pick_config_path(explicit, &config_search_dirs())
}

/// Loads, parses and validates the config file at `path`.
///
/// Returns vector of error messages with `Err` on invalid JSON or validation errors.
pub fn load_config_from_path(path: &str) -> Result<LoadedConfig, Vec<String>> {
    let content =
        fs::read_to_string(path).map_err(|e| vec![format!("Couldn't open {path}: {e}")])?;

    let json: Value = serde_json::from_str(&strip_comments(&content))
        .map_err(|error| vec![format_json_error(path, &content, &error)])?;

    validate(&json)
}

/// Renders a human-readable JSON parse error with the offending line and a caret.
fn format_json_error(path: &str, content: &str, error: &serde_json::Error) -> String {
    let line = error.line();
    let column = error.column();
    let context = content.lines().nth(line.saturating_sub(1)).unwrap_or("");
    let caret = format!("{}{}", " ".repeat(column.saturating_sub(1)), "^");
    format!(
        "{path}: invalid JSON at line {line}, column {column}:\n  {context}\n  {caret}\n  {error}"
    )
}

/// Removes `//` line and `/* */` block comments from a JSON config so it can carry
/// comments, while leaving string contents and other JSON text untouched.
///
/// The comment text is replaced with an equal number of spaces, keeping `\n` newlines
/// in place. The output therefore has the same length and the same line starts as the
/// input, so a later JSON parse error still reports line and column numbers relative
/// to the original file. An unterminated `/*` comments out the rest of the file.
pub fn strip_comments(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let chars: Vec<char> = input.chars().collect();
    let mut index = 0;
    let mut in_string = false;
    let mut escaped = false;
    while index < chars.len() {
        let c = chars[index];
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
                index += 1;
            }
            '/' if index + 1 < chars.len() && chars[index + 1] == '/' => {
                while index < chars.len() && chars[index] != '\n' {
                    out.push(' ');
                    index += 1;
                }
            }
            '/' if index + 1 < chars.len() && chars[index + 1] == '*' => {
                index += 2;
                while index + 1 < chars.len() && !(chars[index] == '*' && chars[index + 1] == '/') {
                    out.push(if chars[index] == '\n' { '\n' } else { ' ' });
                    index += 1;
                }
                index = (index + 2).min(chars.len());
            }
            _ => {
                out.push(c);
                index += 1;
            }
        }
    }
    out
}

/// Validates the whole config (the `scenes` and `devices` sections), returning all errors at
/// once or warnings with the parsed config.
fn validate(config: &Value) -> Result<LoadedConfig, Vec<String>> {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();

    let map = match config.as_object() {
        Some(map) => map,
        None => {
            return Err(vec![
                "Config must be a JSON object with \"scenes\" and \"devices\" keys".to_string(),
            ]);
        }
    };

    for key in map.keys() {
        if key != "scenes" && key != "devices" && key != "defaults" && key != "version" {
            errors.push(format!(
                "unknown top-level key \"{key}\", expected \"scenes\", \"devices\", \"defaults\" and \"version\""
            ));
        }
    }

    let missing_scenes = !map.contains_key("scenes");
    let missing_devices = !map.contains_key("devices");
    if missing_scenes {
        errors.push("the config is missing the \"scenes\" section".to_string());
    }
    if missing_devices {
        errors.push("the config is missing the \"devices\" section".to_string());
    }
    if missing_scenes || missing_devices {
        return Err(errors);
    }

    let version = match map.get("version") {
        Some(value) => match value.as_str() {
            Some(version) => version.to_string(),
            None => {
                errors.push(format!(
                    "top-level \"version\" must be a string, got {}",
                    value_type(value)
                ));
                DEFAULT_CONFIG_VERSION.to_string()
            }
        },
        None => DEFAULT_CONFIG_VERSION.to_string(),
    };

    let scenes = map.get("scenes").expect("checked above");
    check_scenes(scenes, &mut warnings, &mut errors);
    let devices = map.get("devices").expect("checked above");
    let by_id = check_devices(devices, &mut errors);
    let defaults = map
        .get("defaults")
        .map(|defaults| check_defaults(defaults, &mut errors))
        .unwrap_or_default();

    if !errors.is_empty() {
        return Err(errors);
    }
    Ok(LoadedConfig {
        version,
        scenes: scenes.clone(),
        devices: ConfiguredDevices { by_id },
        defaults,
        warnings,
    })
}

/// Validates the optional `defaults` section: an object holding the press-detection
/// timing knobs (positive millisecond durations) and the connect-time brightness
/// levels (0-100 percent). Missing keys fall back to [`Defaults::default`].
fn check_defaults(defaults: &Value, errors: &mut Vec<String>) -> Defaults {
    const KNOWN_KEYS: &[&str] = &[
        "short_press_duration",
        "double_click_gap",
        "button_brightness",
        "encoder_brightness",
    ];

    let map = match defaults.as_object() {
        Some(map) => map,
        None => {
            errors.push(format!(
                "config \"defaults\" must be an object, got {}",
                value_type(defaults)
            ));
            return Defaults::default();
        }
    };

    let mut result = Defaults::default();
    for (key, value) in map {
        if !KNOWN_KEYS.contains(&key.as_str()) {
            errors.push(format!(
                "defaults: unknown key \"{key}\", expected one of {}",
                KNOWN_KEYS
                    .iter()
                    .map(|key| format!("\"{key}\""))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            continue;
        }

        match key.as_str() {
            "short_press_duration" | "double_click_gap" => {
                let Some(number) = value.as_u64() else {
                    errors.push(format!(
                        "defaults.{key} must be a positive number of milliseconds, got {}",
                        value_type(value)
                    ));
                    continue;
                };
                if number == 0 {
                    errors.push(format!(
                        "defaults.{key} must be a positive number of milliseconds"
                    ));
                    continue;
                }
                match key.as_str() {
                    "short_press_duration" => {
                        result.short_press_duration = Duration::from_millis(number);
                    }
                    "double_click_gap" => result.double_click_gap = Duration::from_millis(number),
                    _ => unreachable!("checked above"),
                }
            }
            "button_brightness" | "encoder_brightness" => {
                let Some(number) = value.as_u64() else {
                    errors.push(format!(
                        "defaults.{key} must be a number between 0 and 100, got {}",
                        value_type(value)
                    ));
                    continue;
                };
                if number > 100 {
                    errors.push(format!(
                        "defaults.{key} must be a number between 0 and 100, got {number}"
                    ));
                    continue;
                }
                let percent = number as u8;
                match key.as_str() {
                    "button_brightness" => result.button_brightness = percent,
                    "encoder_brightness" => result.encoder_brightness = percent,
                    _ => unreachable!("checked above"),
                }
            }
            _ => unreachable!("unknown keys are rejected above"),
        }
    }
    result
}

/// Validates the `scenes` section: an object whose keys are scene names.
fn check_scenes(scenes: &Value, warnings: &mut Vec<String>, errors: &mut Vec<String>) {
    let map = match scenes.as_object() {
        Some(map) => map,
        None => {
            errors
                .push("config \"scenes\" must be an object whose keys are scene names".to_string());
            return;
        }
    };

    for (scene_name, scene) in map {
        check_scene(scenes, scene_name, scene, warnings, errors);
    }
}

/// Validates the `devices` section: a dictionary keyed by logical device id, each value a
/// device mapping in the shape `dak --map` prints out.
///
/// Invalid ids and mappings are reported; the valid definitions are collected into a map
/// keyed by device id, which holds the definitions that parsed even when other entries
/// failed (the caller only accepts the result when there were no errors at all).
fn check_devices(devices: &Value, errors: &mut Vec<String>) -> BTreeMap<u8, Mapping> {
    let mut by_id = BTreeMap::new();
    let map = match devices.as_object() {
        Some(map) => map,
        None => {
            errors.push("config \"devices\" must be an object of logical device ids".to_string());
            return by_id;
        }
    };

    for (id, value) in map {
        let Some(number) = parse_device_id(id) else {
            errors.push(format!(
                "device id \"{id}\" is not a valid logical device id, expected a single digit from 1 to 9"
            ));
            continue;
        };
        match serde_json::from_value::<Mapping>(value.clone()) {
            Ok(mapping) => {
                by_id.insert(number, mapping);
            }
            Err(error) => errors.push(format!(
                "device \"{id}\" is not a valid device mapping: {error}"
            )),
        }
    }
    by_id
}

/// Parses one logical device id from a `devices` dictionary key.
///
/// Device ids are the digits control references use: exactly one digit from `1` to 9.
fn parse_device_id(id: &str) -> Option<u8> {
    let bytes = id.as_bytes();
    if bytes.len() != 1 || !(b'1'..=b'9').contains(&bytes[0]) {
        return None;
    }
    Some(bytes[0] - b'0')
}

/// Whether a discovered device matches a config device definition.
///
/// A definition whose serial is anything but the "unknown" placeholder only matches a
/// discovered device reporting that exact serial, so identical devices are told apart. A
/// definition whose serial is "unknown" falls back to comparing the VID:PID string, so
/// devices without serials still work as long as only one of their kind is connected.
pub fn discovered_device_matches(
    definition: &Mapping,
    serial: &Option<String>,
    vendor_id: u16,
    product_id: u16,
) -> bool {
    if definition.serial != "unknown" {
        return serial.as_deref() == Some(definition.serial.as_str());
    }
    definition.device_id == format!("{vendor_id:04X}:{product_id:04X}")
}

/// Validates a scene: the reserved `setup` key holds numbered button operations
/// (`type`/`params` dictionaries), the reserved `actions` key holds per-key action
/// bindings. No other scene keys are allowed.
fn check_scene(
    scenes: &Value,
    scene_name: &str,
    scene: &Value,
    warnings: &mut Vec<String>,
    errors: &mut Vec<String>,
) {
    let map = match scene.as_object() {
        Some(map) => map,
        None => {
            errors.push(format!("scene \"{scene_name}\" must be an object"));
            return;
        }
    };

    for (key, value) in map {
        match key.as_str() {
            "setup" => check_setup(scene_name, value, warnings, errors),
            "actions" => check_actions(scenes, scene_name, value, errors, warnings),
            other => errors.push(format!(
                "scene \"{scene_name}\": unknown key \"{other}\", expected \"setup\" or \"actions\""
            )),
        }
    }
}

/// Validates the `setup` dictionary of a scene: numbered button entries with a known
/// `type` and a `params` string suited to that type.
fn check_setup(
    scene_name: &str,
    setup: &Value,
    warnings: &mut Vec<String>,
    errors: &mut Vec<String>,
) {
    let map = match setup.as_object() {
        Some(map) => map,
        None => {
            errors.push(format!(
                "scene \"{scene_name}\": setup must be an object of numbered buttons"
            ));
            return;
        }
    };

    for (key, value) in map {
        check_button_op(scene_name, key, value, warnings, errors);
    }
}

/// Validates one numbered button entry of a scene: a dictionary with a known `type`
/// and a `params` string suited to that type.
fn check_button_op(
    scene_name: &str,
    key: &str,
    value: &Value,
    warnings: &mut Vec<String>,
    errors: &mut Vec<String>,
) {
    let reference = match Reference::parse(key) {
        Ok(reference) => reference,
        Err(error) => {
            errors.push(format!("scene \"{scene_name}\": key \"{key}\" {error}"));
            return;
        }
    };

    let location = format!("key \"{key}\"");
    let object = match value.as_object() {
        Some(object) => object,
        None => {
            errors.push(format!(
                "scene \"{scene_name}\": {location} must be an object with \"type\" and \"params\""
            ));
            return;
        }
    };
    for field in object.keys() {
        if field != "type" && field != "params" && field != "refresh" {
            errors.push(format!(
                "scene \"{scene_name}\": {location} has unknown field \"{field}\", expected \"type\", \"params\" and \"refresh\""
            ));
        }
    }

    let kind = match object.get("type").and_then(|value| value.as_str()) {
        Some(kind) => kind,
        None => {
            errors.push(format!(
                "scene \"{scene_name}\": {location} type must be a string"
            ));
            return;
        }
    };

    // Encoders have no display, so assigning an image to one is a config error; the
    // program must not start with it.
    if reference.kind == Kind::Encoder
        && matches!(kind, "image" | "text" | "image_exec" | "text_exec")
    {
        errors.push(format!(
            "scene \"{scene_name}\": {location} cannot assign {kind} to encoder {reference}"
        ));
        return;
    }

    let params = match object.get("params") {
        Some(value) => match value.as_str() {
            Some(params) => params.trim(),
            None => {
                errors.push(format!(
                    "scene \"{scene_name}\": {location} params must be a string"
                ));
                return;
            }
        },
        None => "",
    };

    // "refresh" (seconds) is optional and defaults to 0, meaning "apply once on scene
    // entry, never again". A nonzero value only makes sense for the types that redraw
    // something: "clear" has nothing left to redraw, and "launch" fires a detached
    // one-off process rather than drawing anything, so a nonzero refresh on either is
    // rejected here rather than silently ignored.
    let refresh = match object.get("refresh") {
        Some(value) => match value.as_u64() {
            Some(refresh) => refresh,
            None => {
                errors.push(format!(
                    "scene \"{scene_name}\": {location} refresh must be a positive number of seconds, got {}",
                    value_type(value)
                ));
                0
            }
        },
        None => 0,
    };
    if refresh != 0 && matches!(kind, "clear" | "launch") {
        errors.push(format!(
            "scene \"{scene_name}\": {location} refresh cannot be used with type \"{kind}\""
        ));
    }

    match kind {
        "image" | "text" => {
            if params.is_empty() {
                errors.push(format!(
                    "scene \"{scene_name}\": {location} {kind} params must be a path"
                ));
            } else {
                check_file_exists(scene_name, &location, params, warnings);
            }
        }
        "image_exec" | "text_exec" | "launch" => {
            if params.is_empty() {
                errors.push(format!(
                    "scene \"{scene_name}\": {location} {kind} params must be a program command line"
                ));
            } else {
                match parse_command_line(params) {
                    Ok(command) => {
                        check_executable(scene_name, &location, &command.program, warnings);
                    }
                    Err(error) => {
                        errors.push(format!("scene \"{scene_name}\": {location}: {error}"))
                    }
                }
            }
        }
        "clear" => {}
        other => {
            errors.push(format!(
                "scene \"{scene_name}\": {location} unknown type \"{other}\", expected image, image_exec, text, text_exec, launch or clear"
            ));
        }
    }
}

/// Validates the `actions` dictionary of a scene: key entries must be object of string events,
/// and the special `timer` key is validated separately.
fn check_actions(
    scenes: &Value,
    scene_name: &str,
    actions: &Value,
    errors: &mut Vec<String>,
    warnings: &mut Vec<String>,
) {
    let map = match actions.as_object() {
        Some(map) => map,
        None => {
            errors.push(format!("scene \"{scene_name}\": actions must be an object"));
            return;
        }
    };

    for (key, action) in map {
        if key == "timer" {
            check_timer(scenes, scene_name, action, errors, warnings);
            continue;
        }

        if let Err(error) = Reference::parse(key) {
            errors.push(format!("scene \"{scene_name}\": actions.\"{key}\" {error}"));
            continue;
        }

        let events = match action.as_object() {
            Some(events) => events,
            None => {
                errors.push(format!(
                    "scene \"{scene_name}\": actions.\"{key}\" must be an object of events"
                ));
                continue;
            }
        };

        if events.is_empty() {
            warnings.push(format!(
                "scene \"{scene_name}\": actions.\"{key}\" defines no events; the entry has no effect"
            ));
        }

        for (event, value) in events {
            let path = format!("actions.\"{key}\".{event}");
            check_action_values(scenes, scene_name, &path, value, errors, warnings);
        }
    }
}

/// Validates the `timer` entry: it must be a single-entry `{ seconds: action }` object.
fn check_timer(
    scenes: &Value,
    scene_name: &str,
    timer: &Value,
    errors: &mut Vec<String>,
    warnings: &mut Vec<String>,
) {
    let entries = match timer.as_object() {
        Some(entries) => entries,
        None => {
            errors.push(format!(
                "scene \"{scene_name}\": actions.timer must be a single-entry object \"{{ seconds: action }}\""
            ));
            return;
        }
    };

    if entries.len() != 1 {
        errors.push(format!(
            "scene \"{scene_name}\": actions.timer must contain exactly one entry, got {}",
            entries.len()
        ));
        return;
    }

    let (seconds, value) = entries.iter().next().unwrap();
    if seconds.parse::<u64>().is_err() {
        errors.push(format!(
            "scene \"{scene_name}\": actions.timer key \"{seconds}\" is not a valid number of seconds"
        ));
    }
    check_action_values(scenes, scene_name, "actions.timer", value, errors, warnings);
}

/// Validates an action value: either a single string (see [`check_action_value`]) or an
/// array of them, run without waiting on each other. An array may contain at most one
/// scene-changing action (a bare `@` or `@scene`, checked here); every other array
/// element is validated exactly like the single-string form. `[]` is the sole way to
/// spell "bound but no action" for the array form (matching `""` for the string form);
/// an empty string *inside* a non-empty array is rejected instead of silently ignored.
fn check_action_values(
    scenes: &Value,
    scene_name: &str,
    path: &str,
    value: &Value,
    errors: &mut Vec<String>,
    warnings: &mut Vec<String>,
) {
    if let Some(value) = value.as_str() {
        check_action_value(scenes, scene_name, path, value, errors, warnings);
        return;
    }
    let Some(items) = value.as_array() else {
        errors.push(format!(
            "scene \"{scene_name}\": {path} must be a string or an array of strings, got {}",
            value_type(value)
        ));
        return;
    };

    let mut scene_changing = 0;
    for (index, item) in items.iter().enumerate() {
        let Some(item) = item.as_str() else {
            errors.push(format!(
                "scene \"{scene_name}\": {path}[{index}] must be a string, got {}",
                value_type(item)
            ));
            continue;
        };
        if item.is_empty() {
            errors.push(format!(
                "scene \"{scene_name}\": {path}[{index}] must not be empty; use an empty array for no action"
            ));
            continue;
        }
        if item.starts_with('@') {
            scene_changing += 1;
        }
        check_action_value(scenes, scene_name, path, item, errors, warnings);
    }
    if scene_changing > 1 {
        errors.push(format!(
            "scene \"{scene_name}\": {path} has {scene_changing} scene-changing actions (@ or @scene); at most one is allowed per event"
        ));
    }
}

/// Validates a single action value: a bare `@` stays, `@scene` must reference an
/// existing scene, a `$path := value` action must target a settable parameter with a
/// value of the right type and in range, and anything else is treated as a command
/// whose executable is checked.
///
/// Breaking change: `~` is no longer special-cased here (it used to mean "stay") - a
/// literal `"~"` value now falls through to the command branch below, same as any other
/// string that isn't `@`/`$`-prefixed.
fn check_action_value(
    scenes: &Value,
    scene_name: &str,
    path: &str,
    value: &str,
    errors: &mut Vec<String>,
    warnings: &mut Vec<String>,
) {
    if let Some(reference) = value.strip_prefix('@') {
        if reference.is_empty() {
            // A bare "@": stay on the current scene. Nothing to validate.
        } else if !scenes.as_object().unwrap().contains_key(reference) {
            errors.push(format!(
                "scene \"{scene_name}\": {path} references undefined scene \"@{reference}\""
            ));
        }
        return;
    }
    if value.starts_with('$') {
        check_set_config_action(scene_name, path, value, errors, warnings);
        return;
    }

    let executable = value.split_whitespace().next().unwrap_or_default();
    if !executable.is_empty() {
        check_executable(scene_name, path, executable, warnings);
    }
}

/// Validates a `$path <op> value` action: `value` must parse as `$<dotted path> <op>
/// <number or "quoted string">` for one of the three [`AssignOp`] operators; `path`
/// must name a currently-settable parameter (only `defaults.button_brightness`/
/// `defaults.encoder_brightness` for now); and the right-hand-side literal must be the
/// type [`SettableDefault::constraint`] expects for that parameter.
///
/// A well-formed path that names a real but immutable config field/section (anything
/// else under `defaults`, or anything under `devices`/`scenes`) is a distinct
/// "read-only parameter" error; a path that doesn't correspond to any real config field
/// at all is a distinct "unknown parameter" error - see [`classify_config_path`]. A
/// wrong-type value (e.g. a quoted string for a numeric parameter) is a distinct error
/// under every operator, since clamping/truncation is a range/length concept, not a
/// type coercion - `"100"` never satisfies a numeric parameter the way `100` does.
///
/// Only `:=` ([`AssignOp::ClampWarn`]) is implemented: an out-of-range number is
/// clamped into its [`Constraint`], with a warning when clamping actually changed the
/// value (an in-range number is silently fine). `=`/`~=` are recognized but rejected
/// with a distinct "not implemented yet" error - see [`AssignOp`]'s doc comments for
/// their intended future behavior.
fn check_set_config_action(
    scene_name: &str,
    path: &str,
    value: &str,
    errors: &mut Vec<String>,
    warnings: &mut Vec<String>,
) {
    let Some((config_path, op, assigned)) = parse_set_config(value) else {
        errors.push(format!(
            "scene \"{scene_name}\": {path} is a malformed \"$\" assignment \"{value}\", expected \"$path := value\" with value a number or a \"quoted string\""
        ));
        return;
    };

    let param = match classify_config_path(&config_path) {
        ParamClass::Settable(param) => param,
        ParamClass::ReadOnly => {
            errors.push(format!(
                "scene \"{scene_name}\": {path} tries to set read-only parameter \"{config_path}\""
            ));
            return;
        }
        ParamClass::Unknown => {
            errors.push(format!(
                "scene \"{scene_name}\": {path} sets unknown parameter \"{config_path}\""
            ));
            return;
        }
    };

    match op {
        AssignOp::Strict => {
            errors.push(format!(
                "scene \"{scene_name}\": {path} uses \"=\" on \"{config_path}\", which is not implemented yet - use \":=\" instead"
            ));
            return;
        }
        AssignOp::ClampSilent => {
            errors.push(format!(
                "scene \"{scene_name}\": {path} uses \"~=\" on \"{config_path}\", which is not implemented yet - use \":=\" instead"
            ));
            return;
        }
        AssignOp::ClampWarn => {}
    }

    match (param.constraint(), assigned) {
        (Constraint::NumberRange { min, max }, AssignedValue::Number(number)) => {
            let clamped = number.clamp(min, max);
            if clamped != number {
                warnings.push(format!(
                    "scene \"{scene_name}\": {path} sets \"{config_path}\" to {number} via \":=\", out of range {min}-{max} - clamped to {clamped}"
                ));
            }
        }
        (Constraint::NumberRange { .. }, AssignedValue::Text(text)) => {
            errors.push(format!(
                "scene \"{scene_name}\": {path} sets \"{config_path}\" to a string (\"{text}\"), but it requires a number"
            ));
        }
        (Constraint::MaxLength(_), _) => {
            unreachable!(
                "no settable string parameter exists yet - see Constraint::MaxLength's doc comment"
            )
        }
    }
}

/// Adds a warning if the program does not exist or is not executable.
fn check_executable(scene_name: &str, path: &str, executable: &str, warnings: &mut Vec<String>) {
    let resolved = expand_tilde(executable);
    if !Path::new(&resolved).exists() {
        warnings.push(format!(
            "scene \"{scene_name}\": {path} program not found: \"{executable}\""
        ));
    } else if !is_executable(&resolved) {
        warnings.push(format!(
            "scene \"{scene_name}\": {path} program is not executable: \"{executable}\""
        ));
    }
}

/// Adds a warning if the referenced file does not exist.
fn check_file_exists(scene_name: &str, path: &str, file: &str, warnings: &mut Vec<String>) {
    if !Path::new(&expand_tilde(file)).exists() {
        warnings.push(format!(
            "scene \"{scene_name}\": {path} file not found: \"{file}\""
        ));
    }
}

/// Expands a leading `~` (home directory) in a path-like string, mirroring shell tilde
/// expansion: a bare `"~"` becomes `$HOME`, and `"~/rest"` becomes `"$HOME/rest"`.
///
/// Only a literal leading `~` is recognized - no `~user` support, and `~` anywhere but
/// the very start of `value` is left untouched (matching shell behavior, where `~` only
/// expands at the start of a word). Left unchanged (including a leading `~`) when `$HOME`
/// isn't set, or when `value` doesn't start with `~` at all.
pub fn expand_tilde(value: &str) -> String {
    let Some(home) = std::env::var_os("HOME") else {
        return value.to_string();
    };
    if value == "~" {
        return PathBuf::from(home).to_string_lossy().into_owned();
    }
    if let Some(rest) = value.strip_prefix("~/") {
        let mut path = PathBuf::from(home);
        path.push(rest);
        return path.to_string_lossy().into_owned();
    }
    value.to_string()
}

/// Whether the file exists and has at least one execute permission bit set.
///
/// Only implemented for unix (the project's only supported platform family, Linux and
/// FreeBSD): there is no portable execute-permission check to fall back to otherwise.
fn is_executable(path: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|meta| meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Human-readable type name of a JSON value, for error messages.
fn value_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// A program along with its command-line arguments, as referenced by an `*_exec` command.
#[derive(Debug, PartialEq, Clone)]
pub struct CommandSpec {
    /// Path of the program to run.
    pub program: String,
    /// Command-line arguments passed to the program.
    pub args: Vec<String>,
}

impl CommandSpec {
    /// One-line human-readable description of the command, e.g. `"/bin/echo hi"`.
    pub fn display(&self) -> String {
        let mut parts = vec![self.program.clone()];
        parts.extend(self.args.iter().cloned());
        parts.join(" ")
    }
}

/// A single operation derived from a scene's numbered button entries.
#[derive(Debug, Clone, PartialEq)]
pub enum SceneOp {
    /// Load `path` as the image for a button. References are physical buttons (1-based).
    /// `refresh_seconds` (0 = never) re-applies this operation on its own, independent of
    /// any scene switch.
    SetImage {
        reference: Reference,
        path: String,
        refresh_seconds: u64,
    },
    /// Show the first lines of `path` as text on a button. References are physical buttons (1-based).
    /// `refresh_seconds` (0 = never) re-applies this operation on its own, independent of
    /// any scene switch.
    Text {
        reference: Reference,
        path: String,
        refresh_seconds: u64,
    },
    /// Run `command` and show its stdout as text on a button. References are physical buttons (1-based).
    /// `refresh_seconds` (0 = never) re-runs the command on its own, independent of any
    /// scene switch.
    TextExec {
        reference: Reference,
        command: CommandSpec,
        refresh_seconds: u64,
    },
    /// Run `command` and set its stdout (an image file) as the button image. References are physical buttons (1-based).
    /// `refresh_seconds` (0 = never) re-runs the command on its own, independent of any
    /// scene switch.
    ImageExec {
        reference: Reference,
        command: CommandSpec,
        refresh_seconds: u64,
    },
    /// Run `command` detached from this program: own process group, no stdio, and not
    /// killed when the program exits. The decoy reference is only a config slot;
    /// nothing is drawn on it and nothing is restored on termination. References are
    /// physical buttons (1-based).
    Launch {
        reference: Reference,
        command: CommandSpec,
    },
    /// Clear the image of a button. References are physical buttons (1-based).
    Clear { reference: Reference },
    /// Control kind that is not implemented.
    Unsupported { kind: String },
}

/// Builds the ordered list of operations for a scene without touching the device.
///
/// The scene's `setup` dictionary holds the numbered button entries; a scene without a
/// `setup` key yields no operations.
pub fn scene_operations(scene_name: &str, scenes: &Value) -> Result<Vec<SceneOp>, String> {
    let mut operations = Vec::new();

    let scene = scenes
        .get(scene_name)
        .ok_or_else(|| format!("scene \"{scene_name}\" is not defined"))?;
    let object = scene
        .as_object()
        .ok_or_else(|| format!("scene \"{scene_name}\" is not an object"))?;

    let Some(setup) = object.get("setup") else {
        return Ok(operations);
    };
    let map = setup
        .as_object()
        .ok_or_else(|| format!("scene \"{scene_name}\": setup is not an object"))?;

    for (key, value) in map {
        let reference = Reference::parse(key)
            .map_err(|error| format!("scene \"{scene_name}\": key \"{key}\" {error}"))?;

        let object = value.as_object().ok_or_else(|| {
            format!("scene \"{scene_name}\": key \"{key}\" must be an object with \"type\" and \"params\"")
        })?;
        let kind = object
            .get("type")
            .and_then(|value| value.as_str())
            .ok_or_else(|| {
                format!("scene \"{scene_name}\": key \"{key}\" type must be a string")
            })?;
        let params = match object.get("params") {
            Some(value) => value.as_str().ok_or_else(|| {
                format!("scene \"{scene_name}\": key \"{key}\" params must be a string")
            })?,
            None => "",
        }
        .trim()
        .to_string();
        // Validated at config-load time only (check_button_op); trusted here, same as
        // "type"/"params" already are.
        let refresh_seconds = object.get("refresh").and_then(Value::as_u64).unwrap_or(0);

        match kind {
            "image" => operations.push(SceneOp::SetImage {
                reference,
                path: expand_tilde(&params),
                refresh_seconds,
            }),
            "text" => operations.push(SceneOp::Text {
                reference,
                path: expand_tilde(&params),
                refresh_seconds,
            }),
            "text_exec" => {
                let command = params_command(scene_name, key, "text_exec", &params)?;
                operations.push(SceneOp::TextExec {
                    reference,
                    command,
                    refresh_seconds,
                });
            }
            "image_exec" => {
                let command = params_command(scene_name, key, "image_exec", &params)?;
                operations.push(SceneOp::ImageExec {
                    reference,
                    command,
                    refresh_seconds,
                });
            }
            "launch" => {
                let command = params_command(scene_name, key, "launch", &params)?;
                operations.push(SceneOp::Launch { reference, command });
            }
            "clear" => operations.push(SceneOp::Clear { reference }),
            other => operations.push(SceneOp::Unsupported {
                kind: other.to_string(),
            }),
        }
    }

    Ok(operations)
}

/// Spawns `command` fully detached from this program: its own process group (so terminal
/// Ctrl-C / SIGHUP never reach it), null stdio, and the child handle is dropped without
/// waiting or killing — the child keeps running and gets re-parented to the OS init when
/// this program terminates, so it outlives us.
///
/// Only implemented for unix (the project's only supported platform family, Linux and
/// FreeBSD): `process_group` is a unix-only `Command` extension.
pub fn spawn_detached(command: &CommandSpec, log: Log) {
    let display = command.display();
    let result = StdCommand::new(&command.program)
        .args(&command.args)
        .process_group(0)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    match result {
        Ok(child) => {
            // Dropping the handle detaches the child from us: it runs on its own,
            // reparented to init, and is never killed when this program exits.
            let pid = child.id();
            drop(child);
            log.debug(
                Subsystem::Actions,
                format!("launched detached \"{display}\" (pid {pid})"),
            );
        }
        Err(error) => log.error(format!("\"{display}\" failed to start detached: {error}")),
    }
}

/// Parses an `*_exec`/`image_exec`/`text_exec` command specification from the collapsed
/// `params` command line. The `scene_name`, physical `key`, and `kind` label are only used
/// for error messages. An empty `params` is an error.
fn params_command(
    scene_name: &str,
    key: &str,
    kind: &str,
    params: &str,
) -> Result<CommandSpec, String> {
    if params.is_empty() {
        return Err(format!(
            "scene \"{scene_name}\": key \"{key}\": {kind} params must be a program command line"
        ));
    }
    parse_command_line(params)
        .map_err(|error| format!("scene \"{scene_name}\": key \"{key}\": {error}"))
}

/// Splits a whitespace-separated command line into a program and its arguments.
///
/// `image_exec`/`text_exec`/`launch` config entries and `Action::Command` values keep
/// the whole command in a single string; this tokenizer turns it back into the
/// [`CommandSpec`] that is run with tokio's process API. Single-quoted and
/// double-quoted segments are kept as one argument (quotes removed) and a backslash
/// escapes the following character outside quotes. An unterminated quote or an empty
/// line is reported as an error. A leading `~` in the program name or in any argument
/// is expanded to `$HOME` via [`expand_tilde`], so `~/scripts/foo.sh --config
/// ~/my.json` resolves both paths.
pub fn parse_command_line(params: &str) -> Result<CommandSpec, String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut chars = params.chars();
    let mut quote: Option<char> = None;

    while let Some(c) = chars.next() {
        if let Some(open) = quote {
            if c == open {
                quote = None;
            } else {
                current.push(c);
            }
            continue;
        }
        match c {
            ' ' | '\t' => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
            }
            '"' | '\'' => quote = Some(c),
            '\\' => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            _ => current.push(c),
        }
    }
    if quote.is_some() {
        return Err(format!("unbalanced quotes in \"{params}\""));
    }
    if !current.is_empty() {
        words.push(current);
    }
    if words.is_empty() {
        return Err(format!("empty command in \"{params}\""));
    }

    let mut words = words.into_iter();
    Ok(CommandSpec {
        program: expand_tilde(&words.next().unwrap()),
        args: words.map(|arg| expand_tilde(&arg)).collect(),
    })
}

/// How long an async `text_exec`/`image_exec` program may run before it is killed.
pub const EXEC_TIMEOUT: Duration = Duration::from_secs(5);

/// Minimal device surface used by scene operations, abstracted so the runner can be
/// exercised with a mock instead of a physical keypad.
///
/// Keys are mirajazz 0-based button indices. Images are only staged by
/// [`ButtonDevice::set_button_image`]; they reach the LCDs once [`ButtonDevice::flush`]
/// succeeds, so every scene application flushes at the end.
///
/// The `Send + Sync` supertrait guarantees the futures returned by the async trait
/// methods are combinable in the `main` select loop, so the trait is implemented with
/// async functions straight away.
#[allow(async_fn_in_trait)]
pub trait ButtonDevice: Send + Sync {
    /// Error type reported by device operations.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Stages `image` for `key`, to be blitted on the next [`ButtonDevice::flush`].
    async fn set_button_image(
        &self,
        key: u8,
        image_format: ImageFormat,
        image: DynamicImage,
    ) -> Result<(), Self::Error>;

    /// Stages the "empty" image for `key`.
    async fn clear_button_image(&self, key: u8) -> Result<(), Self::Error>;

    /// Sends all staged images to the device's LCDs.
    async fn flush(&self) -> Result<(), Self::Error>;

    /// Number of buttons the device physically has.
    fn key_count(&self) -> u8;

    /// Sets the device's button/screen LCD brightness (0-100 percent).
    async fn set_brightness(&self, percent: u8) -> Result<(), Self::Error>;

    /// Sets the device's encoder LED-ring brightness (0-100 percent).
    async fn set_led_brightness(&self, percent: u8) -> Result<(), Self::Error>;
}

impl ButtonDevice for Device {
    type Error = MirajazzError;

    async fn set_button_image(
        &self,
        key: u8,
        image_format: ImageFormat,
        image: DynamicImage,
    ) -> Result<(), Self::Error> {
        Device::set_button_image(self, key, image_format, image).await
    }

    async fn clear_button_image(&self, key: u8) -> Result<(), Self::Error> {
        Device::clear_button_image(self, key).await
    }

    async fn flush(&self) -> Result<(), Self::Error> {
        Device::flush(self).await
    }

    fn key_count(&self) -> u8 {
        Device::key_count(self) as u8
    }

    async fn set_brightness(&self, percent: u8) -> Result<(), Self::Error> {
        Device::set_brightness(self, percent).await
    }

    async fn set_led_brightness(&self, percent: u8) -> Result<(), Self::Error> {
        Device::set_led_brightness(self, percent).await
    }
}

/// Runs scene operations against one device, managing asynchronous `image_exec` and
/// `text_exec` tasks.
///
/// Async tasks never touch the device directly; they report their outcome through a
/// channel, and [`SceneRunner::handle_exec_event`] applies the result to the button.
/// A `text_exec`/`image_exec` is cancelled (process killed, red "Error" drawn) when the
/// same button is reassigned by a later scene operation, and cancelled on its own
/// after [`EXEC_TIMEOUT`].
pub struct SceneRunner<'a, D: ButtonDevice> {
    device: &'a D,
    image_format: ImageFormat,
    tracker: ExecTracker,
    /// Output filter: debug lines for scene/device events only print when enabled.
    log: Log,
    /// Config number of the device this runner drives. Operations targeting another
    /// device's number are detected and skipped with a notice.
    device_number: u8,
    /// Physical button numbers (1-based) that this runner applied any image operation to
    /// during the session. Termination cleanup clears exactly these buttons, so buttons the
    /// program never touched are left alone.
    changed_keys: std::collections::HashSet<u8>,
    /// Physical button numbers (1-based) that have no display on this device. Image
    /// operations targetting them are skipped with a warning: the hardware ignores them.
    screenless_buttons: std::collections::HashSet<u8>,
    /// The operation currently active on each physical button (1-based), i.e. whatever
    /// the most recent explicit scene entry applied there. Consulted by
    /// [`SceneRunner::refresh_button`] to redraw just that button on its own schedule,
    /// independent of whichever scene happens to be active - a button not redefined by
    /// a later scene keeps both its content and its refresh schedule, exactly like an
    /// inherited action binding.
    active_setup: std::collections::HashMap<u8, SceneOp>,
    /// The pending "next tick" task for each physical button (1-based) with a nonzero
    /// `refresh_seconds`, if any. Replaced (old task aborted) every time that button is
    /// explicitly re-applied, whether by its own tick or by a scene redefining it.
    refresh_handles: std::collections::HashMap<u8, tokio::task::JoinHandle<()>>,
    /// Sender for refresh ticks: a spawned task sleeps for a button's `refresh_seconds`
    /// then sends its key here; the receiving end drives [`SceneRunner::refresh_button`].
    refresh_tx: mpsc::Sender<u8>,
}

impl<'a, D: ButtonDevice> SceneRunner<'a, D> {
    /// Creates a runner driving the config device `device_number` bound to `device`,
    /// sending `exec` results through `exec_tx`, refresh ticks through `refresh_tx`,
    /// reporting scene/device events through `log`.
    ///
    /// `screenless_buttons` lists the device buttons that have no display; image
    /// assignment to them is skipped with a warning (see [`SceneRunner`]).
    pub fn new(
        device_number: u8,
        device: &'a D,
        image_format: ImageFormat,
        exec_tx: mpsc::Sender<ExecEvent>,
        refresh_tx: mpsc::Sender<u8>,
        log: Log,
        screenless_buttons: &std::collections::HashSet<u8>,
    ) -> Self {
        Self {
            device,
            image_format,
            tracker: ExecTracker::new(exec_tx),
            log,
            device_number,
            changed_keys: std::collections::HashSet::new(),
            screenless_buttons: screenless_buttons.clone(),
            active_setup: std::collections::HashMap::new(),
            refresh_handles: std::collections::HashMap::new(),
            refresh_tx,
        }
    }

    /// Applies the numbered button operations of `scene_name` to the device.
    pub async fn enter_scene(
        &mut self,
        scene_name: &str,
        scenes: &Value,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.log
            .debug(Subsystem::Scene, format!("Entering scene \"{scene_name}\""));
        let operations = scene_operations(scene_name, scenes)?;
        self.apply_scene_operations(&operations).await
    }

    /// Sets this device's button/screen LCD brightness (0-100 percent), e.g. from a
    /// `$defaults.button_brightness := N` action. Takes effect immediately; not
    /// persisted anywhere (config.json is untouched) and not reapplied by a later
    /// `Stay`/scene switch, which never touch brightness.
    pub async fn set_button_brightness(&self, percent: u8) -> Result<(), D::Error> {
        self.device.set_brightness(percent).await
    }

    /// Sets this device's encoder LED-ring brightness (0-100 percent), e.g. from a
    /// `$defaults.encoder_brightness := N` action. Same immediate, non-persisted
    /// semantics as [`SceneRunner::set_button_brightness`].
    pub async fn set_encoder_brightness(&self, percent: u8) -> Result<(), D::Error> {
        self.device.set_led_brightness(percent).await
    }

    /// Applies scene operations to the device; unsupported operations are skipped with a notice.
    ///
    /// Operations targeting another device's number, encoder references (not implemented
    /// yet), buttons beyond the device's physical count, or buttons without a display are
    /// reported and skipped. Before touching a button, any still-running `exec` task for
    /// that button is cancelled: its process is killed, a red "Error" is drawn, and the
    /// failure is logged. Async results arriving later for the old generation are
    /// discarded.
    pub async fn apply_scene_operations(
        &mut self,
        operations: &[SceneOp],
    ) -> Result<(), Box<dyn std::error::Error>> {
        for operation in operations {
            self.apply_one_operation(operation).await;
        }
        // set_button_image only stages images in the write cache, so every application
        // of a scene must flush for the staged images to reach the device's LCDs.
        self.device.flush().await?;
        Ok(())
    }

    /// Re-applies whatever operation is currently active on `key` - i.e. whatever the
    /// most recent explicit scene entry drew there, tracked in `active_setup` - without
    /// touching any other button, then flushes so the redraw reaches the LCD.
    ///
    /// This is how a nonzero `refresh_seconds` keeps a button updating on its own
    /// schedule, independent of whichever scene happens to be active:
    /// [`SceneRunner::apply_one_operation`] re-arms the next tick as a side effect of
    /// applying the operation, exactly as it does the first time the operation is
    /// applied, so no special-casing is needed between "just entered the scene" and
    /// "a scheduled refresh tick fired".
    ///
    /// Does nothing if `key` has no active operation - a stale tick arriving after the
    /// button was reassigned to something without a refresh (or reassigned since the
    /// tick was scheduled, and the new operation's own tick already superseded it, since
    /// `apply_one_operation` always cancels the previous handle before scheduling a new
    /// one).
    pub async fn refresh_button(&mut self, key: u8) -> Result<(), Box<dyn std::error::Error>> {
        let Some(operation) = self.active_setup.get(&key).cloned() else {
            return Ok(());
        };
        self.apply_one_operation(&operation).await;
        self.device.flush().await?;
        Ok(())
    }

    /// Applies one scene operation to the device: the shared per-operation logic behind
    /// both [`SceneRunner::apply_scene_operations`] (a whole scene's worth, batched, one
    /// flush at the end) and [`SceneRunner::refresh_button`] (a single button, on its own
    /// schedule, flushing immediately).
    ///
    /// See [`SceneRunner::apply_scene_operations`]'s doc comment for the validity checks
    /// performed (device/encoder/range/screenless skips). On success, records the
    /// operation as `key`'s active setup and, for operations with a nonzero
    /// `refresh_seconds`, schedules the next tick - replacing (aborting) any tick already
    /// scheduled for that button first, whether or not the operation actually changed.
    ///
    /// Never fails: every content-acquisition/processing failure (a missing/unreadable
    /// file, an undecodable image, unrenderable text) is logged and draws the red
    /// "Error" label on that button instead of propagating, so one bad operation never
    /// stops the rest of the batch from being applied and flushed. A failure writing to
    /// the device itself (as opposed to preparing the content to write) is logged only -
    /// attempting to also draw an "Error" label would use the same failing device call
    /// and likely just fail too.
    async fn apply_one_operation(&mut self, operation: &SceneOp) {
        // Unsupported carries no reference, so it is skipped before any per-reference
        // filtering can apply.
        let Some(reference) = operation_reference(operation) else {
            let SceneOp::Unsupported { kind } = operation else {
                unreachable!("operation without a reference must be Unsupported");
            };
            self.log.warn(format!(
                "scene setup \"{kind}\" on key is not supported and was skipped"
            ));
            return;
        };
        let reference = *reference;

        if reference.device != self.device_number {
            self.log.warn(format!(
                "device {} is referenced but not present; skipping operation: {operation:?}",
                reference.device
            ));
            return;
        }
        if reference.kind == Kind::Encoder {
            self.log.warn(format!(
                "{kind} {reference} is not supported yet; skipping operation: {operation:?}",
                kind = reference.kind.label()
            ));
            return;
        }

        // A launch only runs its command; its reference is a config slot, so no
        // button is drawn and none is restored on termination.
        if let SceneOp::Launch { command, .. } = operation {
            spawn_detached(command, self.log);
            return;
        }

        let key = reference.number;
        if key > self.device.key_count() {
            self.log.warn(format!(
                "button {reference} is out of range (device has {} buttons); skipping operation: {operation:?}",
                self.device.key_count()
            ));
            return;
        }

        // A button without a display cannot show any image: assignment is pointless
        // and the hardware ignores the transfer, so warn and skip the work.
        if self.screenless_buttons.contains(&key)
            && matches!(
                operation,
                SceneOp::SetImage { .. }
                    | SceneOp::Text { .. }
                    | SceneOp::TextExec { .. }
                    | SceneOp::ImageExec { .. }
            )
        {
            self.log.warn(format!(
                "button {reference} has no display; skipping operation: {operation:?}"
            ));
            return;
        }

        // record the button as "touched" so termination cleanup can restore exactly
        // the buttons this session changed (unused buttons are left alone)
        self.changed_keys.insert(key);
        self.cancel_exec_if_running(key).await;
        // Any refresh previously scheduled for this button belonged to whatever
        // operation was active before; it is unconditionally replaced below by whatever
        // this operation schedules (if anything), even if the operation is unchanged.
        self.cancel_refresh(key);
        match operation {
            // config references are physical buttons numbered from 1; mirajazz keys are 0-based
            SceneOp::SetImage {
                reference: _, path, ..
            } => {
                self.log.debug(
                    Subsystem::Scene,
                    format!("set image from \"{path}\" on key {key}"),
                );
                // Errors are stringified immediately, before any match/await: the
                // concrete error types here are not guaranteed `Send`, and
                // `apply_one_operation`'s future is spawned onto the runtime (via
                // `run_device` in `main.rs`), which requires it to be. An owned
                // `String` has no such restriction.
                match load_image_file(path)
                    .await
                    .map_err(|error| error.to_string())
                {
                    Ok(image) => {
                        if let Err(error) = self
                            .device
                            .set_button_image(key.saturating_sub(1), self.image_format, image)
                            .await
                        {
                            self.log.error(format!(
                                "button {key}: failed to draw image from \"{path}\": {error}"
                            ));
                        } else {
                            self.log
                                .debug(Subsystem::Device, format!("set image on button {key}"));
                        }
                    }
                    Err(message) => {
                        self.fail_button(
                            key,
                            format!(
                                "button {key}: failed to load image from \"{path}\": {message}"
                            ),
                        )
                        .await;
                    }
                }
            }
            SceneOp::Text {
                reference: _, path, ..
            } => {
                self.log.debug(
                    Subsystem::Scene,
                    format!("render text from \"{path}\" on key {key}"),
                );
                // See the `SetImage` branch above: errors are stringified immediately.
                let text_result = read_text_file_bounded(path)
                    .await
                    .map_err(|error| error.to_string());
                match text_result {
                    Ok(content) => {
                        let render_result = crate::text::render_text(
                            &crate::text::button_text(&content),
                            self.image_format,
                        )
                        .map_err(|error| error.to_string());
                        match render_result {
                            Ok(image) => {
                                if let Err(error) = self
                                    .device
                                    .set_button_image(
                                        key.saturating_sub(1),
                                        self.image_format,
                                        image,
                                    )
                                    .await
                                {
                                    self.log.error(format!(
                                        "button {key}: failed to draw text from \"{path}\": {error}"
                                    ));
                                } else {
                                    self.log.debug(
                                        Subsystem::Device,
                                        format!("set image on button {key} from text"),
                                    );
                                }
                            }
                            Err(message) => {
                                self.fail_button(
                                    key,
                                    format!(
                                        "button {key}: failed to render text from \"{path}\": {message}"
                                    ),
                                )
                                .await;
                            }
                        }
                    }
                    Err(message) => {
                        self.fail_button(
                            key,
                            format!("button {key}: failed to read text from \"{path}\": {message}"),
                        )
                        .await;
                    }
                }
            }
            SceneOp::TextExec {
                reference: _,
                command,
                ..
            } => {
                self.log.debug(
                    Subsystem::Scene,
                    format!("start text exec \"{}\" on key {key}", command.display()),
                );
                self.start_exec_task(key, ExecOutputKind::Text, command);
            }
            SceneOp::ImageExec {
                reference: _,
                command,
                ..
            } => {
                self.log.debug(
                    Subsystem::Scene,
                    format!("start image exec \"{}\" on key {key}", command.display()),
                );
                self.start_exec_task(key, ExecOutputKind::Image, command);
            }
            SceneOp::Launch { .. } => {
                unreachable!("Launch is handled before the per-button match")
            }
            SceneOp::Clear { reference: _ } => {
                self.log.debug(Subsystem::Scene, format!("clear key {key}"));
                if let Err(error) = self.device.clear_button_image(key.saturating_sub(1)).await {
                    self.log
                        .error(format!("button {key}: failed to clear: {error}"));
                } else {
                    self.log
                        .debug(Subsystem::Device, format!("clear image on button {key}"));
                }
            }
            SceneOp::Unsupported { .. } => {
                unreachable!("Unsupported operations are skipped before the match")
            }
        }

        self.active_setup.insert(key, operation.clone());
        let refresh_seconds = refresh_seconds_of(operation);
        if refresh_seconds > 0 {
            self.schedule_refresh(key, refresh_seconds);
        }
    }

    /// Aborts and forgets `key`'s pending refresh tick, if any.
    fn cancel_refresh(&mut self, key: u8) {
        if let Some(handle) = self.refresh_handles.remove(&key) {
            handle.abort();
        }
    }

    /// Schedules a one-shot tick for `key` after `seconds`, replacing (see
    /// [`SceneRunner::cancel_refresh`], always called by the only caller,
    /// [`SceneRunner::apply_one_operation`], before this) any tick already pending for
    /// it.
    ///
    /// Mirrors the scene-level timer's own one-shot-respawn style
    /// (`arm_scene_timer`/`rearm_scene_timer` in `main.rs`) rather than a repeating
    /// `tokio::interval`: each tick, once handled, schedules the next one itself.
    fn schedule_refresh(&mut self, key: u8, seconds: u64) {
        let tx = self.refresh_tx.clone();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(seconds)).await;
            let _ = tx.send(key).await;
        });
        self.refresh_handles.insert(key, handle);
    }

    /// Restores every button the runner changed during this session to its original
    /// state by clearing its image, then flushes so the clear reaches the LCDs.
    ///
    /// This is the termination cleanup called right before the program shuts the device
    /// down: only buttons this session actually wrote to are touched, so buttons the
    /// program never changed are left exactly as they were.
    pub async fn clear_changed_button_images(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let mut keys: Vec<_> = self.changed_keys.iter().copied().collect();
        keys.sort_unstable();
        for key in keys {
            self.log.debug(
                Subsystem::Device,
                format!("clear image on button {key} during cleanup"),
            );
            self.device
                .clear_button_image(key.saturating_sub(1))
                .await?;
        }
        self.device.flush().await?;
        Ok(())
    }

    /// Bumps the button's generation and spawns an `exec` task for `command`.
    ///
    /// The output (or the red "Error") lands on the button asynchronously through
    /// [`ExecEvent`]s handled by [`SceneRunner::handle_exec_event`].
    fn start_exec_task(&mut self, key: u8, kind: ExecOutputKind, command: &CommandSpec) {
        let generation = self.tracker.bump(key);
        let exec_tx = self.tracker.sender();
        let handle = spawn_exec(exec_tx, key, kind, command.clone(), generation);
        self.tracker.insert(key, PendingExec { handle, generation });
    }

    /// Draws the result of a finished `exec` task on its button: `text_exec` output is
    /// rendered as text, `image_exec` output is decoded as an image file.
    ///
    /// Results tagged with a generation older than the button's current one are stale
    /// (the button was reassigned while the program was running) and are discarded.
    pub async fn handle_exec_event(&mut self, event: ExecEvent) {
        match event {
            ExecEvent::Output {
                key,
                generation,
                kind,
                stdout,
            } => {
                if !self.tracker.is_current(key, generation) {
                    return;
                }
                let image = match kind {
                    ExecOutputKind::Text => {
                        let text = match String::from_utf8(stdout) {
                            Ok(text) => text,
                            Err(error) => {
                                self.log
                                    .error(format!("button {key}: non-UTF-8 output: {error}"));
                                if let Err(error) = self.draw_error_label(key).await {
                                    self.log.error(format!(
                                        "button {key}: failed to draw error label: {error}"
                                    ));
                                }
                                return;
                            }
                        };
                        match crate::text::render_text(
                            &crate::text::button_text(&text),
                            self.image_format,
                        ) {
                            Ok(image) => image,
                            Err(error) => {
                                self.log.error(format!(
                                    "button {key}: failed to render output: {error}"
                                ));
                                return;
                            }
                        }
                    }
                    ExecOutputKind::Image => match image::load_from_memory(&stdout) {
                        Ok(image) => image,
                        Err(error) => {
                            self.log.error(format!(
                                "button {key}: output is not a valid image: {error}"
                            ));
                            if let Err(error) = self.draw_error_label(key).await {
                                self.log.error(format!(
                                    "button {key}: failed to draw error label: {error}"
                                ));
                            }
                            return;
                        }
                    },
                };
                if let Err(error) = self
                    .device
                    .set_button_image(key.saturating_sub(1), self.image_format, image)
                    .await
                {
                    self.log
                        .error(format!("button {key}: failed to draw output: {error}"));
                    return;
                }
                self.log.debug(
                    Subsystem::Device,
                    format!("set image on button {key} from exec output"),
                );
                if let Err(error) = self.device.flush().await {
                    self.log
                        .error(format!("button {key}: failed to flush output: {error}"));
                }
            }
            ExecEvent::Error {
                key,
                generation,
                error,
            } => {
                if !self.tracker.is_current(key, generation) {
                    return;
                }
                self.log.error(format!("button {key}: {error}"));
                if let Err(error) = self.draw_error_label(key).await {
                    self.log
                        .error(format!("button {key}: failed to draw error label: {error}"));
                }
            }
        }
    }

    /// Kills the `exec` process running for `key` (if any), logs it and draws a red
    /// "Error" label, because a new scene operation owns the button from now on.
    ///
    /// Tasks whose program already finished are only dereferenced: nothing running needs
    /// to be killed, so no error is drawn for them.
    async fn cancel_exec_if_running(&mut self, key: u8) {
        let Some(pending) = self.tracker.cancel(key) else {
            return;
        };
        if pending.handle.is_finished() {
            return;
        }
        pending.handle.abort();
        self.log.error(format!(
            "button {key}: killed running program because its content changed"
        ));
        if let Err(error) = self.draw_error_label(key).await {
            self.log
                .error(format!("button {key}: failed to draw error label: {error}"));
        }
    }

    /// Renders the red "Error" label on `key`.
    async fn draw_error_label(&self, key: u8) -> Result<(), Box<dyn std::error::Error>> {
        let image = crate::text::render_error_image(self.image_format)?;
        self.device
            .set_button_image(key.saturating_sub(1), self.image_format, image)
            .await?;
        self.log.debug(
            Subsystem::Device,
            format!("set error label on button {key}"),
        );
        self.device.flush().await?;
        Ok(())
    }

    /// Logs `message` (a content-acquisition/processing failure already rendered into
    /// an owned `String`) and draws the red "Error" label on `key`.
    ///
    /// Callers must format any foreign error into `message` *before* calling this, not
    /// hold it across this call's own `await`: most error types this crate deals with
    /// (`image::ImageError`, `Box<dyn std::error::Error>` from `crate::text`) are not
    /// `Send`, and `apply_one_operation`'s future is spawned onto the runtime (via
    /// `run_device` in `main.rs`), which requires it - and does require - to be `Send`.
    /// An owned `String` has no such restriction.
    async fn fail_button(&mut self, key: u8, message: String) {
        self.log.error(message);
        if let Err(error) = self.draw_error_label(key).await {
            self.log
                .error(format!("button {key}: failed to draw error label: {error}"));
        }
    }
}

/// Returns the control reference an operation targets, if any.
///
/// Every operation carries a [Reference], including `Launch` whose reference is only a
/// config slot; only `Unsupported` has no reference at all.
fn operation_reference(operation: &SceneOp) -> Option<&Reference> {
    match operation {
        SceneOp::SetImage { reference, .. }
        | SceneOp::Text { reference, .. }
        | SceneOp::TextExec { reference, .. }
        | SceneOp::ImageExec { reference, .. }
        | SceneOp::Launch { reference, .. }
        | SceneOp::Clear { reference } => Some(reference),
        SceneOp::Unsupported { .. } => None,
    }
}

/// The `refresh_seconds` an operation carries, or 0 for the variants that never have one
/// (`Launch`/`Clear`/`Unsupported`) - 0 means "never", the same value an absent `refresh`
/// field defaults to, so callers can treat both cases identically.
fn refresh_seconds_of(operation: &SceneOp) -> u64 {
    match operation {
        SceneOp::SetImage {
            refresh_seconds, ..
        }
        | SceneOp::Text {
            refresh_seconds, ..
        }
        | SceneOp::TextExec {
            refresh_seconds, ..
        }
        | SceneOp::ImageExec {
            refresh_seconds, ..
        } => *refresh_seconds,
        SceneOp::Launch { .. } | SceneOp::Clear { .. } | SceneOp::Unsupported { .. } => 0,
    }
}

/// Tracks the asynchronous `exec` tasks per button and their cancellation generations.
///
/// Each button has a generation counter that only grows. When a button is reassigned the
/// generation is bumped, so results sent by the still-running task are recognised as stale
/// and dropped instead of overwriting the button's new content.
#[derive(Debug)]
pub struct ExecTracker {
    exec_tx: mpsc::Sender<ExecEvent>,
    pending: HashMap<u8, PendingExec>,
    generations: HashMap<u8, u64>,
}

impl ExecTracker {
    /// Creates a tracker delivering results through `exec_tx`.
    pub fn new(exec_tx: mpsc::Sender<ExecEvent>) -> Self {
        Self {
            exec_tx,
            pending: HashMap::new(),
            generations: HashMap::new(),
        }
    }

    /// The channel tasks send their outcomes through.
    pub fn sender(&self) -> mpsc::Sender<ExecEvent> {
        self.exec_tx.clone()
    }

    /// Invalidates any in-flight task of `key` by advancing its generation.
    pub fn bump(&mut self, key: u8) -> u64 {
        let entry = self.generations.entry(key).or_insert(0);
        *entry += 1;
        *entry
    }

    /// Removes and returns the task registered for `key`, invalidating it first.
    pub fn cancel(&mut self, key: u8) -> Option<PendingExec> {
        self.bump(key);
        self.pending.remove(&key)
    }

    /// Whether an event tagged `generation` is still valid for `key`.
    pub fn is_current(&self, key: u8, generation: u64) -> bool {
        self.generations.get(&key).copied() == Some(generation)
    }

    /// Registers a just-spawned task for `key`.
    pub fn insert(&mut self, key: u8, pending: PendingExec) {
        self.pending.insert(key, pending);
    }
}

/// A single running `exec` task for a button.
#[derive(Debug)]
pub struct PendingExec {
    /// Task handle; aborting it drops the child process (killing it).
    pub handle: tokio::task::JoinHandle<()>,
    /// Generation the task was spawned with, matched against the tracker's current value.
    pub generation: u64,
}

/// What a successful `image_exec`/`text_exec` program's stdout should be treated as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecOutputKind {
    /// Render stdout as text on the button.
    Text,
    /// Decode stdout as an image file and set it as the button image.
    Image,
}

/// Outcome reported by a spawned `exec` task once its program finished or failed.
#[derive(Debug, PartialEq)]
pub enum ExecEvent {
    /// The program finished successfully; `stdout` is its complete output.
    Output {
        /// 1-based button the output belongs to.
        key: u8,
        /// Tracker generation the task was spawned with.
        generation: u64,
        /// Whether the output is text or image data.
        kind: ExecOutputKind,
        /// The program's raw stdout.
        stdout: Vec<u8>,
    },
    /// The program failed, timed out, or was killed while running.
    Error {
        /// 1-based button the error belongs to.
        key: u8,
        /// Tracker generation the task was spawned with.
        generation: u64,
        /// Human-readable description of what went wrong.
        error: String,
    },
}

/// Spawns an asynchronous task that runs `command`, enforcing [`EXEC_TIMEOUT`].
///
/// The program's output (or an error description) is sent through `exec_tx` as an
/// [`ExecEvent`] tagged with `kind`, telling the runner whether stdout is text or an
/// image. The process is killed via `kill_on_drop` if the task is aborted (button
/// reassigned) or if it runs longer than the timeout.
pub fn spawn_exec(
    exec_tx: mpsc::Sender<ExecEvent>,
    key: u8,
    kind: ExecOutputKind,
    command: CommandSpec,
    generation: u64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let event = match run_command_with_timeout(&command, EXEC_TIMEOUT).await {
            Ok(stdout) => ExecEvent::Output {
                key,
                generation,
                kind,
                stdout,
            },
            Err(error) => ExecEvent::Error {
                key,
                generation,
                error,
            },
        };
        let _ = exec_tx.send(event).await;
    })
}

/// Maximum bytes captured from a `text_exec`/`image_exec` program's stdout - generous
/// for a 60x60 button image or a few lines of text, but a hard bound: without one, a
/// program producing more than the OS pipe buffer's worth of output would block on its
/// own `write()` for the entire timeout every time (see [`run_command_with_timeout`]).
pub const MAX_EXEC_OUTPUT_BYTES: u64 = 10 * 1024 * 1024;

/// Runs `command` async, killing it if it does not finish within `timeout`.
///
/// Returns the program's raw stdout on success or a description of the failure (spawn
/// error, non-zero exit, timeout, or output past [`MAX_EXEC_OUTPUT_BYTES`]) otherwise.
/// Callers interpret the bytes: `text_exec` renders them as text, `image_exec` decodes
/// them as an image file.
pub async fn run_command_with_timeout(
    command: &CommandSpec,
    timeout: Duration,
) -> Result<Vec<u8>, String> {
    use tokio::io::AsyncReadExt;

    let display = command.display();
    let mut child = tokio::process::Command::new(&command.program)
        .args(&command.args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("command \"{display}\" failed to start: {error}"))?;
    let mut stdout = child.stdout.take().expect("stdout pipe was requested");

    // Drain stdout *before* waiting for exit, not after: a program producing more than
    // one OS pipe buffer's worth of output blocks on its own `write()` once that buffer
    // fills, since nothing reads it until later - waiting for exit first would then
    // never see the child actually exit (it can't, still blocked mid-write) until this
    // whole function's caller-supplied timeout kills it, even for output that would
    // otherwise finish in an instant. Reading one byte past the cap (rather than
    // exactly at it) tells a genuine overflow apart from output that happens to be
    // exactly the cap size and then legitimately ends.
    let run = async {
        let mut bytes = Vec::new();
        (&mut stdout)
            .take(MAX_EXEC_OUTPUT_BYTES + 1)
            .read_to_end(&mut bytes)
            .await
            .map_err(|error| format!("command \"{display}\" failed to read stdout: {error}"))?;
        if bytes.len() as u64 > MAX_EXEC_OUTPUT_BYTES {
            return Err(format!(
                "command \"{display}\" output exceeded {MAX_EXEC_OUTPUT_BYTES} bytes"
            ));
        }
        let status = child
            .wait()
            .await
            .map_err(|error| format!("command \"{display}\" failed to wait: {error}"))?;
        Ok((status, bytes))
    };

    let (status, bytes) = match tokio::time::timeout(timeout, run).await {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(error);
        }
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(format!(
                "command \"{display}\" was killed after running longer than {timeout:?}"
            ));
        }
    };

    if !status.success() {
        return Err(format!("command \"{display}\" exited with {status}"));
    }
    Ok(bytes)
}

/// Runs an action command (`Action::Command`) to completion in the background.
///
/// Unlike [`run_command_with_timeout`] this imposes no timeout and captures no output:
/// the child inherits this process's stdio and runs until it exits on its own, so
/// long-running programs (e.g. playing a sound) are neither killed nor awaited inline
/// by the device input loop. Callers spawn the returned future on the runtime.
pub async fn run_action_command(command: CommandSpec) -> Result<(), String> {
    let display = command.display();
    let status = tokio::process::Command::new(&command.program)
        .args(&command.args)
        .status()
        .await
        .map_err(|error| format!("command \"{display}\" failed to start: {error}"))?;
    if !status.success() {
        return Err(format!("command \"{display}\" exited with {status}"));
    }
    Ok(())
}

/// Maximum bytes read from a `text` setup entry's file - far more than the 3x6
/// characters ever shown on a button, but a hard bound: without one, a huge or
/// infinite source (e.g. `/dev/zero`) would be read until EOF, which such a source
/// never reaches, growing memory without limit instead of ever finishing.
pub const MAX_TEXT_FILE_BYTES: u64 = 64 * 1024;

/// Loads the image at `path` off the async runtime's worker thread: `image::open` does
/// blocking file I/O and (for a large or complex image) CPU-bound decoding, neither of
/// which should run directly on a tokio worker thread.
async fn load_image_file(
    path: &str,
) -> Result<DynamicImage, Box<dyn std::error::Error + Send + Sync>> {
    let path = path.to_string();
    let result = tokio::task::spawn_blocking(move || image::open(path))
        .await
        .map_err(|error| -> Box<dyn std::error::Error + Send + Sync> { Box::new(error) })?;
    result.map_err(|error| -> Box<dyn std::error::Error + Send + Sync> { Box::new(error) })
}

/// Reads at most [`MAX_TEXT_FILE_BYTES`] from `path`, using `tokio::fs` so a large or
/// slow read never blocks a worker thread; a source with more data than the cap (e.g.
/// `/dev/zero`) simply stops there instead of reading forever. Bytes are decoded
/// lossily, since a `text` entry only ever shows the first few lines, so invalid UTF-8
/// anywhere in a large file is not worth failing the whole read over.
async fn read_text_file_bounded(
    path: &str,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    use tokio::io::AsyncReadExt;
    let file = tokio::fs::File::open(path).await?;
    let mut buf = Vec::new();
    file.take(MAX_TEXT_FILE_BYTES).read_to_end(&mut buf).await?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Loads the image at `path` and sets it on the given device key (mirajazz 0-based).
pub async fn set_image_from_file<D: ButtonDevice>(
    device: &D,
    key: u8,
    image_format: ImageFormat,
    path: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let image = load_image_file(path)
        .await
        .map_err(|error| -> Box<dyn std::error::Error> { error })?;
    device.set_button_image(key, image_format, image).await?;
    Ok(())
}

/// A config `defaults` field settable at runtime via a `$path := value` action.
///
/// Currently the only two settable parameters; everything else in the config (all of
/// `devices`/`scenes`, the rest of `defaults`, and `version`) is read-only - see
/// [`classify_config_path`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettableDefault {
    ButtonBrightness,
    EncoderBrightness,
}

impl SettableDefault {
    /// The dotted config path a `$`-action names this parameter by, e.g.
    /// `"defaults.button_brightness"`.
    pub fn path(&self) -> &'static str {
        match self {
            SettableDefault::ButtonBrightness => "defaults.button_brightness",
            SettableDefault::EncoderBrightness => "defaults.encoder_brightness",
        }
    }

    /// The value constraint `:=`/`~=` clamp or truncate into (and `=` will hard-error
    /// against, once implemented). Both of today's settable parameters are numeric
    /// brightness percentages, matching `mirajazz::Device::set_brightness`/
    /// `set_led_brightness`'s own internal `percent.clamp(0, 100)`.
    fn constraint(&self) -> Constraint {
        match self {
            SettableDefault::ButtonBrightness | SettableDefault::EncoderBrightness => {
                Constraint::NumberRange { min: 0, max: 100 }
            }
        }
    }
}

/// The value constraint a settable parameter's assignment operator checks against -
/// what `:=`/`~=` clamp or truncate into, and what `=` will hard-error against once
/// implemented (see [`AssignOp`]).
enum Constraint {
    /// A number must fall within `min..=max` (inclusive on both ends).
    NumberRange { min: i64, max: i64 },
    /// A string must be at most `max_len` characters. Reserved for a future
    /// string-typed settable parameter; [`SettableDefault::constraint`] never returns
    /// this today, since both current settable parameters are numeric.
    #[allow(dead_code)]
    MaxLength(usize),
}

/// The right-hand-side literal of a `$path <op> value` action, before it's matched
/// against the type a specific [`SettableDefault`] expects.
///
/// Kept distinct from [`AssignedValue::Number`] on purpose: a quoted `"123"` is text,
/// not the number `123`, so a settable parameter that requires a number rejects a
/// quoted numeral instead of silently coercing it - see [`check_action_value`].
#[derive(Debug, Clone, PartialEq)]
enum AssignedValue {
    /// A signed integer literal, e.g. `-10`, `0`, `100` - signed so a negative value
    /// can be clamped up to a constraint's `min` instead of being rejected outright as
    /// unparsable.
    Number(i64),
    Text(String),
}

/// The assignment operator of a `$path <op> value` action.
///
/// Only [`AssignOp::ClampWarn`] (`:=`) is implemented today; the other two are
/// recognized by [`parse_set_config`] (so a config author's choice of operator is
/// remembered and can be reported back with a clear "not implemented yet" error
/// instead of a generic "malformed" one) but rejected by [`check_set_config_action`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AssignOp {
    /// `=` - **not yet implemented.** Will hard-error instead of clamping/truncating
    /// when the value violates the target's [`Constraint`]. For a constant literal
    /// (today's only supported right-hand side) that check could happen entirely at
    /// config-load time, same as `:=`'s out-of-range case used to before this operator
    /// family existed; once real runtime variables exist, the same check will instead
    /// need to happen when the action actually fires, since the value won't be known
    /// until then.
    Strict,
    /// `:=` - clamps a number into its [`Constraint`]'s range, or (once a string-typed
    /// settable parameter exists) truncates a string to its max length, and warns
    /// when it had to.
    ClampWarn,
    /// `~=` - **not yet implemented.** Will apply the same clamping/truncation as
    /// `ClampWarn`, but silently - no warning even when the value was out of range.
    ClampSilent,
}

/// How a `$`-action's dotted path resolves against the config schema.
enum ParamClass {
    /// A real, currently-settable parameter.
    Settable(SettableDefault),
    /// A real config field or section that isn't (yet) settable at runtime.
    ReadOnly,
    /// Not a real config path at all (typo, or a section/field that doesn't exist).
    Unknown,
}

/// Classifies a `$`-action's dotted path against the config schema.
///
/// `devices.*`/`scenes.*` are read-only wholesale rather than field-by-field: both
/// sections are dynamically shaped (device ids, scene names, control references are
/// all config-author-chosen), so there is no fixed field list to check deeper than the
/// section name - anything under either is real but immutable. `defaults.*` has an
/// exact, fixed field list, so it's checked field-by-field instead: the two brightness
/// keys are settable, `short_press_duration`/`double_click_gap` are read-only, and any
/// other `defaults.*` field is unknown (no such field exists).
fn classify_config_path(path: &str) -> ParamClass {
    match path {
        "defaults.button_brightness" => ParamClass::Settable(SettableDefault::ButtonBrightness),
        "defaults.encoder_brightness" => ParamClass::Settable(SettableDefault::EncoderBrightness),
        "defaults.short_press_duration"
        | "defaults.double_click_gap"
        | "version"
        | "scenes"
        | "devices"
        | "defaults" => ParamClass::ReadOnly,
        _ if path.starts_with("devices.") || path.starts_with("scenes.") => ParamClass::ReadOnly,
        _ => ParamClass::Unknown,
    }
}

/// Splits a `$path <op> value` action string into its dotted path, assignment
/// operator, and parsed right-hand-side literal.
///
/// Recognizes all three operators (`:=`, `~=`, `=`, checked in that order so `:=`'s own
/// `=` character is never mis-split as the bare `=` operator, and likewise for `~=`)
/// even though only `:=` is implemented ([`AssignOp`]) - that way a config using `=`/
/// `~=` gets a clear "not implemented yet" error from [`check_set_config_action`]
/// instead of a generic "malformed" one.
///
/// Returns `None` when `value` doesn't start with `$`, has no operator at all, has an
/// empty path or right-hand side, or a right-hand side that is neither a bare signed
/// integer (e.g. `-10`, `100`) nor a `"double-quoted string"` (no escape support inside
/// quotes yet). Doesn't check the path against the config schema at all - that's
/// [`classify_config_path`]'s job, called separately by both [`check_action_value`]
/// (which needs to report path-specific errors) and [`parse_action`] (which just needs
/// *a* parsed value or none).
fn parse_set_config(value: &str) -> Option<(String, AssignOp, AssignedValue)> {
    let rest = value.strip_prefix('$')?;
    let (path, op, rhs) = if let Some((path, rhs)) = rest.split_once(":=") {
        (path, AssignOp::ClampWarn, rhs)
    } else if let Some((path, rhs)) = rest.split_once("~=") {
        (path, AssignOp::ClampSilent, rhs)
    } else if let Some((path, rhs)) = rest.split_once('=') {
        (path, AssignOp::Strict, rhs)
    } else {
        return None;
    };
    let path = path.trim();
    let rhs = rhs.trim();
    if path.is_empty() || rhs.is_empty() {
        return None;
    }
    let parsed = if let Some(inner) = rhs
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
    {
        AssignedValue::Text(inner.to_string())
    } else if let Ok(number) = rhs.parse::<i64>() {
        AssignedValue::Number(number)
    } else {
        return None;
    };
    Some((path.to_string(), op, parsed))
}

/// The outcome of an action value: stay on the scene, switch to another scene, run a
/// command, or set a runtime-settable config parameter.
#[derive(Debug, PartialEq)]
pub enum Action {
    Stay,
    SwitchScene { scene: String },
    Command { command: String },
    SetConfig { param: SettableDefault, value: u8 },
}

/// Classifies an action value: a bare `@` stays, `@name` switches scene, a well-formed
/// `$path := value` targeting a settable parameter with `:=` (today's only
/// implemented operator - see [`AssignOp`]) sets it, anything else (including a
/// malformed or not-yet-implemented `$`-action - config validation is the real gate
/// against those ever reaching here) is a command.
///
/// A `:=` number is clamped into its target's [`Constraint`] here exactly like
/// [`check_set_config_action`] already validated at config-load time (e.g. `:=
/// 9999999999999` on a `0`-`100` parameter becomes `100`), so the value actually sent
/// to the device always matches what was validated/warned about.
///
/// Note the breaking change from earlier versions: `~` no longer means "stay" (that's
/// now a bare `@`, freeing `~` up for the home-directory expansion [`expand_tilde`]
/// applies to paths/commands elsewhere) - a literal `"~"` action value is now just a
/// (typically failing) attempt to run `~` as a command.
pub fn parse_action(value: &str) -> Action {
    if let Some(scene) = value.strip_prefix('@') {
        if scene.is_empty() {
            Action::Stay
        } else {
            Action::SwitchScene {
                scene: scene.to_string(),
            }
        }
    } else if value.starts_with('$') {
        match parse_set_config(value) {
            Some((path, AssignOp::ClampWarn, AssignedValue::Number(number))) => {
                match classify_config_path(&path) {
                    ParamClass::Settable(param) => {
                        let Constraint::NumberRange { min, max } = param.constraint() else {
                            unreachable!(
                                "no settable string parameter exists yet - see \
                                 Constraint::MaxLength's doc comment"
                            )
                        };
                        // Safe: every constraint in use today has a `max` that fits in
                        // a u8 (0-100), so the clamped value always does too.
                        let clamped = number.clamp(min, max) as u8;
                        Action::SetConfig {
                            param,
                            value: clamped,
                        }
                    }
                    _ => Action::Command {
                        command: value.to_string(),
                    },
                }
            }
            _ => Action::Command {
                command: value.to_string(),
            },
        }
    } else {
        Action::Command {
            command: value.to_string(),
        }
    }
}

/// Reads an action value - a single string or an array of them, per [`check_action_values`]
/// - into an ordered list of action strings, run without waiting on each other.
///
/// Anything that isn't a non-empty string is filtered out (an absent, malformed, or
/// explicitly empty value all yield an empty list) - every case means "nothing to run"
/// to callers, which already treat all of them identically, so there is no need to tell
/// them apart here.
fn action_values(value: &Value) -> Vec<&str> {
    if let Some(value) = value.as_str() {
        if value.is_empty() {
            Vec::new()
        } else {
            vec![value]
        }
    } else if let Some(items) = value.as_array() {
        items
            .iter()
            .filter_map(|item| item.as_str())
            .filter(|item| !item.is_empty())
            .collect()
    } else {
        Vec::new()
    }
}

/// Resolves the actions bound to `event` (e.g. `"pressed"` or `"released"`) on
/// `reference`, falling back to the previously active scene.
///
/// Button actions are inherited from the previous scene: `scene_name` is consulted first,
/// then `previous_scene`. A scene that explicitly configures the reference ends the search —
/// its value for `event` wins (an empty string or an empty array both mean "bound but no
/// action", yielding an empty list, indistinguishable here from "not bound at all").
pub fn action_for_event<'a>(
    scene_name: &str,
    previous_scene: Option<&str>,
    reference: &Reference,
    event: &str,
    scenes: &'a Value,
) -> Vec<&'a str> {
    for name in std::iter::once(scene_name).chain(previous_scene) {
        let Some(scene) = scenes.get(name) else {
            continue;
        };
        let Some(actions) = scene.get("actions") else {
            continue;
        };
        let Some(key_actions) = actions
            .as_object()
            .and_then(|map| map.get(&reference.to_string()))
        else {
            continue;
        };
        return key_actions
            .get(event)
            .map(action_values)
            .unwrap_or_default();
    }
    Vec::new()
}

/// Reads the timer actions for a scene, returning `(seconds, action_values)` if defined.
///
/// The timer entry in `actions` is a single-entry object `{ "<seconds>": "<action>" }`,
/// whose value may be a single string or an array of them, exactly like an event's.
/// Returns `None` if the scene has no timer.
pub fn timer_for_scene<'a>(scene_name: &str, scenes: &'a Value) -> Option<(u64, Vec<&'a str>)> {
    let scene = scenes.get(scene_name)?.as_object()?;
    let actions = scene.get("actions")?.as_object()?;
    let timer = actions.get("timer")?.as_object()?;
    let (seconds_str, action) = timer.iter().next()?;
    let seconds = seconds_str.parse::<u64>().ok()?;
    Some((seconds, action_values(action)))
}

#[cfg(test)]
mod tests {
    use serde_json::Value;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// Serialises the tests that mutate `HOME`, so their environment changes never
    /// interleave with each other while the shared process-global variable is in use.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Temporarily sets `HOME` to `home`, restoring the previous value when dropped.
    struct SetHome(Option<std::ffi::OsString>);

    impl SetHome {
        fn new(home: &Path) -> Self {
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

    /// Creates a unique empty temp directory for config-resolution tests.
    fn temp_dir() -> PathBuf {
        let n = TMP_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = format!("/tmp/dak_resolve_{}_{n}", std::process::id());
        std::fs::create_dir_all(&dir).unwrap();
        PathBuf::from(dir)
    }

    /// An explicitly given config path wins as-is, even if the search directories
    /// hold a config or are completely unrelated.
    #[test]
    fn pick_config_path_prefers_explicit_path() {
        let dir = temp_dir();
        let config = dir.join("config.json");
        std::fs::write(&config, "{}").unwrap();
        let explicit = Path::new("/some/elsewhere/custom.json");

        let picked = super::pick_config_path(Some(explicit), std::slice::from_ref(&dir));
        assert_eq!(picked, explicit);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The first directory holding a `config.json` wins over later ones.
    #[test]
    fn pick_config_path_takes_first_dir_with_config() {
        let first = temp_dir();
        let second = temp_dir();
        std::fs::write(first.join("config.json"), "{}").unwrap();
        std::fs::write(second.join("config.json"), "{}").unwrap();

        let picked = super::pick_config_path(None, &[first.clone(), second.clone()]);
        assert_eq!(picked, first.join("config.json"));
        let _ = std::fs::remove_dir_all(&first);
        let _ = std::fs::remove_dir_all(&second);
    }

    /// Directories without a config are skipped, so a later directory with one wins.
    #[test]
    fn pick_config_path_skips_dirs_without_config() {
        let empty_dir = temp_dir();
        let with_config = temp_dir();
        std::fs::write(with_config.join("config.json"), "{}").unwrap();

        let picked = super::pick_config_path(None, &[empty_dir.clone(), with_config.clone()]);
        assert_eq!(picked, with_config.join("config.json"));
        let _ = std::fs::remove_dir_all(&empty_dir);
        let _ = std::fs::remove_dir_all(&with_config);
    }

    /// When no directory holds a config, the first directory's candidate is returned
    /// so loading reports the missing file at a concrete location.
    #[test]
    fn pick_config_path_falls_back_to_first_dir_candidate() {
        let first = temp_dir();
        let second = temp_dir();

        let picked = super::pick_config_path(None, &[first.clone(), second.clone()]);
        assert_eq!(picked, first.join("config.json"));
        let _ = std::fs::remove_dir_all(&first);
        let _ = std::fs::remove_dir_all(&second);
    }

    /// An empty list of search directories falls back to a bare `config.json`.
    #[test]
    fn pick_config_path_falls_back_to_bare_name() {
        assert_eq!(
            super::pick_config_path(None, &[]),
            PathBuf::from("config.json")
        );
    }

    /// `resolve_config_path` returns an explicit path verbatim, without searching.
    #[test]
    fn resolve_config_path_passes_explicit_path_through() {
        let explicit = Path::new("/opt/custom/settings.json");
        assert_eq!(
            super::resolve_config_path(Some(explicit)),
            explicit.to_path_buf()
        );
    }

    /// A `config.json` in `$HOME/.config/dak/` beats the current directory's one.
    #[test]
    fn resolve_config_path_prefers_home_config_over_cwd() {
        let _guard = ENV_LOCK.lock().unwrap();
        let home = temp_dir();
        std::fs::create_dir_all(home.join(".config/dak")).unwrap();
        let home_config = home.join(".config/dak/config.json");
        std::fs::write(&home_config, "{}").unwrap();
        let _home_guard = SetHome::new(&home);

        let picked = super::resolve_config_path(None);

        assert_eq!(picked, home_config);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// With no config in `$HOME`, the current directory's `config.json` is picked
    /// ahead of the binary directory (the test process's harness directory).
    #[test]
    fn resolve_config_path_cwd_wins_when_home_has_no_config() {
        let _guard = ENV_LOCK.lock().unwrap();
        let home = temp_dir();
        let _home_guard = SetHome::new(&home);

        let picked = super::resolve_config_path(None);

        assert_eq!(picked, std::env::current_dir().unwrap().join("config.json"));
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Every JSON value type maps to a distinct human-readable name, including strings
    /// (which validation never reaches because string action values are handled earlier).
    #[test]
    fn value_type_names_all_types() {
        for (value, expected) in [
            (Value::Null, "null"),
            (Value::Bool(true), "bool"),
            (Value::Number(serde_json::Number::from(1)), "a number"),
            (Value::String("x".to_string()), "a string"),
            (Value::Array(vec![]), "an array"),
            (Value::Object(serde_json::Map::new()), "an object"),
        ] {
            assert_eq!(super::value_type(&value), expected);
        }
    }

    /// `CommandSpec::display` joins the program and its arguments with spaces.
    #[test]
    fn command_spec_display_joins_program_and_args() {
        let command = super::CommandSpec {
            program: "/bin/echo".to_string(),
            args: vec!["hello".to_string(), "world".to_string()],
        };
        assert_eq!(command.display(), "/bin/echo hello world");
    }

    /// A command with no arguments displays just the program.
    #[test]
    fn command_spec_display_without_args() {
        let command = super::CommandSpec {
            program: "/usr/bin/uptime".to_string(),
            args: vec![],
        };
        assert_eq!(command.display(), "/usr/bin/uptime");
    }

    /// A collapsed `params` command line splits into program and whitespace-separated args.
    #[test]
    fn parse_command_line_splits_words_and_args() {
        let command = super::parse_command_line("/usr/bin/date +%H:%M").unwrap();
        assert_eq!(
            command,
            super::CommandSpec {
                program: "/usr/bin/date".to_string(),
                args: vec!["+%H:%M".to_string()],
            }
        );
    }

    /// Quoted segments become a single argument with the quotes removed.
    #[test]
    fn parse_command_line_keeps_quoted_segments_together() {
        let command = super::parse_command_line("/bin/sh -c 'echo hello world'").unwrap();
        assert_eq!(
            command,
            super::CommandSpec {
                program: "/bin/sh".to_string(),
                args: vec!["-c".to_string(), "echo hello world".to_string()],
            }
        );
    }

    /// An unterminated quote makes the whole command line invalid.
    #[test]
    fn parse_command_line_rejects_unbalanced_quotes() {
        let error = super::parse_command_line("/bin/sh -c 'oops").unwrap_err();
        assert!(error.contains("unbalanced quotes"), "{error}");
    }

    /// An empty or whitespace-only command line has no program to run.
    #[test]
    fn parse_command_line_rejects_empty_line() {
        for params in ["", "   "] {
            let error = super::parse_command_line(params).unwrap_err();
            assert!(error.contains("empty command"), "{error}");
        }
    }

    /// A backslash escapes the following character, so a space-separated command can
    /// pass arguments containing spaces without quoting.
    #[test]
    fn parse_command_line_backslash_escapes_next_character() {
        let spec = super::parse_command_line("/bin/echo one\\ two\\ three").unwrap();
        assert_eq!(spec.program, "/bin/echo");
        assert_eq!(spec.args, vec!["one two three"]);
    }

    /// A trailing backslash has no following character to escape; the command
    /// parses fine and the trailing backslash contributes nothing.
    #[test]
    fn parse_command_line_backslash_at_end_is_fine() {
        let params = "/bin/echo one \\";
        let spec = super::parse_command_line(params).unwrap();
        assert_eq!(spec.program, "/bin/echo");
        assert_eq!(spec.args, vec!["one"]);
    }

    /// `parse_device_id` accepts exactly the single digits 1..=9 used in references.
    #[test]
    fn parse_device_id_accepts_single_digits() {
        assert_eq!(super::parse_device_id("1"), Some(1));
        assert_eq!(super::parse_device_id("9"), Some(9));
        assert_eq!(super::parse_device_id("0"), None);
        assert_eq!(super::parse_device_id("10"), None);
        assert_eq!(super::parse_device_id("a"), None);
        assert_eq!(super::parse_device_id(""), None);
    }

    /// `strip_comments` removes both line and block comments outside strings, keeping
    /// non-comment text (and the newline layout) intact.
    #[test]
    fn strip_comments_removes_line_and_block_comments() {
        let input = r#"// leading comment
{
  "scenes": /* inline */ {},
  "devices": {
    // picked device
    "1": { }
  } // trailing
}"#;
        let stripped = super::strip_comments(input);
        assert!(!stripped.contains("//"));
        assert!(!stripped.contains("/*"));
        let json: Value = serde_json::from_str(&stripped).unwrap();
        assert!(json["devices"]["1"].is_object());
        // Line starts are preserved so error positions stay useful.
        assert_eq!(stripped.lines().count(), input.lines().count());
    }

    /// `strip_comments` treats `//` and `/* ... */` inside a string as literal text,
    /// since they are part of the string value and not comments.
    #[test]
    fn strip_comments_leaves_strings_untouched() {
        let input = r#"{ "actions": "@//not/a/comment", "setup": "/* also not */" }"#;
        let json: Value = serde_json::from_str(&super::strip_comments(input)).unwrap();
        assert_eq!(json["actions"], "@//not/a/comment");
        assert_eq!(json["setup"], "/* also not */");
    }

    /// `strip_comments` honours escaped quotes, so `\"` inside a string does not end
    /// the string early and comment-like text after it stays inside the string.
    #[test]
    fn strip_comments_honours_escaped_quotes() {
        let input = r#"{ "text": "say \"//hi\"" }"#;
        let json: Value = serde_json::from_str(&super::strip_comments(input)).unwrap();
        assert_eq!(json["text"], "say \"//hi\"");
    }

    /// An unterminated `/*` comment comments out the rest of the file, so a config
    /// may leave a trailing comment open at the end.
    #[test]
    fn strip_comments_handles_unterminated_block_comment() {
        let input = "{ \"scenes\": {} }\n/* never closed";
        let json: Value = serde_json::from_str(&super::strip_comments(input)).unwrap();
        assert!(json["scenes"].is_object());
    }

    /// A definition with a real serial matches only a discovered device reporting that
    /// exact serial, telling identical devices apart.
    #[test]
    fn discovered_device_matches_by_serial() {
        let definition = super::Mapping {
            device_id: "0300:3002".to_string(),
            device_name: "keypad".to_string(),
            serial: "ABC123".to_string(),
            key_count: 9,
            encoder_count: 3,
            screens: 6,
            buttons: vec![],
            encoders: vec![],
        };
        assert!(super::discovered_device_matches(
            &definition,
            &Some("ABC123".to_string()),
            0x0300,
            0x3002
        ));
        assert!(!super::discovered_device_matches(
            &definition,
            &Some("OTHER".to_string()),
            0x0300,
            0x3002
        ));
        assert!(!super::discovered_device_matches(
            &definition,
            &None,
            0x0300,
            0x3002
        ));
    }

    /// A definition whose serial is "unknown" falls back to the VID:PID string, so
    /// devices without serials still match as long as only one of their kind is present.
    #[test]
    fn discovered_device_matches_falls_back_to_vid_pid() {
        let definition = super::Mapping {
            device_id: "0300:3002".to_string(),
            device_name: "keypad".to_string(),
            serial: "unknown".to_string(),
            key_count: 9,
            encoder_count: 3,
            screens: 6,
            buttons: vec![],
            encoders: vec![],
        };
        assert!(super::discovered_device_matches(
            &definition,
            &None,
            0x0300,
            0x3002
        ));
        assert!(!super::discovered_device_matches(
            &definition,
            &None,
            0x0300,
            0x3003
        ));
        // A reported serial never overrides the VID:PID fallback.
        assert!(super::discovered_device_matches(
            &definition,
            &Some("ANY".to_string()),
            0x0300,
            0x3002
        ));
    }

    /// `operation_reference` reports the reference an operation targets, or `None` only
    /// for unsupported operations, which carry no reference at all.
    #[test]
    fn operation_reference_reports_target_reference() {
        let image = super::SceneOp::SetImage {
            reference: super::Reference::button(1, 3),
            path: "x.png".to_string(),
            refresh_seconds: 0,
        };
        let text = super::SceneOp::Text {
            reference: super::Reference::button(2, 4),
            path: "x.txt".to_string(),
            refresh_seconds: 0,
        };
        let text_exec = super::SceneOp::TextExec {
            reference: super::Reference::button(9, 8),
            command: super::CommandSpec {
                program: "echo".to_string(),
                args: vec![],
            },
            refresh_seconds: 0,
        };
        let image_exec = super::SceneOp::ImageExec {
            reference: super::Reference::button(1, 9),
            command: super::CommandSpec {
                program: "convert".to_string(),
                args: vec![],
            },
            refresh_seconds: 0,
        };
        let launch = super::SceneOp::Launch {
            reference: super::Reference::button(1, 5),
            command: super::CommandSpec {
                program: "true".to_string(),
                args: vec![],
            },
        };
        let clear = super::SceneOp::Clear {
            reference: super::Reference::encoder(1, 1),
        };
        let unsupported = super::SceneOp::Unsupported {
            kind: "frobnicate".to_string(),
        };
        assert_eq!(
            super::operation_reference(&image),
            Some(&super::Reference::button(1, 3))
        );
        assert_eq!(
            super::operation_reference(&text),
            Some(&super::Reference::button(2, 4))
        );
        assert_eq!(
            super::operation_reference(&text_exec),
            Some(&super::Reference::button(9, 8))
        );
        assert_eq!(
            super::operation_reference(&image_exec),
            Some(&super::Reference::button(1, 9))
        );
        assert_eq!(
            super::operation_reference(&launch),
            Some(&super::Reference::button(1, 5))
        );
        assert_eq!(
            super::operation_reference(&clear),
            Some(&super::Reference::encoder(1, 1))
        );
        assert_eq!(super::operation_reference(&unsupported), None);
    }

    /// `refresh_seconds_of` reads the field from the four variants that carry one, and
    /// reports 0 (the same as an absent/zero field) for the three that never do.
    #[test]
    fn refresh_seconds_of_reads_the_field_or_reports_zero() {
        let image = super::SceneOp::SetImage {
            reference: super::Reference::button(1, 1),
            path: "x.png".to_string(),
            refresh_seconds: 5,
        };
        let text = super::SceneOp::Text {
            reference: super::Reference::button(1, 2),
            path: "x.txt".to_string(),
            refresh_seconds: 7,
        };
        let text_exec = super::SceneOp::TextExec {
            reference: super::Reference::button(1, 3),
            command: super::CommandSpec {
                program: "echo".to_string(),
                args: vec![],
            },
            refresh_seconds: 11,
        };
        let image_exec = super::SceneOp::ImageExec {
            reference: super::Reference::button(1, 4),
            command: super::CommandSpec {
                program: "convert".to_string(),
                args: vec![],
            },
            refresh_seconds: 13,
        };
        let launch = super::SceneOp::Launch {
            reference: super::Reference::button(1, 5),
            command: super::CommandSpec {
                program: "true".to_string(),
                args: vec![],
            },
        };
        let clear = super::SceneOp::Clear {
            reference: super::Reference::button(1, 6),
        };
        let unsupported = super::SceneOp::Unsupported {
            kind: "frobnicate".to_string(),
        };
        assert_eq!(super::refresh_seconds_of(&image), 5);
        assert_eq!(super::refresh_seconds_of(&text), 7);
        assert_eq!(super::refresh_seconds_of(&text_exec), 11);
        assert_eq!(super::refresh_seconds_of(&image_exec), 13);
        assert_eq!(super::refresh_seconds_of(&launch), 0);
        assert_eq!(super::refresh_seconds_of(&clear), 0);
        assert_eq!(super::refresh_seconds_of(&unsupported), 0);
    }

    /// A fresh tracker accepts its first generation for a button but rejects unknown ones.
    #[tokio::test]
    async fn text_exec_tracker_starts_at_first_generation() {
        let (tx, _rx) = tokio::sync::mpsc::channel(4);
        let mut tracker = super::ExecTracker::new(tx);
        assert!(!tracker.is_current(4, 0));
        assert_eq!(tracker.bump(4), 1);
        assert!(tracker.is_current(4, 1));
        assert!(!tracker.is_current(4, 0));
        assert!(!tracker.is_current(4, 2));
    }

    /// Bumping a button's generation invalidates the results of any in-flight task.
    #[tokio::test]
    async fn text_exec_tracker_bump_invalidates_older_results() {
        let (tx, _rx) = tokio::sync::mpsc::channel(4);
        let mut tracker = super::ExecTracker::new(tx);
        tracker.bump(7);
        tracker.bump(7);
        assert!(tracker.is_current(7, 2));
        assert!(!tracker.is_current(7, 1));
    }

    /// Cancelling removes the registered task handle and advances the generation.
    #[tokio::test]
    async fn text_exec_tracker_cancel_removes_pending_and_bumps() {
        let (tx, _rx) = tokio::sync::mpsc::channel(4);
        let mut tracker = super::ExecTracker::new(tx);
        let generation = tracker.bump(3);
        let (done_tx, mut done_rx) = tokio::sync::mpsc::channel(1);
        let handle = tokio::spawn(async move {
            let _ = done_tx.send(()).await;
        });
        tracker.insert(3, super::PendingExec { handle, generation });
        assert!(tracker.is_current(3, generation));
        let cancelled = tracker.cancel(3);
        assert!(
            cancelled.is_some(),
            "expected the pending task to be cancelled"
        );
        assert!(tracker.pending.is_empty());
        assert_eq!(tracker.generations.get(&3).copied(), Some(generation + 1));
        assert!(!tracker.is_current(3, generation));
        done_rx.recv().await;
    }

    /// A bare `"~"` expands to `$HOME` exactly.
    #[test]
    fn expand_tilde_expands_bare_tilde() {
        let _guard = ENV_LOCK.lock().unwrap();
        let home = temp_dir();
        let _set_home = SetHome::new(&home);
        assert_eq!(
            super::expand_tilde("~"),
            home.to_string_lossy().into_owned()
        );
    }

    /// A `"~/rest"` expands to `$HOME/rest`.
    #[test]
    fn expand_tilde_expands_tilde_slash_prefix() {
        let _guard = ENV_LOCK.lock().unwrap();
        let home = temp_dir();
        let _set_home = SetHome::new(&home);
        assert_eq!(
            super::expand_tilde("~/foo/bar.png"),
            home.join("foo/bar.png").to_string_lossy().into_owned()
        );
    }

    /// `~` anywhere but the very start of the string is left untouched, matching shell
    /// semantics (only a leading `~` expands).
    #[test]
    fn expand_tilde_leaves_non_leading_tilde_untouched() {
        let _guard = ENV_LOCK.lock().unwrap();
        let home = temp_dir();
        let _set_home = SetHome::new(&home);
        assert_eq!(super::expand_tilde("/foo/~bar"), "/foo/~bar");
        assert_eq!(super::expand_tilde("foo~bar"), "foo~bar");
    }

    /// An absolute path with no leading `~` is returned unchanged.
    #[test]
    fn expand_tilde_leaves_absolute_paths_untouched() {
        let _guard = ENV_LOCK.lock().unwrap();
        let home = temp_dir();
        let _set_home = SetHome::new(&home);
        assert_eq!(super::expand_tilde("/etc/passwd"), "/etc/passwd");
    }

    /// `parse_command_line` expands a leading `~` in both the program name and every
    /// argument.
    #[test]
    fn parse_command_line_expands_tilde_in_program_and_args() {
        let _guard = ENV_LOCK.lock().unwrap();
        let home = temp_dir();
        let _set_home = SetHome::new(&home);
        let command = super::parse_command_line("~/bin/text2gif -t ~/greeting.txt").unwrap();
        assert_eq!(
            command.program,
            home.join("bin/text2gif").to_string_lossy().into_owned()
        );
        assert_eq!(
            command.args,
            vec![
                "-t".to_string(),
                home.join("greeting.txt").to_string_lossy().into_owned()
            ]
        );
    }

    /// A bare `"@"` classifies as `Stay`; the old `"~"` convention no longer does
    /// (breaking change) and is now just a literal (typically failing) command.
    #[test]
    fn parse_action_bare_at_is_stay_and_tilde_is_a_command() {
        assert_eq!(super::parse_action("@"), super::Action::Stay);
        assert_eq!(
            super::parse_action("~"),
            super::Action::Command {
                command: "~".to_string()
            }
        );
    }

    /// `"@SceneName"` classifies as a scene switch.
    #[test]
    fn parse_action_at_name_switches_scene() {
        assert_eq!(
            super::parse_action("@Main"),
            super::Action::SwitchScene {
                scene: "Main".to_string()
            }
        );
    }

    /// A well-formed `$`-assignment targeting a settable parameter classifies as
    /// `SetConfig`.
    #[test]
    fn parse_action_classifies_valid_set_config() {
        assert_eq!(
            super::parse_action("$defaults.button_brightness := 80"),
            super::Action::SetConfig {
                param: super::SettableDefault::ButtonBrightness,
                value: 80,
            }
        );
        assert_eq!(
            super::parse_action("$defaults.encoder_brightness := 5"),
            super::Action::SetConfig {
                param: super::SettableDefault::EncoderBrightness,
                value: 5,
            }
        );
    }

    /// `:=` clamps an out-of-range number into its target's constraint (`0`-`100` for
    /// both of today's settable parameters) instead of rejecting it - a negative
    /// number clamps up to `0`, and one above the max clamps down to `100`, matching
    /// [`super::check_set_config_action`]'s validation-time warning for the same value.
    #[test]
    fn parse_action_clamps_out_of_range_numbers_for_settable_default() {
        assert_eq!(
            super::parse_action("$defaults.button_brightness := -10"),
            super::Action::SetConfig {
                param: super::SettableDefault::ButtonBrightness,
                value: 0,
            }
        );
        assert_eq!(
            super::parse_action("$defaults.button_brightness := 9999999999999"),
            super::Action::SetConfig {
                param: super::SettableDefault::ButtonBrightness,
                value: 100,
            }
        );
    }

    /// A `$`-assignment targeting a read-only/unknown parameter, one with the wrong
    /// value type, or one using a not-yet-implemented operator (`=`/`~=`), falls back
    /// to being treated as a literal command at runtime - config validation is the
    /// real gate against these ever being used.
    #[test]
    fn parse_action_falls_back_to_command_for_invalid_set_config() {
        for value in [
            "$defaults.short_press_duration := 100", // read-only
            "$devices.1.key_count := 5",             // read-only
            "$foo.bar := 1",                         // unknown
            "$defaults.button_brightness := \"80\"", // wrong type
            "$defaults.button_brightness = 80",      // not-yet-implemented operator "="
            "$defaults.button_brightness ~= 80",     // not-yet-implemented operator "~="
        ] {
            assert_eq!(
                super::parse_action(value),
                super::Action::Command {
                    command: value.to_string()
                },
                "for {value}"
            );
        }
    }

    /// [`super::classify_config_path`] recognizes the two settable paths, treats the
    /// rest of `defaults` and all of `devices`/`scenes` as read-only, and anything else
    /// as unknown.
    #[test]
    fn classify_config_path_covers_settable_readonly_and_unknown() {
        assert!(matches!(
            super::classify_config_path("defaults.button_brightness"),
            super::ParamClass::Settable(super::SettableDefault::ButtonBrightness)
        ));
        assert!(matches!(
            super::classify_config_path("defaults.encoder_brightness"),
            super::ParamClass::Settable(super::SettableDefault::EncoderBrightness)
        ));
        for path in [
            "defaults.short_press_duration",
            "defaults.double_click_gap",
            "version",
            "scenes",
            "devices",
            "defaults",
            "devices.1.key_count",
            "scenes.on_start.setup",
        ] {
            assert!(
                matches!(
                    super::classify_config_path(path),
                    super::ParamClass::ReadOnly
                ),
                "expected {path} to be read-only"
            );
        }
        for path in [
            "foo.bar",
            "defaults.nonexistent",
            "default.button_brightness",
        ] {
            assert!(
                matches!(
                    super::classify_config_path(path),
                    super::ParamClass::Unknown
                ),
                "expected {path} to be unknown"
            );
        }
    }

    /// [`super::parse_set_config`] splits a well-formed `$path <op> value` action,
    /// distinguishing a quoted string from a bare number - `"123"` is never treated as
    /// the number `123` - and recognizes all three assignment operators (`:=`, `~=`,
    /// `=`, checked in that order so `:=`'s own `=` character is never mis-split as the
    /// bare `=` operator), even though only `:=` is implemented ([`super::AssignOp`]).
    #[test]
    fn parse_set_config_distinguishes_numbers_strings_and_operators() {
        assert_eq!(
            super::parse_set_config("$defaults.button_brightness := 80"),
            Some((
                "defaults.button_brightness".to_string(),
                super::AssignOp::ClampWarn,
                super::AssignedValue::Number(80)
            ))
        );
        assert_eq!(
            super::parse_set_config("$defaults.button_brightness := \"80\""),
            Some((
                "defaults.button_brightness".to_string(),
                super::AssignOp::ClampWarn,
                super::AssignedValue::Text("80".to_string())
            ))
        );
        assert_eq!(
            super::parse_set_config("$defaults.button_brightness ~= 80"),
            Some((
                "defaults.button_brightness".to_string(),
                super::AssignOp::ClampSilent,
                super::AssignedValue::Number(80)
            ))
        );
        assert_eq!(
            super::parse_set_config("$defaults.button_brightness = 80"),
            Some((
                "defaults.button_brightness".to_string(),
                super::AssignOp::Strict,
                super::AssignedValue::Number(80)
            ))
        );
        assert_eq!(
            super::parse_set_config("$defaults.button_brightness := -10"),
            Some((
                "defaults.button_brightness".to_string(),
                super::AssignOp::ClampWarn,
                super::AssignedValue::Number(-10)
            )),
            "a negative number is a valid literal, clamped later - not malformed"
        );
    }

    /// Malformed `$`-assignments (no operator at all, empty path/value, an unquoted
    /// non-numeric value, or no leading `$` at all) are rejected.
    #[test]
    fn parse_set_config_rejects_malformed_input() {
        for value in [
            "defaults.button_brightness := 80",    // no leading $
            "$defaults.button_brightness 80",      // no operator at all
            "$ := 80",                             // empty path
            "$defaults.button_brightness := ",     // empty value
            "$defaults.button_brightness := high", // unquoted non-numeric
        ] {
            assert_eq!(super::parse_set_config(value), None, "for {value}");
        }
    }
}
