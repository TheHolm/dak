use serde_json::{self, Value};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::baseplane::{Kind, Reference};
use crate::log::{Log, Subsystem};
use crate::map::Mapping;
use crate::press::Defaults;
use crate::variables::{
    check_variables, is_reserved_name, is_valid_name, parse_lone_reference, references_in, VarDef,
    VarRef, VarType, VarValue, Variables,
};
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
    /// The validated `variables` section, keyed by variable name. Empty when the config
    /// declares no variables.
    pub variables: BTreeMap<String, VarDef>,
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
        if key != "scenes"
            && key != "devices"
            && key != "defaults"
            && key != "variables"
            && key != "version"
        {
            errors.push(format!(
                "unknown top-level key \"{key}\", expected \"scenes\", \"devices\", \"defaults\", \"variables\" and \"version\""
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
    let devices = map.get("devices").expect("checked above");
    let by_id = check_devices(devices, &mut errors);
    let defaults = map
        .get("defaults")
        .map(|defaults| check_defaults(defaults, &mut errors))
        .unwrap_or_default();
    let variables = map
        .get("variables")
        .map(|variables| check_variables(variables, &mut errors))
        .unwrap_or_default();

    // References are validated against the declarations with each variable at its initial
    // value; only existence/type matter here, not the (runtime) values themselves.
    let runtime_variables = Variables::new(variables.clone(), &defaults);
    check_scenes(&runtime_variables, scenes, &mut warnings, &mut errors);

    if !errors.is_empty() {
        return Err(errors);
    }
    Ok(LoadedConfig {
        version,
        scenes: scenes.clone(),
        devices: ConfiguredDevices { by_id },
        defaults,
        variables,
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
fn check_scenes(
    variables: &Variables,
    scenes: &Value,
    warnings: &mut Vec<String>,
    errors: &mut Vec<String>,
) {
    let map = match scenes.as_object() {
        Some(map) => map,
        None => {
            errors
                .push("config \"scenes\" must be an object whose keys are scene names".to_string());
            return;
        }
    };

    for (scene_name, scene) in map {
        check_scene(variables, scenes, scene_name, scene, warnings, errors);
    }
}

/// Validates every `$` reference in `text`, pushing an error for each malformed or
/// undeclared one, and returns the references found (empty when the text has none or is
/// malformed).
///
/// Callers use the result to decide whether a value is fully known at load time: a value
/// containing a reference can only be resolved at runtime, so checks against literal
/// content (a scene name, an executable path) are skipped for it.
fn check_references(
    scene_name: &str,
    path: &str,
    text: &str,
    variables: &Variables,
    errors: &mut Vec<String>,
) -> Vec<VarRef> {
    match references_in(text) {
        Ok(refs) => {
            for reference in &refs {
                if variables.kind_of(reference).is_none() {
                    errors.push(format!(
                        "scene \"{scene_name}\": {path} references undefined variable \"{reference}\""
                    ));
                }
            }
            refs
        }
        Err(error) => {
            errors.push(format!("scene \"{scene_name}\": {path}: {error}"));
            Vec::new()
        }
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
    variables: &Variables,
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
            "setup" => check_setup(variables, scene_name, value, warnings, errors),
            "actions" => check_actions(variables, scenes, scene_name, value, errors, warnings),
            other => errors.push(format!(
                "scene \"{scene_name}\": unknown key \"{other}\", expected \"setup\" or \"actions\""
            )),
        }
    }
}

/// Validates the `setup` dictionary of a scene: numbered button entries with a known
/// `type` and a `params` string suited to that type.
fn check_setup(
    variables: &Variables,
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
        check_button_op(variables, scene_name, key, value, warnings, errors);
    }
}

/// Validates one numbered button entry of a scene: a dictionary with a known `type`
/// and a `params` string suited to that type.
fn check_button_op(
    variables: &Variables,
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

    // References in params are validated here; a value that contains one can only be
    // resolved at runtime, so literal checks (file/program existence) are skipped for it.
    let param_refs = check_references(
        scene_name,
        &format!("{location} params"),
        params,
        variables,
        errors,
    );
    let dynamic_params = !param_refs.is_empty();

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
            } else if !dynamic_params {
                check_file_exists(scene_name, &location, params, warnings);
            }
        }
        "text_value" => {
            if params.is_empty() {
                errors.push(format!(
                    "scene \"{scene_name}\": {location} text_value params must be text"
                ));
            }
        }
        "image_exec" | "text_exec" | "launch" => {
            if params.is_empty() {
                errors.push(format!(
                    "scene \"{scene_name}\": {location} {kind} params must be a program command line"
                ));
            } else {
                if !command_needs_shell(params) {
                    match parse_command_line(params) {
                        Ok(command) => {
                            // The program itself may be a reference resolved only at
                            // runtime; only check a literal program path.
                            if matches!(references_in(&command.program), Ok(refs) if refs.is_empty())
                            {
                                check_executable(scene_name, &location, &command.program, warnings);
                            }
                        }
                        Err(error) => {
                            errors.push(format!("scene \"{scene_name}\": {location}: {error}"))
                        }
                    }
                }
            }
        }
        "clear" => {}
        other => {
            errors.push(format!(
                "scene \"{scene_name}\": {location} unknown type \"{other}\", expected image, image_exec, text, text_value, text_exec, launch or clear"
            ));
        }
    }
}

/// Validates the `actions` dictionary of a scene: key entries must be object of string events,
/// and the special `timer` key is validated separately.
fn check_actions(
    variables: &Variables,
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
            check_timer(variables, scenes, scene_name, action, errors, warnings);
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
            check_action_values(
                variables, scenes, scene_name, &path, value, errors, warnings,
            );
        }
    }
}

/// Validates the `timer` entry: it must be a single-entry `{ seconds: action }` object.
fn check_timer(
    variables: &Variables,
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
    let seconds_refs = check_references(scene_name, "actions.timer", seconds, variables, errors);
    if seconds_refs.is_empty() {
        if seconds.parse::<u64>().is_err() {
            errors.push(format!(
                "scene \"{scene_name}\": actions.timer key \"{seconds}\" is not a valid number of seconds"
            ));
        }
    } else {
        for reference in &seconds_refs {
            if matches!(variables.kind_of(reference), Some(kind) if kind != VarType::Int) {
                errors.push(format!(
                    "scene \"{scene_name}\": actions.timer key \"{seconds}\" uses non-integer \"{reference}\", but timer seconds must be an int"
                ));
            }
        }
    }
    check_action_values(
        variables,
        scenes,
        scene_name,
        "actions.timer",
        value,
        errors,
        warnings,
    );
}

