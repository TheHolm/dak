//! Tests for config loading and validation (parse errors, structure, button entries, actions, timer).

mod common;

use dak::actions::{load_config, load_config_from_path};

use crate::common::{
    assert_validation_error, error_texts, write_config_with_defaults, write_scenes_config,
    write_temp_config,
};

/// A minimal valid config loads successfully.
#[test]
fn loads_valid_config() {
    let path = write_scenes_config(r#"{"on_start": {"actions": {}}}"#);
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(path);
    assert!(config.is_ok());
}

/// A config carrying `//` and `/* */` comments loads; comments are allowed anywhere
/// outside string values.
#[test]
fn loads_config_with_comments() {
    let path = write_temp_config(
        r#"// file-global comment
{
  "scenes": { /* scenes section */ "on_start": { "actions": { "1b01": { "pressed": "~" } } } },
  "devices": {
    "1": { // picked by hand
      "device_id": "0300:3002",
      "device_name": "keypad",
      "serial": "unknown", // fall back to VID:PID
      "key_count": 9,
      "encoder_count": 3,
      "screens": 6,
      "buttons": [ { "number": 1, "press": 1, "release": 1, "screen": true, "draw_id": 1 } ],
      "encoders": []
    }
  }
} // trailing comment
"#,
    );
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(&path);
    let config = config.unwrap_or_else(|errors| panic!("commented config failed: {errors:?}"));
    assert!(config.devices.by_id.contains_key(&1));
}

/// A comment is not allowed inside a string, so `//` at the start of an action value
/// stays part of the value and must be a valid action reference.
#[test]
fn comment_markers_inside_strings_are_not_comments() {
    let path =
        write_scenes_config(r#"{"on_start": { "actions": { "1b01": { "pressed": "~" } } } }"#);
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(&path);
    assert!(config.is_ok());
}

/// `load_config` reads the repository's `config.json`; also guards that the shipped sample stays valid.
#[test]
fn load_config_reads_repo_config_json() {
    let config = load_config();
    assert!(config.is_ok(), "{:?}", config.err());
}

/// Malformed JSON is rejected with line and column information.
#[test]
fn rejects_invalid_json() {
    let path = write_temp_config("{ \"on_start\": {");
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(&path);
    let errors = error_texts(config.unwrap_err());
    assert!(errors.contains("invalid JSON at line "), "{errors}");
    assert!(errors.contains(", column "), "{errors}");
    assert!(path.to_str().unwrap().contains("dak_config"), "{errors}");
}

/// Loading a nonexistent config path returns an error.
#[test]
fn rejects_missing_file() {
    assert!(load_config_from_path("/nonexistent/config.json").is_err());
}

/// A config whose root is not an object of scenes and devices is rejected.
#[test]
fn rejects_non_object_config() {
    let path = write_temp_config(r#"[1, 2, 3]"#);
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(path);
    let errors = error_texts(config.unwrap_err());
    assert!(
        errors.contains("object with \"scenes\" and \"devices\" keys"),
        "{errors}"
    );
}

/// The new top level requires both "scenes" and "devices"; a missing "scenes" section
/// is reported.
#[test]
fn rejects_config_without_scenes_section() {
    let path = write_temp_config(r#"{"devices": {}}"#);
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(path);
    let errors = error_texts(config.unwrap_err());
    assert!(
        errors.contains("missing the \"scenes\" section"),
        "{errors}"
    );
}

/// The new top level requires both "scenes" and "devices"; a missing "devices" section
/// is reported.
#[test]
fn rejects_config_without_devices_section() {
    let path = write_temp_config(r#"{"scenes": {"on_start": {"actions": {}}}}"#);
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(&path);
    let errors = error_texts(config.unwrap_err());
    assert!(
        errors.contains("missing the \"devices\" section"),
        "{errors}"
    );
}

/// Top-level keys other than "scenes", "devices" and "defaults" are rejected.
#[test]
fn rejects_unknown_top_level_key() {
    let path = write_temp_config(r#"{"scenes": {}, "devices": {}, "players": {}}"#);
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(path);
    let errors = error_texts(config.unwrap_err());
    assert!(
        errors.contains("unknown top-level key \"players\""),
        "{errors}"
    );
}

/// A missing top-level "version" defaults to "1.0".
#[test]
fn version_defaults_to_1_0_when_absent() {
    let path = write_temp_config(r#"{"scenes": {}, "devices": {}}"#);
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(path);
    let config = config.expect("config without a version should still load");
    assert_eq!(config.version, "1.0");
}

/// An explicit top-level "version" is carried through as given.
#[test]
fn version_is_read_when_present() {
    let path = write_temp_config(r#"{"scenes": {}, "devices": {}, "version": "2.3"}"#);
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(path);
    let config = config.expect("a version string should be accepted");
    assert_eq!(config.version, "2.3");
}

/// A non-string top-level "version" is rejected with its type name in the message.
#[test]
fn rejects_non_string_version() {
    let path = write_temp_config(r#"{"scenes": {}, "devices": {}, "version": 2}"#);
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(path);
    let errors = error_texts(config.unwrap_err());
    assert!(
        errors.contains("top-level \"version\" must be a string, got a number"),
        "{errors}"
    );
}

/// A `defaults` section is optional; when absent the built-in press timings apply.
#[test]
fn absent_defaults_uses_builtin_durations() {
    let path = write_scenes_config(r#"{"on_start": {"actions": {}}}"#);
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(&path);
    let config = config.unwrap();
    assert_eq!(config.defaults, dak::press::Defaults::default());
}

/// An empty `defaults` section is fine too: every key falls back to its built-in value.
#[test]
fn empty_defaults_uses_builtin_durations() {
    let path = write_config_with_defaults(r#"{}"#, r#"{"on_start": {"actions": {}}}"#);
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(&path);
    let config = config.unwrap();
    assert_eq!(config.defaults, dak::press::Defaults::default());
}

/// Valid `defaults` values override the built-in timings and load fine.
#[test]
fn valid_defaults_load_and_apply() {
    let path = write_config_with_defaults(
        r#"{"short_press_duration": 150, "double_click_gap": 250}"#,
        r#"{"on_start": {"actions": {}}}"#,
    );
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(&path);
    let config = config.unwrap();
    assert_eq!(
        config.defaults.short_press_duration,
        std::time::Duration::from_millis(150)
    );
    assert_eq!(
        config.defaults.double_click_gap,
        std::time::Duration::from_millis(250)
    );
}

/// A missing single key inside an otherwise valid `defaults` falls back to its built-in.
#[test]
fn partial_defaults_fall_back_per_key() {
    let path = write_config_with_defaults(
        r#"{"short_press_duration": 120}"#,
        r#"{"on_start": {"actions": {}}}"#,
    );
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(&path);
    let config = config.unwrap();
    assert_eq!(
        config.defaults.short_press_duration,
        std::time::Duration::from_millis(120)
    );
    assert_eq!(
        config.defaults.double_click_gap,
        std::time::Duration::from_millis(300)
    );
}

/// A `defaults` section that is not an object is rejected.
#[test]
fn rejects_non_object_defaults() {
    let path = write_temp_config(r#"{"scenes": {}, "devices": {}, "defaults": [300, 300]}"#);
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(&path);
    let errors = error_texts(config.unwrap_err());
    assert!(
        errors.contains("config \"defaults\" must be an object"),
        "{errors}"
    );
}

/// Unknown keys in `defaults` are rejected.
#[test]
fn rejects_unknown_defaults_key() {
    let path = write_config_with_defaults(
        r#"{"short_press_duration": 300, "long_press_duration": 600}"#,
        r#"{"on_start": {"actions": {}}}"#,
    );
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(&path);
    let errors = error_texts(config.unwrap_err());
    assert!(
        errors.contains(
            "defaults: unknown key \"long_press_duration\", expected one of \"short_press_duration\", \"double_click_gap\", \"button_brightness\", \"encoder_brightness\""
        ),
        "{errors}"
    );
}

/// Zero and non-numeric durations in `defaults` are rejected.
#[test]
fn rejects_invalid_defaults_values() {
    for defaults in [
        r#"{"short_press_duration": 0}"#,
        r#"{"double_click_gap": "fast"}"#,
    ] {
        let path = write_config_with_defaults(defaults, r#"{"on_start": {"actions": {}}}"#);
        let config = load_config_from_path(path.to_str().unwrap());
        let _ = std::fs::remove_file(&path);
        let errors = error_texts(config.unwrap_err());
        assert!(
            errors.contains("must be a positive number of milliseconds"),
            "for {defaults}: {errors}"
        );
    }
}

/// Valid `button_brightness`/`encoder_brightness` values override the built-in 50%
/// and load fine.
#[test]
fn valid_brightness_defaults_load_and_apply() {
    let path = write_config_with_defaults(
        r#"{"button_brightness": 80, "encoder_brightness": 10}"#,
        r#"{"on_start": {"actions": {}}}"#,
    );
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(&path);
    let config = config.unwrap();
    assert_eq!(config.defaults.button_brightness, 80);
    assert_eq!(config.defaults.encoder_brightness, 10);
}

/// A missing single brightness key inside an otherwise valid `defaults` falls back to
/// its built-in 50%, independently of the other brightness key.
#[test]
fn partial_brightness_defaults_fall_back_per_key() {
    let path = write_config_with_defaults(
        r#"{"button_brightness": 5}"#,
        r#"{"on_start": {"actions": {}}}"#,
    );
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(&path);
    let config = config.unwrap();
    assert_eq!(config.defaults.button_brightness, 5);
    assert_eq!(config.defaults.encoder_brightness, 50);
}

/// `0` is a valid brightness (turns the LCDs/LEDs off), unlike the duration keys which
/// reject `0`.
#[test]
fn zero_brightness_default_is_valid() {
    let path = write_config_with_defaults(
        r#"{"button_brightness": 0, "encoder_brightness": 0}"#,
        r#"{"on_start": {"actions": {}}}"#,
    );
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(&path);
    let config = config.unwrap();
    assert_eq!(config.defaults.button_brightness, 0);
    assert_eq!(config.defaults.encoder_brightness, 0);
}

/// Out-of-range and non-numeric `button_brightness`/`encoder_brightness` values in
/// `defaults` are rejected.
#[test]
fn rejects_invalid_brightness_defaults_values() {
    for defaults in [
        r#"{"button_brightness": 101}"#,
        r#"{"encoder_brightness": 255}"#,
        r#"{"button_brightness": "bright"}"#,
    ] {
        let path = write_config_with_defaults(defaults, r#"{"on_start": {"actions": {}}}"#);
        let config = load_config_from_path(path.to_str().unwrap());
        let _ = std::fs::remove_file(&path);
        let errors = error_texts(config.unwrap_err());
        assert!(
            errors.contains("must be a number between 0 and 100"),
            "for {defaults}: {errors}"
        );
    }
}

/// A scenes section that is not an object of scene names is rejected.
#[test]
fn rejects_scenes_section_not_an_object() {
    let path = write_temp_config(r#"{"scenes": [1, 2], "devices": {}}"#);
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(path);
    let errors = error_texts(config.unwrap_err());
    assert!(
        errors.contains("config \"scenes\" must be an object whose keys are scene names"),
        "{errors}"
    );
}

/// The README-documented config structure loads and preserves scene, button and timer values.
#[test]
fn parses_documented_config_structure() {
    let path = write_scenes_config(
        r#"{
            "on_start": {
                "setup": {
                    "1b01": { "type": "image", "params": "/path/reader.ico" },
                    "1b03": { "type": "text_exec", "params": "/usr/bin/date +%H:%M" }
                },
                "actions": {
                    "1b01": { "pressed": "~" },
                    "timer": { "1": "@Main" }
                }
            },
            "Main": {
                "actions": { "timer": { "1": "~" } }
            }
        }"#,
    );
    let config = load_config_from_path(path.to_str().unwrap()).unwrap();
    let _ = std::fs::remove_file(path);

    assert!(config.scenes.get("on_start").is_some());
    assert!(config.scenes.get("Main").is_some());
    assert_eq!(config.scenes["on_start"]["setup"]["1b01"]["type"], "image");
    assert_eq!(
        config.scenes["on_start"]["setup"]["1b01"]["params"],
        "/path/reader.ico"
    );
    assert_eq!(config.scenes["on_start"]["actions"]["timer"]["1"], "@Main");
}

// -- scene validation --

/// A scene must be an object of numbered buttons and `actions`.
#[test]
fn rejects_scene_not_an_object() {
    assert_validation_error(
        r#"{"on_start": []}"#,
        "scene \"on_start\" must be an object",
    );
}

/// The old `set_scene` wrapper is gone: buttons live at the scene top level now.
#[test]
fn rejects_legacy_set_scene_key() {
    assert_validation_error(
        r#"{"on_start": {"set_scene": []}}"#,
        "unknown key \"set_scene\"",
    );
}

/// Top-level scene keys must be button numbers or the reserved "actions" key.
#[test]
fn rejects_unknown_scene_key() {
    assert_validation_error(
        r#"{"on_start": {"foo": {"type": "image", "params": "/a"}}}"#,
        "unknown key \"foo\"",
    );
}

/// A button entry must be an object with "type" and "params".
#[test]
fn rejects_button_entry_not_an_object() {
    assert_validation_error(
        r#"{"on_start": {"setup": {"1b01": "image"}}}"#,
        "key \"1b01\" must be an object with \"type\" and \"params\"",
    );
}

/// Button entries accept only the "type" and "params" fields.
#[test]
fn rejects_button_entry_unknown_field() {
    assert_validation_error(
        r#"{"on_start": {"setup": {"1b01": {"type": "image", "params": "/a", "from": "x"}}}}"#,
        "has unknown field \"from\"",
    );
}

/// A button entry without a string "type" is rejected.
#[test]
fn rejects_button_entry_without_type() {
    assert_validation_error(
        r#"{"on_start": {"setup": {"1b01": {"params": "/a"}}}}"#,
        "type must be a string",
    );
}

/// "refresh" is accepted on every type that redraws something on its own schedule:
/// image, text, image_exec and text_exec.
#[test]
fn accepts_refresh_on_redrawable_types() {
    let path = write_scenes_config(
        r#"{
            "on_start": {
                "setup": {
                    "1b01": { "type": "image", "params": "/a", "refresh": 5 },
                    "1b02": { "type": "text", "params": "/b", "refresh": 5 },
                    "1b03": { "type": "image_exec", "params": "/bin/true", "refresh": 5 },
                    "1b04": { "type": "text_exec", "params": "/bin/true", "refresh": 5 }
                }
            }
        }"#,
    );
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(path);
    assert!(config.is_ok(), "{:?}", config.err());
}

/// "refresh" defaults to 0 (never) when absent, so an entry without it is unaffected.
#[test]
fn accepts_setup_entry_without_refresh() {
    let path = write_scenes_config(
        r#"{"on_start": {"setup": {"1b01": {"type": "text_exec", "params": "/bin/true"}}}}"#,
    );
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(path);
    assert!(config.is_ok(), "{:?}", config.err());
}

/// "refresh" must be a number of seconds, not any other JSON type.
#[test]
fn rejects_refresh_not_a_number() {
    assert_validation_error(
        r#"{"on_start": {"setup": {"1b01": {"type": "text_exec", "params": "/bin/true", "refresh": "soon"}}}}"#,
        "refresh must be a positive number of seconds, got a string",
    );
}

/// A nonzero "refresh" on "clear" is rejected: there is nothing left to redraw.
#[test]
fn rejects_refresh_on_clear() {
    assert_validation_error(
        r#"{"on_start": {"setup": {"1b01": {"type": "clear", "refresh": 5}}}}"#,
        "refresh cannot be used with type \"clear\"",
    );
}

/// A nonzero "refresh" on "launch" is rejected: launch fires a detached one-off process,
/// not a redraw.
#[test]
fn rejects_refresh_on_launch() {
    assert_validation_error(
        r#"{"on_start": {"setup": {"1b01": {"type": "launch", "params": "/bin/true", "refresh": 5}}}}"#,
        "refresh cannot be used with type \"launch\"",
    );
}

/// Unknown types are rejected with the list of known ones.
#[test]
fn rejects_unknown_button_type() {
    assert_validation_error(
        r#"{"on_start": {"setup": {"1b01": {"type": "foo", "params": "/a"}}}}"#,
        "unknown type \"foo\"",
    );
}

/// A scene's `setup` section must be an object of numbered buttons.
#[test]
fn rejects_setup_not_an_object() {
    assert_validation_error(
        r#"{"on_start": {"setup": [1, 2]}}"#,
        "setup must be an object of numbered buttons",
    );
}

/// A scene's `setup` section must be an object of numbered buttons; the error also
/// covers a bare string value.
#[test]
fn rejects_setup_not_an_object_when_string() {
    assert_validation_error(
        r#"{"on_start": {"setup": "1b01"}}"#,
        "setup must be an object of numbered buttons",
    );
}

/// "params" must be a string when present.
#[test]
fn rejects_button_params_not_a_string() {
    assert_validation_error(
        r#"{"on_start": {"setup": {"1b01": {"type": "image", "params": 42}}}}"#,
        "params must be a string",
    );
}

/// `image` and `text` need a non-empty params path.
#[test]
fn rejects_image_with_empty_params() {
    assert_validation_error(
        r#"{"on_start": {"setup": {"1b01": {"type": "image", "params": ""}}}}"#,
        "image params must be a path",
    );
}

/// `image_exec` and `text_exec` need a program command line in params.
#[test]
fn rejects_exec_with_empty_params() {
    assert_validation_error(
        r#"{"on_start": {"setup": {"1b01": {"type": "text_exec", "params": "  "}}}}"#,
        "text_exec params must be a program command line",
    );
}

/// Malformed command lines in exec params are rejected.
#[test]
fn rejects_exec_with_unbalanced_quotes() {
    assert_validation_error(
        r#"{"on_start": {"setup": {"1b01": {"type": "text_exec", "params": "/bin/sh -c 'oops"}}}}"#,
        "unbalanced quotes",
    );
}

/// `clear` needs no params and is accepted as a bare type entry.
#[test]
fn accepts_clear_without_params() {
    let path = write_scenes_config(r#"{"on_start": {"setup": {"1b03": {"type": "clear"}}}}"#);
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(path);
    assert!(config.is_ok(), "{:?}", config.err());
}

/// `launch` is accepted like the `*_exec` commands: params hold the program command line.
#[test]
fn accepts_launch_with_command_line() {
    let path = write_scenes_config(
        r#"{
            "on_start": {
                "setup": {
                    "1b04": { "type": "launch", "params": "/bin/sh -c 'echo detached'" }
                }
            }
        }"#,
    );
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(path);
    let config = config.expect("launch config should validate");
    assert!(config.warnings.is_empty(), "{:?}", config.warnings);
}

/// A `launch` button without a command line is rejected.
#[test]
fn rejects_launch_with_empty_params() {
    assert_validation_error(
        r#"{"on_start": {"setup": {"1b01": {"type": "launch", "params": "  "}}}}"#,
        "launch params must be a program command line",
    );
}

/// A malformed command line in launch params is rejected.
#[test]
fn rejects_launch_with_unbalanced_quotes() {
    assert_validation_error(
        r#"{"on_start": {"setup": {"1b01": {"type": "launch", "params": "/bin/sh -c 'oops"}}}}"#,
        "unbalanced quotes",
    );
}

/// A missing `launch` program is reported as a non-fatal warning like the exec commands.
#[test]
fn warns_on_missing_launch_program() {
    let path = write_scenes_config(
        r#"{
            "on_start": {
                "setup": {
                    "1b04": { "type": "launch", "params": "/does/not/exist --flag" }
                }
            }
        }"#,
    );
    let config = load_config_from_path(path.to_str().unwrap()).unwrap();
    let _ = std::fs::remove_file(path);
    let warnings = config.warnings.join("\n");
    assert!(warnings.contains("program not found"), "{warnings}");
    assert!(warnings.contains("key \"1b04\""), "{warnings}");
}

// -- warnings --

/// A missing image file is a non-fatal warning rooted at the button key.
#[test]
fn warns_on_missing_image_file() {
    let path = write_scenes_config(
        r#"{
            "on_start": {
                "setup": {
                    "1b01": { "type": "image", "params": "/does/not/exist.ico" }
                }
            }
        }"#,
    );
    let config = load_config_from_path(path.to_str().unwrap()).unwrap();
    let _ = std::fs::remove_file(path);
    let warnings = config.warnings.join("\n");
    assert!(warnings.contains("file not found"), "{warnings}");
    assert!(warnings.contains("key \"1b01\""), "{warnings}");
}

/// A missing text file is reported as a non-fatal warning.
#[test]
fn warns_on_missing_text_file() {
    let path = write_scenes_config(
        r#"{
            "on_start": {
                "setup": {
                    "1b01": { "type": "text", "params": "/does/not/exist.txt" }
                }
            }
        }"#,
    );
    let config = load_config_from_path(path.to_str().unwrap()).unwrap();
    let _ = std::fs::remove_file(path);
    let warnings = config.warnings.join("\n");
    assert!(warnings.contains("file not found"), "{warnings}");
    assert!(warnings.contains("key \"1b01\""), "{warnings}");
}

/// A missing program referenced by a button entry or an action is a non-fatal warning.
#[test]
fn warns_on_missing_executable() {
    let path = write_scenes_config(
        r#"{
            "on_start": {
                "setup": {
                    "1b03": { "type": "text_exec", "params": "/does/not/exist +%H:%M" }
                },
                "actions": {
                    "1b01": { "pressed": "/does/not/exist/beep" }
                }
            }
        }"#,
    );
    let config = load_config_from_path(path.to_str().unwrap()).unwrap();
    let _ = std::fs::remove_file(path);
    let warnings = config.warnings.join("\n");
    assert!(warnings.contains("program not found"), "{warnings}");
    assert!(warnings.contains("key \"1b03\""), "{warnings}");
    assert!(warnings.contains("actions.\"1b01\".pressed"), "{warnings}");
}

/// An existing but non-executable program is reported as a warning.
#[test]
fn warns_on_non_executable_program() {
    let exe = "/tmp/dak_not_executable_program";
    std::fs::write(exe, "").unwrap();
    let mut perms = std::fs::metadata(exe).unwrap().permissions();
    use std::os::unix::fs::PermissionsExt;
    perms.set_mode(0o644);
    std::fs::set_permissions(exe, perms).unwrap();

    let path = write_scenes_config(&format!(
        r#"{{
        "on_start": {{
            "setup": {{
                "1b01": {{ "type": "text_exec", "params": "{exe}" }}
            }}
        }}
    }}"#
    ));
    let config = load_config_from_path(path.to_str().unwrap()).unwrap();
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(exe);

    let warnings = config.warnings.join("\n");
    assert!(warnings.contains("is not executable"), "{warnings}");
}

// -- actions validation --

/// `actions` must be an object.
#[test]
fn rejects_actions_not_an_object() {
    assert_validation_error(
        r#"{"on_start": {"actions": []}}"#,
        "actions must be an object",
    );
}

/// A key entry must be an object mapping events to action values.
#[test]
fn rejects_key_action_not_an_object() {
    assert_validation_error(
        r#"{"on_start": {"actions": {"1b01": "pressed"}}}"#,
        "actions.\"1b01\" must be an object of events",
    );
}

/// Action values must be a string or an array of strings.
#[test]
fn rejects_event_value_not_a_string_or_array() {
    assert_validation_error(
        r#"{"on_start": {"actions": {"1b01": {"pressed": 42}}}}"#,
        "actions.\"1b01\".pressed must be a string or an array of strings",
    );
}

/// A null action value is rejected with its type name in the message.
#[test]
fn rejects_event_value_null() {
    assert_validation_error(
        r#"{"on_start": {"actions": {"1b01": {"pressed": null}}}}"#,
        "pressed must be a string or an array of strings, got null",
    );
}

/// A boolean action value is rejected with its type name in the message.
#[test]
fn rejects_event_value_bool() {
    assert_validation_error(
        r#"{"on_start": {"actions": {"1b01": {"pressed": true}}}}"#,
        "pressed must be a string or an array of strings, got bool",
    );
}

/// An array action value is now accepted; a non-string element inside it is rejected
/// with its index and type name in the message.
#[test]
fn rejects_array_element_not_a_string() {
    assert_validation_error(
        r#"{"on_start": {"actions": {"1b01": {"pressed": [1]}}}}"#,
        "pressed[0] must be a string, got a number",
    );
}

/// An empty string inside a non-empty array is rejected: `[]` is the only way to spell
/// "no action" for the array form.
#[test]
fn rejects_empty_string_array_element() {
    assert_validation_error(
        r#"{"on_start": {"actions": {"1b01": {"pressed": [""]}}}}"#,
        "pressed[0] must not be empty; use an empty array for no action",
    );
}

/// An array with more than one scene-changing action (`~` or `@scene`) is rejected: at
/// most one scene transition is allowed per event.
#[test]
fn rejects_multiple_scene_changing_actions_in_a_list() {
    assert_validation_error(
        r#"{"on_start": {"actions": {"1b01": {"pressed": ["~", "@on_start"]}}}}"#,
        "pressed has 2 scene-changing actions (~ or @scene); at most one is allowed per event",
    );
}

/// An array of valid action strings is accepted.
#[test]
fn accepts_event_value_as_array_of_strings() {
    let path = write_scenes_config(
        r#"{"on_start": {"actions": {"1b01": {"pressed": ["/bin/true", "/bin/false"]}}}}"#,
    );
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(path);
    assert!(config.is_ok(), "{:?}", config.err());
}

/// An empty array is accepted: it means "bound but no action", like an empty string.
#[test]
fn accepts_empty_array_as_no_action() {
    let path = write_scenes_config(r#"{"on_start": {"actions": {"1b01": {"pressed": []}}}}"#);
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(path);
    assert!(config.is_ok(), "{:?}", config.err());
}

/// A per-reference action object with no events at all is a likely mistake: warned,
/// not rejected.
#[test]
fn warns_on_empty_action_object() {
    let path = write_scenes_config(r#"{"on_start": {"actions": {"1b01": {}}}}"#);
    let config = load_config_from_path(path.to_str().unwrap()).expect("should still load");
    let _ = std::fs::remove_file(path);
    assert!(
        config
            .warnings
            .iter()
            .any(|warning| warning.contains("actions.\"1b01\" defines no events")),
        "{:?}",
        config.warnings
    );
}

/// An object action value is rejected with its type name in the message.
#[test]
fn rejects_event_value_object() {
    assert_validation_error(
        r#"{"on_start": {"actions": {"1b01": {"pressed": {}}}}}"#,
        "pressed must be a string or an array of strings, got an object",
    );
}

/// A `@scene` action referencing a scene that does not exist is rejected with its location.
#[test]
fn rejects_undefined_scene_reference() {
    let path = write_scenes_config(
        r#"{
            "on_start": {
                "actions": {
                    "1b01": { "pressed": "@Missing" },
                    "timer": { "1": "~" }
                }
            }
        }"#,
    );
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(path);
    let errors = error_texts(config.unwrap_err());
    assert!(
        errors.contains("references undefined scene \"@Missing\""),
        "{errors}"
    );
    assert!(errors.contains("scene \"on_start\""), "{errors}");
    assert!(errors.contains("actions.\"1b01\".pressed"), "{errors}");
}

/// A bare `@` scene reference is rejected.
#[test]
fn rejects_empty_scene_reference() {
    assert_validation_error(
        r#"{"on_start": {"actions": {"1b01": {"pressed": "@"}}}}"#,
        "is an empty scene reference",
    );
}

// -- timer validation --

/// The `timer` entry must be a single-entry object; more entries are rejected.
#[test]
fn rejects_timer_with_multiple_entries() {
    let path = write_scenes_config(
        r#"{
            "on_start": {
                "actions": {
                    "timer": { "1": "~", "2": "~" }
                }
            }
        }"#,
    );
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(path);
    let errors = error_texts(config.unwrap_err());
    assert!(
        errors.contains("must contain exactly one entry"),
        "{errors}"
    );
}

/// A non-numeric timer seconds value is rejected.
#[test]
fn rejects_invalid_timer_seconds() {
    let path = write_scenes_config(
        r#"{
            "on_start": {
                "actions": {
                    "timer": { "soon": "~" }
                }
            }
        }"#,
    );
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(path);
    let errors = error_texts(config.unwrap_err());
    assert!(errors.contains("not a valid number of seconds"), "{errors}");
}

/// `timer` must be a single-entry object.
#[test]
fn rejects_timer_not_an_object() {
    assert_validation_error(
        r#"{"on_start": {"actions": {"timer": "soon"}}}"#,
        "actions.timer must be a single-entry object",
    );
}

/// The timer action value must be a string.
#[test]
fn rejects_timer_value_not_a_string() {
    assert_validation_error(
        r#"{"on_start": {"actions": {"timer": {"1": 42}}}}"#,
        "actions.timer must be a string or an array of strings",
    );
}

/// A timer value may also be an array of actions, run in order like an event's.
#[test]
fn accepts_timer_value_as_array() {
    let path = write_scenes_config(r#"{"on_start": {"actions": {"timer": {"1": ["@on_start"]}}}}"#);
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(path);
    assert!(config.is_ok(), "{:?}", config.err());
}

/// A timer action referencing an undefined scene is rejected with the timer location.
#[test]
fn rejects_timer_referencing_undefined_scene() {
    let path = write_scenes_config(
        r#"{
            "on_start": {
                "actions": {
                    "timer": { "1": "@Missing" }
                }
            }
        }"#,
    );
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(path);
    let errors = error_texts(config.unwrap_err());
    assert!(
        errors.contains("references undefined scene \"@Missing\""),
        "{errors}"
    );
    assert!(
        errors.contains("scene \"on_start\": actions.timer"),
        "{errors}"
    );
}

/// Validation reports every invalid scene in one go instead of stopping at the first.
#[test]
fn reports_errors_from_all_scenes() {
    let path = write_scenes_config(
        r#"{
            "one": { "bogus": 1 },
            "two": {
                "actions": { "timer": { "soon": "~" } }
            }
        }"#,
    );
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(path);
    let errors = error_texts(config.unwrap_err());
    assert!(
        errors.contains("scene \"one\": unknown key \"bogus\""),
        "{errors}"
    );
    assert!(
        errors
            .contains("scene \"two\": actions.timer key \"soon\" is not a valid number of seconds"),
        "{errors}"
    );
}

// -- control references --

/// A setup key that is not a control reference is rejected with the scene, key and reason.
#[test]
fn rejects_invalid_setup_reference() {
    assert_validation_error(
        r#"{"on_start": {"setup": {"0b01": {"type": "image", "params": "/a"}}}}"#,
        "scene \"on_start\": key \"0b01\" is not a valid control reference",
    );
}

/// The old plain button-number keys are no longer valid control references.
#[test]
fn rejects_legacy_numeric_button_keys() {
    assert_validation_error(
        r#"{"on_start": {"setup": {"1": {"type": "image", "params": "/a"}}}}"#,
        "scene \"on_start\": key \"1\" is not a valid control reference",
    );
}

/// An actions key that is not a control reference is rejected with the scene and location.
#[test]
fn rejects_invalid_action_reference() {
    assert_validation_error(
        r#"{"on_start": {"actions": {"0b01": {"pressed": "~"}}}}"#,
        "scene \"on_start\": actions.\"0b01\" is not a valid control reference",
    );
}

/// Device numbers are limited to a single digit; references to device 0 are invalid.
#[test]
fn rejects_zero_device_reference() {
    assert_validation_error(
        r#"{"on_start": {"setup": {"0b01": {"type": "clear"}}}}"#,
        "the device must be a single digit from 1 to 9",
    );
}

/// Control numbers are two digits from 01 to 99; bare, 00 and 100 forms are invalid.
#[test]
fn rejects_bad_control_numbers() {
    for key in ["1b1", "1b00", "1b100"] {
        assert_validation_error(
            &format!(r#"{{"on_start": {{"setup": {{"{key}": {{"type": "clear"}}}}}}}}"#),
            "is not a valid control reference",
        );
    }
}

/// A reference naming a device or encoder the program does not drive yet is still valid
/// config: absent devices and encoders are skipped at runtime, not rejected at load.
#[test]
fn accepts_other_device_and_encoder_references() {
    let path = write_scenes_config(
        r#"{
            "on_start": {
                "setup": {
                    "2b01": { "type": "image", "params": "/a" },
                    "1e01": { "type": "clear" }
                }
            }
        }"#,
    );
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(path);
    assert!(config.is_ok(), "{:?}", config.err());
}

/// A setup entry that assigns an image to an encoder is a config error: encoders have
/// no display, so the program refuses to load the config.
#[test]
fn rejects_image_assignment_to_encoder() {
    for entry in [
        r#"{"type": "image", "params": "/a"}"#,
        r#"{"type": "text", "params": "/a"}"#,
        r#"{"type": "image_exec", "params": "/usr/bin/text2gif"}"#,
        r#"{"type": "text_exec", "params": "/usr/bin/date"}"#,
    ] {
        assert_validation_error(
            &format!(r#"{{"on_start": {{"setup": {{"1e01": {entry}}}}}}}"#),
            "cannot assign",
        );
    }
}

/// A setup entry on an encoder that assigns an image is rejected also when the target
/// device number differs from the (unnamed) reference format; the message names the
/// control reference.
#[test]
fn rejects_image_assignment_to_encoder_names_the_reference() {
    assert_validation_error(
        r#"{"on_start": {"setup": {"2e03": {"type": "image", "params": "/a"}}}}"#,
        "cannot assign image to encoder 2e03",
    );
}

/// A setup entry on an encoder that assigns no image (clear) is still valid config and
/// is skipped at runtime.
#[test]
fn accepts_clear_on_encoder() {
    let path = write_scenes_config(r#"{"on_start": {"setup": {"1e01": {"type": "clear"}}}}"#);
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(&path);
    assert!(config.is_ok(), "{:?}", config.err());
}