/// Validates an action value: either a single string (see [`check_action_value`]) or an
/// array of them, run without waiting on each other. An array may contain at most one
/// scene-changing action (a bare `@` or `@scene`, checked here); every other array
/// element is validated exactly like the single-string form. `[]` is the sole way to
/// spell "bound but no action" for the array form (matching `""` for the string form);
/// an empty string *inside* a non-empty array is rejected instead of silently ignored.
fn check_action_values(
    variables: &Variables,
    scenes: &Value,
    scene_name: &str,
    path: &str,
    value: &Value,
    errors: &mut Vec<String>,
    warnings: &mut Vec<String>,
) {
    if let Some(value) = value.as_str() {
        check_action_value(variables, scenes, scene_name, path, value, errors, warnings);
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
        check_action_value(variables, scenes, scene_name, path, item, errors, warnings);
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
/// Every `$` reference is validated against the declarations first. A value that contains
/// one is only fully known at runtime, so the literal checks (scene existence, executable
/// path) are skipped for it.
///
/// Breaking change: `~` is no longer special-cased here (it used to mean "stay") - a
/// literal `"~"` value now falls through to the command branch below, same as any other
/// string that isn't `@`/`$`-prefixed.
fn check_action_value(
    variables: &Variables,
    scenes: &Value,
    scene_name: &str,
    path: &str,
    value: &str,
    errors: &mut Vec<String>,
    warnings: &mut Vec<String>,
) {
    if let Some(reference) = value.strip_prefix('@') {
        let refs = check_references(scene_name, path, value, variables, errors);
        if reference.is_empty() {
            // A bare "@": stay on the current scene. Nothing to validate.
        } else if refs.is_empty() && !scenes.as_object().unwrap().contains_key(reference) {
            // A literal scene name must exist; a `$`-reference can only be resolved at
            // runtime, so its target scene is checked when the action fires.
            errors.push(format!(
                "scene \"{scene_name}\": {path} references undefined scene \"@{reference}\""
            ));
        }
        return;
    }
    if value.starts_with('$') {
        check_set_config_action(variables, scene_name, path, value, errors, warnings);
        return;
    }

    // A command's executable is only checkable when it is not built from references and
    // does not use a shell.
    let refs = check_references(scene_name, path, value, variables, errors);
    if refs.is_empty() && !command_needs_shell(value) {
        let executable = value.split_whitespace().next().unwrap_or_default();
        if !executable.is_empty() {
            check_executable(scene_name, path, executable, warnings);
        }
    }
}

/// Validates a `$target <op> rhs` assignment action.
///
/// The target must be a declared variable or a writable `defaults` parameter. A real but
/// immutable config field/section is a "read-only parameter" error, a syntactically valid
/// but undeclared variable is an "undefined variable" error, and anything else is an
/// "unknown parameter" error.
///
/// The right-hand side must have the target's type: a numeric target rejects a quoted
/// string, and an int variable rejects a string, under every operator - clamping and
/// truncation are range/length concepts, not type coercion. A literal that is out of
/// range (or over-long) is clamped/truncated by `:=` (with a warning), silently by `~=`,
/// and rejected by `=`. A variable right-hand side is type-checked here, but its value is
/// only known when the action fires, so its range is checked then.
fn check_set_config_action(
    variables: &Variables,
    scene_name: &str,
    path: &str,
    value: &str,
    errors: &mut Vec<String>,
    warnings: &mut Vec<String>,
) {
    let Some((target_path, op, rhs)) = parse_assignment(value) else {
        errors.push(format!(
            "scene \"{scene_name}\": {path} is a malformed \"$\" assignment \"{value}\", expected \"$target <op> value\" with value a literal, a $variable or a \"$(command)\""
        ));
        return;
    };

    match classify_target(&target_path, variables) {
        TargetClass::ReadOnly(config_path) => errors.push(format!(
            "scene \"{scene_name}\": {path} tries to set read-only parameter \"{config_path}\""
        )),
        TargetClass::Unknown(unknown) => errors.push(format!(
            "scene \"{scene_name}\": {path} sets unknown parameter \"{unknown}\""
        )),
        TargetClass::UndefinedVariable(name) => errors.push(format!(
            "scene \"{scene_name}\": {path} sets undefined variable \"${name}\""
        )),
        TargetClass::Variable(name) => {
            let def = variables
                .store()
                .def(&name)
                .cloned()
                .expect("classified as a declared variable");
            check_variable_assignment(
                scene_name, path, &name, &def, op, &rhs, variables, errors, warnings,
            );
        }
        TargetClass::Default(param) => {
            check_default_assignment(
                scene_name, path, param, op, &rhs, variables, errors, warnings,
            );
        }
    }
}

/// Validates the right-hand side of an assignment to a declared user variable against
/// the variable's declared type and, for literals, its range/length.
fn check_variable_assignment(
    scene_name: &str,
    path: &str,
    name: &str,
    def: &VarDef,
    op: AssignOp,
    rhs: &AssignRhs,
    variables: &Variables,
    errors: &mut Vec<String>,
    warnings: &mut Vec<String>,
) {
    let label = format!("${name}");
    match rhs {
        AssignRhs::Int(number) => {
            if def.kind != VarType::Int {
                errors.push(format!(
                    "scene \"{scene_name}\": {path} sets string variable \"${name}\" to the number {number}"
                ));
                return;
            }
            check_number_range(
                scene_name,
                path,
                &label,
                op,
                *number,
                def.min as i64,
                def.max as i64,
                errors,
                warnings,
            );
        }
        AssignRhs::Str(text) => {
            if def.kind != VarType::Str {
                errors.push(format!(
                    "scene \"{scene_name}\": {path} sets int variable \"${name}\" to a string (\"{text}\")"
                ));
                return;
            }
            check_string_length(
                scene_name,
                path,
                &label,
                op,
                text.chars().count(),
                def.max_length,
                errors,
                warnings,
            );
        }
        AssignRhs::Variable(reference) => match variables.kind_of(reference) {
            None => errors.push(format!(
                "scene \"{scene_name}\": {path} references undefined variable \"{reference}\""
            )),
            Some(kind) if kind != def.kind => errors.push(format!(
                "scene \"{scene_name}\": {path} sets {} variable \"${name}\" to \"{reference}\", which is {}",
                kind_label(def.kind),
                kind_label(kind)
            )),
            Some(_) => {}
        },
        AssignRhs::Command(inner) => {
            validate_command_rhs(scene_name, path, inner, variables, errors, warnings);
        }
    }
}

/// Validates a `$(command)` right-hand side: every reference inside it must resolve, and
/// a literal program that needs no shell is checked for existence. The output's type is
/// only known at action time, so no range/type check happens here.
fn validate_command_rhs(
    scene_name: &str,
    path: &str,
    inner: &str,
    variables: &Variables,
    errors: &mut Vec<String>,
    warnings: &mut Vec<String>,
) {
    let refs = check_references(scene_name, path, inner, variables, errors);
    if refs.is_empty() && !command_needs_shell(inner) {
        match parse_command_line(inner) {
            Ok(command) => check_executable(scene_name, path, &command.program, warnings),
            Err(error) => errors.push(format!("scene \"{scene_name}\": {path}: {error}")),
        }
    }
}

/// Validates the right-hand side of an assignment to a writable `defaults` parameter,
/// which is always numeric.
fn check_default_assignment(
    scene_name: &str,
    path: &str,
    param: SettableDefault,
    op: AssignOp,
    rhs: &AssignRhs,
    variables: &Variables,
    errors: &mut Vec<String>,
    warnings: &mut Vec<String>,
) {
    let Constraint::NumberRange { min, max } = param.constraint();
    let label = param.path();
    match rhs {
        AssignRhs::Int(number) => {
            check_number_range(scene_name, path, label, op, *number, min, max, errors, warnings);
        }
        AssignRhs::Str(text) => errors.push(format!(
            "scene \"{scene_name}\": {path} sets \"{label}\" to a string (\"{text}\"), but it requires a number"
        )),
        AssignRhs::Variable(reference) => match variables.kind_of(reference) {
            None => errors.push(format!(
                "scene \"{scene_name}\": {path} references undefined variable \"{reference}\""
            )),
            Some(VarType::Int) => {}
            Some(VarType::Str) => errors.push(format!(
                "scene \"{scene_name}\": {path} sets \"{label}\" to \"{reference}\", which is a string, but it requires a number"
            )),
        },
        AssignRhs::Command(inner) => {
            validate_command_rhs(scene_name, path, inner, variables, errors, warnings);
        }
    }
}

/// A human-readable type name for an assignment error message.
fn kind_label(kind: VarType) -> &'static str {
    match kind {
        VarType::Int => "an int",
        VarType::Str => "a string",
    }
}

/// Applies an operator's range policy to a literal number: `=` rejects an out-of-range
/// value, `:=` clamps with a warning, `~=` clamps silently.
#[allow(clippy::too_many_arguments)]
fn check_number_range(
    scene_name: &str,
    path: &str,
    label: &str,
    op: AssignOp,
    number: i64,
    min: i64,
    max: i64,
    errors: &mut Vec<String>,
    warnings: &mut Vec<String>,
) {
    let clamped = number.clamp(min, max);
    if clamped == number {
        return;
    }
    match op {
        AssignOp::Strict => errors.push(format!(
            "scene \"{scene_name}\": {path} sets \"{label}\" to {number}, outside {min}..={max}; use \":=\" to clamp"
        )),
        AssignOp::ClampWarn => warnings.push(format!(
            "scene \"{scene_name}\": {path} sets \"{label}\" to {number} via \":=\", out of range {min}-{max} - clamped to {clamped}"
        )),
        AssignOp::ClampSilent => {}
    }
}

/// Applies an operator's length policy to a literal string, mirroring
/// [`check_number_range`].
#[allow(clippy::too_many_arguments)]
fn check_string_length(
    scene_name: &str,
    path: &str,
    label: &str,
    op: AssignOp,
    length: usize,
    max_length: usize,
    errors: &mut Vec<String>,
    warnings: &mut Vec<String>,
) {
    if length <= max_length {
        return;
    }
    match op {
        AssignOp::Strict => errors.push(format!(
            "scene \"{scene_name}\": {path} sets \"{label}\" to a {length}-character string, longer than {max_length}; use \":=\" to truncate"
        )),
        AssignOp::ClampWarn => warnings.push(format!(
            "scene \"{scene_name}\": {path} sets \"{label}\" via \":=\" to a {length}-character string, longer than {max_length} - truncated"
        )),
        AssignOp::ClampSilent => {}
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
pub(crate) fn value_type(value: &Value) -> &'static str {
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
    /// Render `text` directly on a button. Unlike [`SceneOp::Text`] the value is the text
    /// itself (already expanded from any `$` references), not a file path, so a variable
    /// can be shown without shelling out to `echo`. References are physical buttons
    /// (1-based). `refresh_seconds` (0 = never) re-applies this operation on its own,
    /// independent of any scene switch - and, like every refreshed entry, re-expands its
    /// `text` from the current variable values each time.
    TextValue {
        reference: Reference,
        text: String,
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

/// A raw scene `setup` entry, before variable/tilde expansion or command parsing.
///
/// Keeping the entry raw lets a `refresh` re-resolve it against the current variables
/// every time it fires, instead of freezing the values it started with.
#[derive(Debug, Clone, PartialEq)]
pub struct RawSceneOp {
    /// The scene the entry came from, for error messages.
    pub scene: String,
    /// The control the entry targets.
    pub reference: Reference,
    /// The entry's `type`.
    pub kind: String,
    /// The entry's raw `params` string (trimmed).
    pub params: String,
    /// The entry's `refresh` seconds (0 = never; unused by launch/clear).
    pub refresh_seconds: u64,
}

/// Parses a scene's `setup` into raw entries without expanding or parsing anything.
///
/// The scene's `setup` dictionary holds the numbered button entries; a scene without a
/// `setup` key (or an undefined scene) yields no entries.
pub fn raw_scene_operations(scene_name: &str, scenes: &Value) -> Result<Vec<RawSceneOp>, String> {
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

        operations.push(RawSceneOp {
            scene: scene_name.to_string(),
            reference,
            kind: kind.to_string(),
            params,
            refresh_seconds,
        });
    }

    Ok(operations)
}

/// Resolves a raw scene entry into a [`SceneOp`], expanding references in `params` from
/// `variables` and then expanding a leading `~` and parsing any command line.
pub fn resolve_scene_op(raw: &RawSceneOp, variables: &Variables) -> Result<SceneOp, String> {
    let scene_name = raw.scene.as_str();
    let reference = raw.reference;
    let key = reference.to_string();
    let params = variables
        .expand(&raw.params)
        .map_err(|error| format!("scene \"{scene_name}\": key \"{key}\": {error}"))?;
    Ok(match raw.kind.as_str() {
        "image" => SceneOp::SetImage {
            reference,
            path: expand_tilde(&params),
            refresh_seconds: raw.refresh_seconds,
        },
        "text" => SceneOp::Text {
            reference,
            path: expand_tilde(&params),
            refresh_seconds: raw.refresh_seconds,
        },
        "text_value" => SceneOp::TextValue {
            reference,
            text: params,
            refresh_seconds: raw.refresh_seconds,
        },
        "text_exec" => SceneOp::TextExec {
            reference,
            command: params_command(scene_name, &key, "text_exec", &params)?,
            refresh_seconds: raw.refresh_seconds,
        },
        "image_exec" => SceneOp::ImageExec {
            reference,
            command: params_command(scene_name, &key, "image_exec", &params)?,
            refresh_seconds: raw.refresh_seconds,
        },
        "launch" => SceneOp::Launch {
            reference,
            command: params_command(scene_name, &key, "launch", &params)?,
        },
        "clear" => SceneOp::Clear { reference },
        other => SceneOp::Unsupported {
            kind: other.to_string(),
        },
    })
}

/// A variable store with no declarations and built-in defaults, for callers (and tests)
/// that have no runtime state; references then fail to resolve.
fn empty_variables() -> Variables {
    Variables::new(BTreeMap::new(), &Defaults::default())
}

/// Builds the ordered list of operations for a scene with no variable state.
///
/// References in `params` cannot be resolved and are reported as errors; callers with a
/// [`Variables`] should use [`scene_operations_with`] instead.
pub fn scene_operations(scene_name: &str, scenes: &Value) -> Result<Vec<SceneOp>, String> {
    scene_operations_with(scene_name, scenes, &empty_variables())
}

/// Builds the ordered list of operations for a scene, expanding references in `params`
/// from `variables`.
pub fn scene_operations_with(
    scene_name: &str,
    scenes: &Value,
    variables: &Variables,
) -> Result<Vec<SceneOp>, String> {
    raw_scene_operations(scene_name, scenes)?
        .iter()
        .map(|raw| resolve_scene_op(raw, variables))
        .collect()
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
    build_command(params).map_err(|error| format!("scene \"{scene_name}\": key \"{key}\": {error}"))
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

/// Whether an already-expanded command line needs a shell to run: it contains an
/// unquoted shell operator (`|`, `&`, `;`, `<`, `>`, a backtick or parentheses) or a
/// newline.
///
/// Operators inside single or double quotes are literal (so `/bin/sh -c 'a | b'` runs
/// `sh` directly, for example), and a backslash escapes the next character outside
/// quotes, exactly as [`parse_command_line`] treats them. A command without any operator
/// runs directly, so no shell is involved.
pub fn command_needs_shell(text: &str) -> bool {
    let mut quote: Option<char> = None;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if let Some(open) = quote {
            if c == open {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => quote = Some(c),
            '\\' => {
                chars.next();
            }
            '|' | '&' | ';' | '<' | '>' | '`' | '(' | ')' | '\n' | '\r' => return true,
            _ => {}
        }
    }
    false
}

/// Builds the command to run for an already-expanded command line: `sh -c "<text>"` when
/// the line contains shell syntax (so pipes and redirection work), or a direct
/// [`parse_command_line`] program plus arguments otherwise.
pub fn build_command(text: &str) -> Result<CommandSpec, String> {
    if command_needs_shell(text) {
        Ok(CommandSpec {
            program: "sh".to_string(),
            args: vec!["-c".to_string(), text.to_string()],
        })
    } else {
        parse_command_line(text)
    }
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
    pub tracker: ExecTracker,
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
    /// The raw `setup` entry behind each button's active operation (1-based), so a
    /// refresh tick can re-resolve it against the current variable values.
    refresh_sources: std::collections::HashMap<u8, RawSceneOp>,
    /// Sender for refresh ticks: a spawned task sleeps for a button's `refresh_seconds`
    /// then sends its key here; the receiving end drives [`SceneRunner::refresh_button`].
    pub refresh_tx: mpsc::Sender<u8>,
    /// The shared variable/default state, when attached (see
    /// [`SceneRunner::set_variables`]); `setup` params are expanded against it.
    variables: Option<Arc<Mutex<Variables>>>,
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
            refresh_sources: std::collections::HashMap::new(),
            refresh_tx,
            variables: None,
        }
    }

    /// Attaches the shared variable/default state, so `setup` params are resolved (and,
    /// for refreshing buttons, re-resolved on every tick) against the current values.
    pub fn set_variables(&mut self, variables: Arc<Mutex<Variables>>) {
        self.variables = Some(variables);
    }

    /// Resolves a raw scene entry against the attached variable state (or an empty one,
    /// which makes any reference an error) for use in `enter_scene`/`refresh_button`.
    fn resolve_op(&self, raw: &RawSceneOp) -> Result<SceneOp, String> {
        match &self.variables {
            Some(variables) => {
                let state = variables.lock().expect("variables mutex poisoned");
                resolve_scene_op(raw, &state)
            }
            None => resolve_scene_op(raw, &empty_variables()),
        }
    }

    /// Applies the numbered button operations of `scene_name` to the device.
    ///
    /// Each raw entry is resolved against the current variable values, and remembered so
    /// its refresh ticks re-resolve it (rather than freezing the values it started with).
    pub async fn enter_scene(
        &mut self,
        scene_name: &str,
        scenes: &Value,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.log
            .debug(Subsystem::Scene, format!("Entering scene \"{scene_name}\""));
        let raw_entries = raw_scene_operations(scene_name, scenes)?;
        let mut operations = Vec::with_capacity(raw_entries.len());
        for raw in &raw_entries {
            operations.push(self.resolve_op(raw)?);
        }
        for raw in raw_entries {
            self.refresh_sources.insert(raw.reference.number, raw);
        }
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
        // Re-resolve from the raw source each tick, so a reference in the params picks up
        // the current variable values instead of the ones captured on scene entry. Falls
        // back to the already-resolved operation when no raw source is known (e.g. when
        // operations were applied directly rather than through `enter_scene`).
        let operation = match self.refresh_sources.get(&key).cloned() {
            Some(raw) => self.resolve_op(&raw)?,
            None => match self.active_setup.get(&key).cloned() {
                Some(operation) => operation,
                None => return Ok(()),
            },
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
                    | SceneOp::TextValue { .. }
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
            SceneOp::TextValue {
                reference: _, text, ..
            } => {
                self.log.debug(
                    Subsystem::Scene,
                    format!("render value on key {key}: \"{text}\""),
                );
                // See the `SetImage` branch above: errors are stringified immediately.
                let render_result =
                    crate::text::render_text(&crate::text::button_text(text), self.image_format)
                        .map_err(|error| error.to_string());
                match render_result {
                    Ok(image) => {
                        if let Err(error) = self
                            .device
                            .set_button_image(key.saturating_sub(1), self.image_format, image)
                            .await
                        {
                            self.log
                                .error(format!("button {key}: failed to draw value text: {error}"));
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
                            format!("button {key}: failed to render value text: {message}"),
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
            ExecEvent::Assignment(_) => {
                // Command-substitution assignments need the shared variable state and
                // the device, so the input loop applies them itself (see `run_device`);
                // this method only handles per-button results.
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
        | SceneOp::TextValue { reference, .. }
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
        | SceneOp::TextValue {
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

/// A command-substitution assignment whose program has finished, ready to be applied by
/// the input loop (which owns the shared state and the device).
#[derive(Debug, PartialEq)]
pub struct CompletedAssign {
    /// The assignment's target.
    pub target: AssignTarget,
    /// The converted value, or a description of why a strict assignment failed.
    pub outcome: Result<VarValue, String>,
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
    /// A command-substitution assignment finished; see [`CompletedAssign`]. The input
    /// loop intercepts this before [`SceneRunner::handle_exec_event`], which only deals
    /// with per-button results.
    Assignment(CompletedAssign),
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

/// A writable `defaults` parameter, settable at runtime by an assignment action.
///
/// The `defaults` address space behaves like variables that also drive the device: a
/// write updates the stored value and pushes the matching hardware setting. Its other
/// members (`short_press_duration`, `double_click_gap`) are read-only - see
/// [`classify_target`].
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

    /// The inclusive numeric range the parameter accepts, matching
    /// `mirajazz::Device::set_brightness`/`set_led_brightness`'s own internal
    /// `percent.clamp(0, 100)`.
    fn constraint(&self) -> Constraint {
        Constraint::NumberRange { min: 0, max: 100 }
    }

    /// The value a non-strict assignment resets the parameter to when a command's output
    /// cannot be converted: the configured `defaults` value loaded at startup.
    pub fn default_value(&self, defaults: &Defaults) -> i32 {
        match self {
            SettableDefault::ButtonBrightness => defaults.button_brightness as i32,
            SettableDefault::EncoderBrightness => defaults.encoder_brightness as i32,
        }
    }
}

/// The numeric constraint an assignment clamps into (`:=`/`~=`) or hard-errors against
/// (`=`).
enum Constraint {
    /// A number must fall within `min..=max` (inclusive on both ends).
    NumberRange { min: i64, max: i64 },
}

/// The target of an assignment action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssignTarget {
    /// A user variable, by name.
    Variable(String),
    /// A writable `defaults` parameter.
    Default(SettableDefault),
}

/// The right-hand side of an assignment action, before clamping/conversion.
///
/// A quoted `"123"` is text, not the number `123`: a numeric target rejects it rather
/// than silently coercing it.
#[derive(Debug, Clone, PartialEq)]
pub enum AssignRhs {
    /// A signed integer literal (signed so a negative value can clamp up to a minimum).
    Int(i64),
    /// A `"double-quoted string"` literal (no escape support inside the quotes yet).
    Str(String),
    /// Another variable, read at action time.
    Variable(VarRef),
    /// A `$(command)` substitution: the command runs at action time and its output is
    /// converted to the target's type. Kept unexpanded, since references inside it are
    /// resolved when the action fires.
    Command(String),
}

/// The assignment operator of a `$target <op> value` action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssignOp {
    /// `=` - rejects an out-of-range/over-long value instead of clamping/truncating.
    Strict,
    /// `:=` - clamps a number or truncates a string into the target's constraints, and
    /// warns when it had to.
    ClampWarn,
    /// `~=` - the same clamping/truncation as [`AssignOp::ClampWarn`], but silently.
    ClampSilent,
}

/// How an assignment target classifies against the actual config, given the declarations.
enum TargetClass {
    /// A declared user variable.
    Variable(String),
    /// A writable `defaults` parameter.
    Default(SettableDefault),
    /// A real but immutable config field/section.
    ReadOnly(String),
    /// A syntactically valid variable name that is not declared.
    UndefinedVariable(String),
    /// Not a real config path or declared variable at all.
    Unknown(String),
}

/// Classifies an assignment target path against the config schema and declarations.
///
/// `devices.*`/`scenes.*` (and the whole `version`/`scenes`/`devices`/`defaults` keys)
/// are read-only wholesale rather than field-by-field, since their shapes are
/// config-author-chosen. `defaults.*` has an exact, fixed field list: the two brightness
/// keys are writable, the two timing keys are read-only, and anything else under
/// `defaults` does not exist. Everything else is a variable reference (`$name` or
/// `$var.name`), declared or not.
fn classify_target(path: &str, variables: &Variables) -> TargetClass {
    match path {
        "defaults.button_brightness" => TargetClass::Default(SettableDefault::ButtonBrightness),
        "defaults.encoder_brightness" => TargetClass::Default(SettableDefault::EncoderBrightness),
        "defaults.short_press_duration" | "defaults.double_click_gap" => {
            TargetClass::ReadOnly(path.to_string())
        }
        "version" | "scenes" | "devices" | "defaults" => TargetClass::ReadOnly(path.to_string()),
        _ if path.starts_with("devices.") || path.starts_with("scenes.") => {
            TargetClass::ReadOnly(path.to_string())
        }
        _ if path.starts_with("defaults.") => TargetClass::Unknown(path.to_string()),
        _ => {
            let name = path.strip_prefix("var.").unwrap_or(path);
            if is_valid_name(name) && (path.starts_with("var.") || !is_reserved_name(name)) {
                if variables.store().contains(name) {
                    TargetClass::Variable(name.to_string())
                } else {
                    TargetClass::UndefinedVariable(name.to_string())
                }
            } else {
                TargetClass::Unknown(path.to_string())
            }
        }
    }
}

/// Splits a `$target <op> rhs` action into its raw target path, assignment operator and
/// parsed right-hand side.
///
/// The target runs from the `$` up to the first whitespace or operator character, so the
/// operator is always the one the author wrote and never one appearing inside the
/// right-hand side. The three operators (`:=`, `~=`, `=`) are checked in that order so
/// `:=` is not mis-split as `=`. Returns `None` when there is no leading `$`, no operator,
/// an empty target, or a malformed right-hand side (see [`parse_rhs`]).
fn parse_assignment(value: &str) -> Option<(String, AssignOp, AssignRhs)> {
    let rest = value.strip_prefix('$')?;
    let target_end = rest
        .find(|c: char| c.is_whitespace() || c == ':' || c == '~' || c == '=')
        .unwrap_or(rest.len());
    let target = rest[..target_end].trim();
    if target.is_empty() {
        return None;
    }
    let after = rest[target_end..].trim_start();
    let (op, rhs) = if let Some(rhs) = after.strip_prefix(":=") {
        (AssignOp::ClampWarn, rhs)
    } else if let Some(rhs) = after.strip_prefix("~=") {
        (AssignOp::ClampSilent, rhs)
    } else if let Some(rhs) = after.strip_prefix('=') {
        (AssignOp::Strict, rhs)
    } else {
        return None;
    };
    let rhs = rhs.trim();
    if rhs.is_empty() {
        return None;
    }
    Some((target.to_string(), op, parse_rhs(rhs)?))
}

/// Parses an assignment's right-hand side: a single variable reference (`$b`), a
/// `"double-quoted string"` literal, or a bare signed integer.
fn parse_rhs(text: &str) -> Option<AssignRhs> {
    if let Some(inner) = text
        .strip_prefix("$(")
        .and_then(|rest| rest.strip_suffix(')'))
    {
        // Nested command substitution is not supported; reject rather than silently
        // handing the inner `$(` to the shell.
        if inner.is_empty() || inner.contains("$(") {
            return None;
        }
        return Some(AssignRhs::Command(inner.to_string()));
    }
    if text.starts_with('$') {
        return parse_lone_reference(text).map(AssignRhs::Variable);
    }
    if let Some(inner) = text
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
    {
        return Some(AssignRhs::Str(inner.to_string()));
    }
    text.parse::<i64>().ok().map(AssignRhs::Int)
}

/// Classifies a raw target path into a typed target using syntax only, without any
/// declarations: `parse_action` runs without a variable store, so a syntactically valid
/// variable target is accepted here and checked against the declarations during config
/// validation ([`classify_target`]).
fn classify_assign_target_syntax(path: &str) -> Option<AssignTarget> {
    match path {
        "defaults.button_brightness" => {
            Some(AssignTarget::Default(SettableDefault::ButtonBrightness))
        }
        "defaults.encoder_brightness" => {
            Some(AssignTarget::Default(SettableDefault::EncoderBrightness))
        }
        _ => {
            if let Some(name) = path.strip_prefix("var.") {
                is_valid_name(name).then(|| AssignTarget::Variable(name.to_string()))
            } else if is_valid_name(path) && !is_reserved_name(path) {
                Some(AssignTarget::Variable(path.to_string()))
            } else {
                None
            }
        }
    }
}

/// The outcome of an action value: stay on the scene, switch to another scene, run a
/// command, or assign to a variable/default.
#[derive(Debug, PartialEq)]
pub enum Action {
    Stay,
    SwitchScene {
        scene: String,
    },
    Command {
        command: String,
    },
    Assign {
        target: AssignTarget,
        op: AssignOp,
        rhs: AssignRhs,
    },
}

/// Classifies an action value: a bare `@` stays, `@name` switches scene, a well-formed
/// `$target <op> rhs` assignment (see [`parse_assignment`]) whose target is
/// syntactically a variable or a writable default assigns, and anything else - including
/// a malformed or read-only/unknown `$`-action, which config validation is the real gate
/// against - is a command.
///
/// Clamping/truncation is deferred to when the action fires (see [`apply_assignment`]),
/// since a variable right-hand side is only known then.
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
        match parse_assignment(value) {
            Some((target_path, op, rhs)) => match classify_assign_target_syntax(&target_path) {
                Some(target) => Action::Assign { target, op, rhs },
                None => Action::Command {
                    command: value.to_string(),
                },
            },
            None => Action::Command {
                command: value.to_string(),
            },
        }
    } else {
        Action::Command {
            command: value.to_string(),
        }
    }
}

/// Resolves an action value to an [`Action`], expanding `$` references against the
/// current state first.
///
/// An assignment is parsed structurally without expanding its left-hand side (only the
/// assignment's own right-hand side is resolved, when it fires); every other value -
/// a scene switch target, a command - is expanded as a whole.
pub fn resolve_action(value: &str, variables: &Variables) -> Result<Action, String> {
    if value.starts_with('$') && parse_assignment(value).is_some() {
        return Ok(parse_action(value));
    }
    Ok(parse_action(&variables.expand(value)?))
}

/// One scalar value on its way into an assignment, kept in its source precision until it
/// is clamped/truncated for the target.
enum Scalar {
    /// An integer, as parsed (before clamping to the target's `i32` range).
    Int(i64),
    /// A string.
    Str(String),
}

/// Applies an assignment whose value is known now - a literal or another variable's
/// current value. Returns the writable default and its new value when the target is one,
/// so the caller can push it to the device; returns `None` for a variable target or a
/// rejected assignment.
///
/// `warn` controls whether a clamp/truncation is reported here: a literal was already
/// validated (and warned about) at config-load time, while a variable's value is only
/// known now.
pub fn apply_assignment(
    target: &AssignTarget,
    op: AssignOp,
    rhs: &AssignRhs,
    variables: &mut Variables,
    warn: bool,
    log: Log,
) -> Option<(SettableDefault, i32)> {
    let scalar = match rhs {
        AssignRhs::Int(number) => Scalar::Int(*number),
        AssignRhs::Str(text) => Scalar::Str(text.clone()),
        AssignRhs::Variable(reference) => match variables.kind_of(reference) {
            Some(VarType::Int) => {
                match variables
                    .read(reference)
                    .ok()
                    .and_then(|text| text.parse::<i64>().ok())
                {
                    Some(number) => Scalar::Int(number),
                    None => {
                        log.error(format!(
                            "assignment reads int variable \"{reference}\", but its value is not an integer"
                        ));
                        return None;
                    }
                }
            }
            Some(VarType::Str) => match variables.read(reference) {
                Ok(text) => Scalar::Str(text),
                Err(error) => {
                    log.error(error);
                    return None;
                }
            },
            None => {
                log.error(format!(
                    "assignment reads undefined variable \"{reference}\""
                ));
                return None;
            }
        },
        AssignRhs::Command(_) => {
            log.error(
                "command-substitution assignments must be started on their own task (start_command_assignment)",
            );
            return None;
        }
    };

    match target {
        AssignTarget::Variable(name) => {
            let Some(def) = variables.store().def(name).cloned() else {
                log.error(format!("assignment to undeclared variable \"${name}\""));
                return None;
            };
            match (def.kind, scalar) {
                (VarType::Int, Scalar::Int(number)) => {
                    let value = clamp_int(
                        number,
                        def.min as i64,
                        def.max as i64,
                        op,
                        &format!("${name}"),
                        warn,
                        log,
                    )?;
                    variables
                        .store_mut()
                        .set(name, crate::variables::VarValue::Int(value));
                }
                (VarType::Str, Scalar::Str(text)) => {
                    let value =
                        truncate_str(text, def.max_length, op, &format!("${name}"), warn, log)?;
                    variables
                        .store_mut()
                        .set(name, crate::variables::VarValue::Str(value));
                }
                (kind, _) => {
                    log.error(format!(
                        "assignment to ${name} has the wrong type, expected {}",
                        match kind {
                            VarType::Int => "an int",
                            VarType::Str => "a string",
                        }
                    ));
                    return None;
                }
            }
            None
        }
        AssignTarget::Default(param) => {
            let Scalar::Int(number) = scalar else {
                log.error(format!("assignment to {} requires a number", param.path()));
                return None;
            };
            let Constraint::NumberRange { min, max } = param.constraint();
            let value = clamp_int(number, min, max, op, param.path(), warn, log)?;
            match param {
                SettableDefault::ButtonBrightness => variables.set_button_brightness(value),
                SettableDefault::EncoderBrightness => variables.set_encoder_brightness(value),
            }
            Some((*param, value))
        }
    }
}

/// Clamps `number` into `min..=max`, applying the operator's policy: `=` rejects an
/// out-of-range value, `:=` clamps (warning when `warn`), `~=` clamps silently. Returns
/// `None` only when a strict assignment rejected the value.
fn clamp_int(
    number: i64,
    min: i64,
    max: i64,
    op: AssignOp,
    label: &str,
    warn: bool,
    log: Log,
) -> Option<i32> {
    if number < min || number > max {
        let clamped = number.clamp(min, max);
        match op {
            AssignOp::Strict => {
                log.error(format!(
                    "assignment to {label} rejected out-of-range value {number} (allowed {min}..={max})"
                ));
                return None;
            }
            AssignOp::ClampWarn => {
                if warn {
                    log.warn(format!(
                        "assignment to {label} clamped out-of-range value {number} to {clamped}"
                    ));
                }
            }
            AssignOp::ClampSilent => {}
        }
        return Some(clamped as i32);
    }
    Some(number as i32)
}

/// Truncates `text` to at most `max_length` characters, applying the operator's policy
/// like [`clamp_int`]. Returns `None` only when a strict assignment rejected the value.
fn truncate_str(
    text: String,
    max_length: usize,
    op: AssignOp,
    label: &str,
    warn: bool,
    log: Log,
) -> Option<String> {
    if text.chars().count() > max_length {
        match op {
            AssignOp::Strict => {
                log.error(format!(
                    "assignment to {label} rejected a value longer than {max_length} characters"
                ));
                return None;
            }
            AssignOp::ClampWarn => {
                if warn {
                    log.warn(format!(
                        "assignment to {label} truncated a value longer than {max_length} characters"
                    ));
                }
            }
            AssignOp::ClampSilent => {}
        }
        return Some(text.chars().take(max_length).collect());
    }
    Some(text)
}

/// The conversion a command-substitution assignment applies to its program's output.
enum Conversion {
    /// Parse the first non-empty line as an integer, then clamp to `min..=max`; `default`
    /// is used on a non-strict conversion failure.
    Int { min: i32, max: i32, default: i32 },
    /// Use the whole output (trailing newlines stripped), truncated to `max_length`;
    /// `default` is used on a non-strict conversion failure.
    Str { max_length: usize, default: String },
}

/// Prepares a `$(command)` assignment: resolves the target's conversion parameters and
/// expands and builds the command to run. Called at dispatch time, so any variable read
/// inside the command sees the value as of the moment the action was triggered.
fn prepare_command_assignment(
    target: &AssignTarget,
    inner: &str,
    state: &Variables,
) -> Result<(Conversion, CommandSpec, String), String> {
    let (conversion, label) = match target {
        AssignTarget::Variable(name) => {
            let def = state
                .store()
                .def(name)
                .ok_or_else(|| format!("assignment to undeclared variable \"${name}\""))?;
            let conversion = match def.kind {
                VarType::Int => Conversion::Int {
                    min: def.min,
                    max: def.max,
                    default: def.initial.as_int().unwrap_or(0),
                },
                VarType::Str => Conversion::Str {
                    max_length: def.max_length,
                    default: def.initial.as_str().unwrap_or("").to_string(),
                },
            };
            (conversion, format!("${name}"))
        }
        AssignTarget::Default(param) => {
            let Constraint::NumberRange { min, max } = param.constraint();
            (
                Conversion::Int {
                    min: min as i32,
                    max: max as i32,
                    default: param.default_value(state.loaded_defaults()),
                },
                param.path().to_string(),
            )
        }
    };
    let expanded = state.expand(inner)?;
    let spec = build_command(&expanded)?;
    Ok((conversion, spec, label))
}

/// Starts a `$(command)` assignment on its own task, so the input loop is never blocked by
/// the command and sibling actions keep running in parallel.
///
/// The command is expanded and built here (reading the current values of any referenced
/// variables), then the task runs it, converts its output, and reports the result through
/// `exec_tx` as [`ExecEvent::Assignment`] for the input loop to apply.
pub fn start_command_assignment(
    target: AssignTarget,
    op: AssignOp,
    inner: &str,
    variables: &Arc<Mutex<Variables>>,
    exec_tx: mpsc::Sender<ExecEvent>,
    log: Log,
) {
    let prepared = {
        let state = variables.lock().expect("variables mutex poisoned");
        prepare_command_assignment(&target, inner, &state)
    };
    let (conversion, spec, label) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            log.error(error);
            return;
        }
    };
    tokio::spawn(async move {
        let outcome = match run_command_with_timeout(&spec, EXEC_TIMEOUT).await {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(output) => convert_output(&output, &conversion, op, &label, log),
                Err(_) => conversion_failure(op, &conversion, &label, "output is not UTF-8", log),
            },
            Err(error) => conversion_failure(op, &conversion, &label, &error, log),
        };
        let _ = exec_tx
            .send(ExecEvent::Assignment(CompletedAssign { target, outcome }))
            .await;
    });
}

/// Converts a command's output to the target type, applying the operator's policy to an
/// out-of-range/over-long value exactly like a literal assignment.
fn convert_output(
    output: &str,
    conversion: &Conversion,
    op: AssignOp,
    label: &str,
    log: Log,
) -> Result<VarValue, String> {
    match conversion {
        Conversion::Int { min, max, .. } => {
            let Some(line) = output.lines().find(|line| !line.trim().is_empty()) else {
                return conversion_failure(op, conversion, label, "no output", log);
            };
            let Ok(number) = line.trim().parse::<i64>() else {
                return conversion_failure(op, conversion, label, "output is not an integer", log);
            };
            let (min, max) = (*min as i64, *max as i64);
            if number < min || number > max {
                return match op {
                    AssignOp::Strict => Err(format!(
                        "assignment to {label} got out-of-range value {number} from its command (allowed {min}..={max})"
                    )),
                    AssignOp::ClampWarn => {
                        log.warn(format!(
                            "assignment to {label} clamped command output {number} to {}",
                            number.clamp(min, max)
                        ));
                        Ok(VarValue::Int(number.clamp(min, max) as i32))
                    }
                    AssignOp::ClampSilent => Ok(VarValue::Int(number.clamp(min, max) as i32)),
                };
            }
            Ok(VarValue::Int(number as i32))
        }
        Conversion::Str { max_length, .. } => {
            let text = output.trim_end_matches(['\n', '\r']).to_string();
            if text.chars().count() > *max_length {
                return match op {
                    AssignOp::Strict => Err(format!(
                        "assignment to {label} got a value longer than {max_length} characters from its command"
                    )),
                    AssignOp::ClampWarn => {
                        log.warn(format!(
                            "assignment to {label} truncated command output to {max_length} characters"
                        ));
                        Ok(VarValue::Str(text.chars().take(*max_length).collect()))
                    }
                    AssignOp::ClampSilent => {
                        Ok(VarValue::Str(text.chars().take(*max_length).collect()))
                    }
                };
            }
            Ok(VarValue::Str(text))
        }
    }
}

/// Builds a conversion failure result: a strict assignment fails, `:=` warns and falls
/// back to the default, `~=` falls back silently.
fn conversion_failure(
    op: AssignOp,
    conversion: &Conversion,
    label: &str,
    reason: &str,
    log: Log,
) -> Result<VarValue, String> {
    match op {
        AssignOp::Strict => Err(format!(
            "assignment to {label} could not use its command output ({reason})"
        )),
        AssignOp::ClampWarn => {
            log.warn(format!(
                "assignment to {label} could not use its command output ({reason}); using the default value"
            ));
            Ok(default_value(conversion))
        }
        AssignOp::ClampSilent => Ok(default_value(conversion)),
    }
}

/// The default value a failed command-output conversion falls back to.
fn default_value(conversion: &Conversion) -> VarValue {
    match conversion {
        Conversion::Int { default, .. } => VarValue::Int(*default),
        Conversion::Str { default, .. } => VarValue::Str(default.clone()),
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
    timer_for_scene_with(scene_name, scenes, &empty_variables())
        .ok()
        .flatten()
}

/// Reads the timer actions for a scene, resolving its seconds against `variables`.
///
/// The seconds key is either a number or a single int variable reference. Returns
/// `Ok(None)` when the scene has no timer (or an unrecognizable one, matching
/// [`timer_for_scene`]), and `Err` when a reference is malformed or unresolvable.
pub fn timer_for_scene_with<'a>(
    scene_name: &str,
    scenes: &'a Value,
    variables: &Variables,
) -> Result<Option<(u64, Vec<&'a str>)>, String> {
    let Some(timer) = scenes
        .get(scene_name)
        .and_then(Value::as_object)
        .and_then(|scene| scene.get("actions"))
        .and_then(Value::as_object)
        .and_then(|actions| actions.get("timer"))
        .and_then(Value::as_object)
    else {
        return Ok(None);
    };
    let Some((seconds_str, action)) = timer.iter().next() else {
        return Ok(None);
    };
    let seconds = match seconds_str.parse::<u64>() {
        Ok(seconds) => seconds,
        Err(_) => {
            let refs = references_in(seconds_str)
                .map_err(|error| format!("scene \"{scene_name}\": actions.timer: {error}"))?;
            match refs.as_slice() {
                [reference] if variables.kind_of(reference) == Some(VarType::Int) => {
                    let text = variables.read(reference).map_err(|error| {
                        format!("scene \"{scene_name}\": actions.timer: {error}")
                    })?;
                    text.parse::<u64>().map_err(|_| {
                        format!(
                            "scene \"{scene_name}\": actions.timer seconds \"{seconds_str}\" resolved to \"{text}\", which is not a valid number of seconds"
                        )
                    })?
                }
                _ => return Ok(None),
            }
        }
    };
    Ok(Some((seconds, action_values(action))))
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};
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

    /// Temporarily unsets `HOME` entirely, restoring the previous value (if any) when
    /// dropped. Distinct from [`SetHome`], which always leaves `HOME` set to something -
    /// this is for exercising the "no `$HOME` at all" fallback of [`expand_tilde`].
    struct UnsetHome(Option<std::ffi::OsString>);

    impl UnsetHome {
        fn new() -> Self {
            let old = std::env::var_os("HOME");
            std::env::remove_var("HOME");
            UnsetHome(old)
        }
    }

    impl Drop for UnsetHome {
        fn drop(&mut self) {
            if let Some(old) = &self.0 {
                std::env::set_var("HOME", old);
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
            protocol_version: None,
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
            protocol_version: None,
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
        let text_value = super::SceneOp::TextValue {
            reference: super::Reference::button(3, 5),
            text: "hello".to_string(),
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
            super::operation_reference(&text_value),
            Some(&super::Reference::button(3, 5))
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

    /// `refresh_seconds_of` reads the field from the five variants that carry one, and
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
        let text_value = super::SceneOp::TextValue {
            reference: super::Reference::button(1, 7),
            text: "hello".to_string(),
            refresh_seconds: 9,
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
        assert_eq!(super::refresh_seconds_of(&text_value), 9);
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

    /// With `$HOME` unset entirely (not merely empty), a leading `~` is left
    /// untouched rather than expanding to nothing or panicking.
    #[test]
    fn expand_tilde_leaves_value_unchanged_when_home_is_unset() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _unset_home = UnsetHome::new();
        assert_eq!(super::expand_tilde("~/foo/bar.png"), "~/foo/bar.png");
        assert_eq!(super::expand_tilde("~"), "~");
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

    /// A well-formed `$`-assignment classifies as `Assign`, with the right target,
    /// operator and right-hand side for literals, variable references and both scopes.
    #[test]
    fn parse_action_classifies_assignments() {
        use crate::variables::{Scope, VarRef};
        assert_eq!(
            super::parse_action("$defaults.button_brightness := 80"),
            super::Action::Assign {
                target: super::AssignTarget::Default(super::SettableDefault::ButtonBrightness),
                op: super::AssignOp::ClampWarn,
                rhs: super::AssignRhs::Int(80),
            }
        );
        assert_eq!(
            super::parse_action("$count ~= 5"),
            super::Action::Assign {
                target: super::AssignTarget::Variable("count".to_string()),
                op: super::AssignOp::ClampSilent,
                rhs: super::AssignRhs::Int(5),
            }
        );
        assert_eq!(
            super::parse_action("$var.count = -10"),
            super::Action::Assign {
                target: super::AssignTarget::Variable("count".to_string()),
                op: super::AssignOp::Strict,
                rhs: super::AssignRhs::Int(-10),
            }
        );
        assert_eq!(
            super::parse_action("$name := \"hi\""),
            super::Action::Assign {
                target: super::AssignTarget::Variable("name".to_string()),
                op: super::AssignOp::ClampWarn,
                rhs: super::AssignRhs::Str("hi".to_string()),
            }
        );
        assert_eq!(
            super::parse_action("$a := $b"),
            super::Action::Assign {
                target: super::AssignTarget::Variable("a".to_string()),
                op: super::AssignOp::ClampWarn,
                rhs: super::AssignRhs::Variable(VarRef {
                    scope: Scope::Var,
                    name: "b".to_string(),
                }),
            }
        );
        assert_eq!(
            super::parse_action("$defaults.encoder_brightness := $level"),
            super::Action::Assign {
                target: super::AssignTarget::Default(super::SettableDefault::EncoderBrightness),
                op: super::AssignOp::ClampWarn,
                rhs: super::AssignRhs::Variable(VarRef {
                    scope: Scope::Var,
                    name: "level".to_string(),
                }),
            }
        );
    }

    /// A `$`-assignment that is not syntactically a variable or writable default falls
    /// back to being treated as a literal command at runtime - config validation is the
    /// real gate against these ever being used.
    #[test]
    fn parse_action_falls_back_to_command_for_unusable_assignments() {
        for value in [
            "$defaults.short_press_duration := 100", // read-only, not a writable default
            "$devices.1.key_count := 5",             // read-only
            "$foo.bar := 1",                         // unknown scope
            "$defaults.button_brightness := high",   // unparsable right-hand side
            "$defaults.button_brightness 80",        // no operator
            "$ := 80",                               // empty target
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

    /// A variable store for assignment tests: `count` is an int in `0..=10` and `name`
    /// is a string of at most 3 characters.
    fn assignment_variables() -> crate::variables::Variables {
        let mut defs = std::collections::BTreeMap::new();
        defs.insert("count".to_string(), crate::variables::VarDef::int(0, 10, 5));
        defs.insert(
            "name".to_string(),
            crate::variables::VarDef::string(3, "abc".to_string()),
        );
        crate::variables::Variables::new(defs, &crate::press::Defaults::default())
    }

    /// `clamp_int` passes an in-range value through, clamps out-of-range values for
    /// `:=`/`~=`, and rejects them for `=`.
    #[test]
    fn clamp_int_applies_operator_policy() {
        let log = crate::log::Log::default();
        assert_eq!(
            super::clamp_int(50, 0, 100, super::AssignOp::Strict, "x", true, log),
            Some(50)
        );
        assert_eq!(
            super::clamp_int(200, 0, 100, super::AssignOp::ClampWarn, "x", true, log),
            Some(100)
        );
        assert_eq!(
            super::clamp_int(-5, 0, 100, super::AssignOp::ClampSilent, "x", true, log),
            Some(0)
        );
        assert_eq!(
            super::clamp_int(200, 0, 100, super::AssignOp::Strict, "x", true, log),
            None,
            "a strict assignment rejects an out-of-range value"
        );
    }

    /// `truncate_str` passes a short value through, truncates an over-long one for
    /// `:=`/`~=`, and rejects it for `=`.
    #[test]
    fn truncate_str_applies_operator_policy() {
        let log = crate::log::Log::default();
        let text = |s: &str| s.to_string();
        assert_eq!(
            super::truncate_str(text("abc"), 3, super::AssignOp::Strict, "x", true, log),
            Some("abc".to_string())
        );
        assert_eq!(
            super::truncate_str(
                text("abcdef"),
                3,
                super::AssignOp::ClampWarn,
                "x",
                true,
                log
            ),
            Some("abc".to_string())
        );
        assert_eq!(
            super::truncate_str(
                text("abcdef"),
                3,
                super::AssignOp::ClampSilent,
                "x",
                true,
                log
            ),
            Some("abc".to_string())
        );
        assert_eq!(
            super::truncate_str(text("abcdef"), 3, super::AssignOp::Strict, "x", true, log),
            None,
            "a strict assignment rejects an over-long value"
        );
    }

    /// `apply_assignment` clamps a variable's value into its declared range, truncates a
    /// string to its max length, and for a writable default stores the clamped value and
    /// reports the side effect the caller must push to the device.
    #[test]
    fn apply_assignment_updates_variables_and_reports_default_side_effect() {
        let log = crate::log::Log::default();
        let mut variables = assignment_variables();

        assert_eq!(
            super::apply_assignment(
                &super::AssignTarget::Variable("count".to_string()),
                super::AssignOp::ClampWarn,
                &super::AssignRhs::Int(50),
                &mut variables,
                true,
                log,
            ),
            None
        );
        assert_eq!(
            variables.store().get("count"),
            Some(&crate::variables::VarValue::Int(10))
        );

        super::apply_assignment(
            &super::AssignTarget::Variable("name".to_string()),
            super::AssignOp::ClampWarn,
            &super::AssignRhs::Str("abcdef".to_string()),
            &mut variables,
            true,
            log,
        );
        assert_eq!(
            variables.store().get("name"),
            Some(&crate::variables::VarValue::Str("abc".to_string()))
        );

        let side_effect = super::apply_assignment(
            &super::AssignTarget::Default(super::SettableDefault::ButtonBrightness),
            super::AssignOp::ClampWarn,
            &super::AssignRhs::Int(200),
            &mut variables,
            true,
            log,
        );
        assert_eq!(
            side_effect,
            Some((super::SettableDefault::ButtonBrightness, 100))
        );
        assert_eq!(variables.button_brightness(), 100);
    }

    /// `parse_assignment` splits the target from the operator and parses each kind of
    /// right-hand side, without mistaking a character inside the right-hand side for the
    /// assignment's own operator.
    #[test]
    fn parse_assignment_splits_target_operator_and_rhs() {
        use crate::variables::{Scope, VarRef};
        assert_eq!(
            super::parse_assignment("$defaults.button_brightness := 80"),
            Some((
                "defaults.button_brightness".to_string(),
                super::AssignOp::ClampWarn,
                super::AssignRhs::Int(80)
            ))
        );
        assert_eq!(
            super::parse_assignment("$a := \"a=b\""),
            Some((
                "a".to_string(),
                super::AssignOp::ClampWarn,
                super::AssignRhs::Str("a=b".to_string())
            )),
            "an `=` inside a quoted right-hand side is not the operator"
        );
        assert_eq!(
            super::parse_assignment("$a := $b"),
            Some((
                "a".to_string(),
                super::AssignOp::ClampWarn,
                super::AssignRhs::Variable(VarRef {
                    scope: Scope::Var,
                    name: "b".to_string(),
                })
            ))
        );
        assert_eq!(
            super::parse_assignment("$var.a = -10"),
            Some((
                "var.a".to_string(),
                super::AssignOp::Strict,
                super::AssignRhs::Int(-10)
            ))
        );
    }

    /// `parse_assignment` accepts a whole `$(command)` right-hand side, treats a quoted
    /// one as a literal string, and rejects an empty or nested substitution.
    #[test]
    fn parse_assignment_accepts_command_substitution() {
        assert_eq!(
            super::parse_assignment("$a := $(echo 1)"),
            Some((
                "a".to_string(),
                super::AssignOp::ClampWarn,
                super::AssignRhs::Command("echo 1".to_string())
            ))
        );
        assert_eq!(
            super::parse_assignment("$a := \"$(echo 1)\""),
            Some((
                "a".to_string(),
                super::AssignOp::ClampWarn,
                super::AssignRhs::Str("$(echo 1)".to_string())
            )),
            "a quoted substitution is a literal string"
        );
        assert_eq!(super::parse_assignment("$a := $()"), None);
        assert_eq!(super::parse_assignment("$a := $(echo $(echo 1))"), None);
    }

    /// `command_needs_shell` only sees operators outside quotes.
    #[test]
    fn command_needs_shell_ignores_quoted_operators() {
        assert!(!super::command_needs_shell("/bin/echo hello"));
        assert!(super::command_needs_shell("/bin/echo a | bc"));
        assert!(super::command_needs_shell("/bin/echo a > /tmp/x"));
        assert!(!super::command_needs_shell("/bin/sh -c 'a | b > c'"));
        assert!(!super::command_needs_shell("/bin/echo 'a;b'"));
        assert!(super::command_needs_shell("/bin/echo a; b"));
    }

    /// `build_command` runs a shell only when the line needs one.
    #[test]
    fn build_command_uses_shell_only_when_needed() {
        let direct = super::build_command("/bin/echo hello").unwrap();
        assert_eq!(direct.program, "/bin/echo");
        assert_eq!(direct.args, ["hello"]);

        let shell = super::build_command("/bin/echo a | bc").unwrap();
        assert_eq!(shell.program, "sh");
        assert_eq!(shell.args, ["-c", "/bin/echo a | bc"]);
    }

    /// `resolve_action` expands references in ordinary values but parses an assignment
    /// structurally, leaving its target (and any `$` in the right-hand side) alone.
    #[test]
    fn resolve_action_expands_values_but_not_assignment_targets() {
        let variables = scene_variables();
        assert_eq!(
            super::resolve_action("/bin/echo $dir", &variables).unwrap(),
            super::Action::Command {
                command: "/bin/echo a".to_string()
            }
        );
        assert!(super::resolve_action("/bin/echo $missing", &variables).is_err());
        assert_eq!(
            super::resolve_action("$dir := 5", &variables).unwrap(),
            super::Action::Assign {
                target: super::AssignTarget::Variable("dir".to_string()),
                op: super::AssignOp::ClampWarn,
                rhs: super::AssignRhs::Int(5),
            }
        );
    }

    /// `convert_output` takes the first non-empty, trimmed line of an int command's
    /// output, clamps it per operator, and falls back to the default on a non-strict
    /// conversion failure.
    #[test]
    fn convert_output_converts_int_output() {
        let log = crate::log::Log::default();
        let conversion = super::Conversion::Int {
            min: 0,
            max: 100,
            default: 5,
        };
        assert_eq!(
            super::convert_output(
                "\n  42 \nignored\n",
                &conversion,
                super::AssignOp::ClampWarn,
                "$a",
                log
            )
            .unwrap(),
            crate::variables::VarValue::Int(42)
        );
        assert!(
            super::convert_output("200", &conversion, super::AssignOp::Strict, "$a", log).is_err(),
            "= rejects out-of-range command output"
        );
        assert_eq!(
            super::convert_output("200", &conversion, super::AssignOp::ClampWarn, "$a", log)
                .unwrap(),
            crate::variables::VarValue::Int(100)
        );
        assert_eq!(
            super::convert_output("-5", &conversion, super::AssignOp::ClampSilent, "$a", log)
                .unwrap(),
            crate::variables::VarValue::Int(0)
        );
        assert!(
            super::convert_output("high", &conversion, super::AssignOp::Strict, "$a", log).is_err(),
            "= fails when the output is not an integer"
        );
        assert_eq!(
            super::convert_output("high", &conversion, super::AssignOp::ClampWarn, "$a", log)
                .unwrap(),
            crate::variables::VarValue::Int(5),
            ":= falls back to the default on a conversion failure"
        );
        assert_eq!(
            super::convert_output("", &conversion, super::AssignOp::ClampSilent, "$a", log)
                .unwrap(),
            crate::variables::VarValue::Int(5),
            "~= falls back silently on empty output"
        );
    }

    /// `convert_output` strips trailing newlines for a str target, keeps internal ones,
    /// and applies the operator policy when truncating.
    #[test]
    fn convert_output_converts_str_output() {
        let log = crate::log::Log::default();
        let conversion = super::Conversion::Str {
            max_length: 5,
            default: "fallback".to_string(),
        };
        assert_eq!(
            super::convert_output("hello\n\n", &conversion, super::AssignOp::Strict, "$a", log)
                .unwrap(),
            crate::variables::VarValue::Str("hello".to_string())
        );
        assert_eq!(
            super::convert_output("a\nb", &conversion, super::AssignOp::Strict, "$a", log).unwrap(),
            crate::variables::VarValue::Str("a\nb".to_string())
        );
        assert!(
            super::convert_output("toolong", &conversion, super::AssignOp::Strict, "$a", log)
                .is_err(),
            "= rejects a too-long command output"
        );
        assert_eq!(
            super::convert_output(
                "toolong",
                &conversion,
                super::AssignOp::ClampWarn,
                "$a",
                log
            )
            .unwrap(),
            crate::variables::VarValue::Str("toolo".to_string())
        );
        assert_eq!(
            super::convert_output(
                "toolong",
                &conversion,
                super::AssignOp::ClampSilent,
                "$a",
                log
            )
            .unwrap(),
            crate::variables::VarValue::Str("toolo".to_string())
        );
    }

    /// `default_value` returns the fallback for both conversion kinds.
    #[test]
    fn default_value_matches_conversion_kind() {
        assert_eq!(
            super::default_value(&super::Conversion::Int {
                min: 0,
                max: 1,
                default: 3
            }),
            crate::variables::VarValue::Int(3)
        );
        assert_eq!(
            super::default_value(&super::Conversion::Str {
                max_length: 1,
                default: "x".to_string()
            }),
            crate::variables::VarValue::Str("x".to_string())
        );
    }

    /// `start_command_assignment` on an undeclared target reports the problem and never
    /// spawns, so no result event is sent.
    #[tokio::test]
    async fn start_command_assignment_errors_on_undeclared_target() {
        let variables = std::sync::Arc::new(std::sync::Mutex::new(scene_variables()));
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        super::start_command_assignment(
            super::AssignTarget::Variable("missing".to_string()),
            super::AssignOp::ClampWarn,
            "echo 1",
            &variables,
            tx,
            crate::log::Log::default(),
        );
        let received = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv()).await;
        assert!(
            matches!(received, Ok(None)),
            "an undeclared target must not spawn a command: {received:?}"
        );
    }

    /// A variable store for scene/param tests: `dir` is a string, `period` an int.
    fn scene_variables() -> crate::variables::Variables {
        let mut defs = std::collections::BTreeMap::new();
        defs.insert(
            "dir".to_string(),
            crate::variables::VarDef::string(255, "a".to_string()),
        );
        defs.insert(
            "period".to_string(),
            crate::variables::VarDef::int(1, 100, 7),
        );
        crate::variables::Variables::new(defs, &crate::press::Defaults::default())
    }

    /// `scene_operations_with` expands references in setup params from the variables,
    /// while the variable-free `scene_operations` reports a reference as an error.
    #[test]
    fn scene_operations_expand_params_from_variables() {
        let scenes = json!({
            "main": { "setup": { "1b01": { "type": "image", "params": "/tmp/$dir.png" } } }
        });
        let operations = super::scene_operations_with("main", &scenes, &scene_variables()).unwrap();
        assert_eq!(
            operations,
            vec![super::SceneOp::SetImage {
                reference: crate::baseplane::Reference::button(1, 1),
                path: "/tmp/a.png".to_string(),
                refresh_seconds: 0,
            }]
        );
        assert!(super::scene_operations("main", &scenes).is_err());
    }

    /// A `text_value` entry is the literal text itself (with references expanded), not a
    /// path, and carries its refresh.
    #[test]
    fn scene_operations_resolve_text_value_inline() {
        let scenes = json!({
            "main": { "setup": { "1b01": { "type": "text_value", "params": "dir=$dir", "refresh": 2 } } }
        });
        let operations = super::scene_operations_with("main", &scenes, &scene_variables()).unwrap();
        assert_eq!(
            operations,
            vec![super::SceneOp::TextValue {
                reference: crate::baseplane::Reference::button(1, 1),
                text: "dir=a".to_string(),
                refresh_seconds: 2,
            }]
        );
    }

    /// `resolve_scene_op` re-expands from the current values each time, so a refresh tick
    /// picks up a changed variable.
    #[test]
    fn resolve_scene_op_re_expands_after_variable_change() {
        let scenes = json!({
            "main": { "setup": { "1b01": { "type": "image", "params": "$dir/pic.png" } } }
        });
        let raw = super::raw_scene_operations("main", &scenes).unwrap();
        let mut variables = scene_variables();
        let first = super::resolve_scene_op(&raw[0], &variables).unwrap();
        variables
            .store_mut()
            .set("dir", crate::variables::VarValue::Str("b".to_string()));
        let second = super::resolve_scene_op(&raw[0], &variables).unwrap();
        assert_ne!(first, second);
    }

    /// `timer_for_scene_with` resolves an int variable's seconds, and treats a str
    /// variable (or an unresolvable key) as no timer.
    #[test]
    fn timer_seconds_resolve_from_int_variable() {
        let scenes = json!({
            "main": { "actions": { "timer": { "$period": "@Main" } } }
        });
        assert_eq!(
            super::timer_for_scene_with("main", &scenes, &scene_variables()).unwrap(),
            Some((7, vec!["@Main"]))
        );

        let mut str_defs = std::collections::BTreeMap::new();
        str_defs.insert(
            "period".to_string(),
            crate::variables::VarDef::string(5, "7".to_string()),
        );
        let str_variables =
            crate::variables::Variables::new(str_defs, &crate::press::Defaults::default());
        assert_eq!(
            super::timer_for_scene_with("main", &scenes, &str_variables).unwrap(),
            None
        );
    }

    /// Malformed assignments (no leading `$`, no operator, empty target, or a
    /// right-hand side that is not a literal or a single variable reference) are
    /// rejected.
    #[test]
    fn parse_assignment_rejects_malformed_input() {
        for value in [
            "a := 80",    // no leading $
            "$a 80",      // no operator at all
            "$ := 80",    // empty target
            "$a := ",     // empty right-hand side
            "$a := high", // unquoted non-numeric literal
            "$a := $",    // malformed reference
        ] {
            assert_eq!(super::parse_assignment(value), None, "for {value}");
        }
    }

    /// An assignment right-hand side can read a `defaults` parameter, not only a user
    /// variable: `$defaults.short_press_duration` (300ms by default) clamps into the
    /// target's range.
    #[test]
    fn apply_assignment_reads_defaults_scope_rhs() {
        let log = crate::log::Log::default();
        let mut variables = assignment_variables();
        let reference = crate::variables::VarRef {
            scope: crate::variables::Scope::Defaults,
            name: "short_press_duration".to_string(),
        };
        super::apply_assignment(
            &super::AssignTarget::Variable("count".to_string()),
            super::AssignOp::ClampWarn,
            &super::AssignRhs::Variable(reference),
            &mut variables,
            true,
            log,
        );
        assert_eq!(
            variables.store().get("count"),
            Some(&crate::variables::VarValue::Int(10)),
            "300ms clamps down to the target's max of 10"
        );
    }

    /// [`action_values`] reads a single non-empty string as a one-element list and a
    /// non-empty array of strings as-is, filtering out empty and non-string entries;
    /// anything else it cannot make sense of (a bool, number, null, or object) - which
    /// `check_action_values` already rejects at config-load time, so a real config can
    /// never actually reach this function with one - falls back to an empty list
    /// rather than panicking, exactly like an absent or empty value does.
    #[test]
    fn action_values_reads_strings_and_arrays_and_ignores_other_types() {
        assert_eq!(
            super::action_values(&Value::String("@Test".to_string())),
            ["@Test"]
        );
        assert_eq!(
            super::action_values(&Value::String(String::new())),
            Vec::<&str>::new(),
            "an explicitly empty string is treated as no action"
        );
        assert_eq!(
            super::action_values(&Value::Array(vec![
                Value::String("@A".to_string()),
                Value::String(String::new()),
                Value::Bool(true),
                Value::String("@B".to_string()),
            ])),
            ["@A", "@B"],
            "empty strings and non-string array entries are dropped"
        );
        for value in [Value::Bool(true), Value::Null, Value::from(5), json!({})] {
            assert_eq!(
                super::action_values(&value),
                Vec::<&str>::new(),
                "a value that is neither a string nor an array yields no actions: {value:?}"
            );
        }
    }
}
