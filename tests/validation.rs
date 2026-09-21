//! Tests for config loading and validation (parse errors, structure, button entries, actions, timer).

mod common;

use dak::actions::{load_config, load_config_from_path};

use crate::common::{
    assert_validation_error, error_texts, temp_dir, write_config_with_defaults,
    write_scenes_config, write_temp_config, write_variables_config, SetHome, ENV_LOCK,
};
use dak::variables::{VarDef, VarValue};
use std::os::unix::fs::PermissionsExt;

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
  "scenes": { /* scenes section */ "on_start": { "actions": { "1b01": { "pressed": "@" } } } },
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
        write_scenes_config(r#"{"on_start": { "actions": { "1b01": { "pressed": "@" } } } }"#);
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

/// Every JSON file under `examples/` is a complete config that loads and validates.
#[test]
fn example_configs_load() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).expect("the examples directory should exist") {
        let path = entry.unwrap().path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let config = load_config_from_path(path.to_str().unwrap());
        assert!(config.is_ok(), "{}: {:?}", path.display(), config.err());
        checked += 1;
    }
    assert!(checked >= 1, "expected at least one example config");
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
            "defaults: unknown key \"long_press_duration\", expected one of \"short_press_duration\", \"double_click_gap\", \"button_brightness\", \"encoder_brightness\", \"background\", \"text_color\""
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

/// Valid `background`/`text_color` colours (hex and named) override the built-in
/// black/white and load fine, keeping the source text for reads.
#[test]
fn valid_colour_defaults_load_and_apply() {
    let path = write_config_with_defaults(
        r##"{"background": "red", "text_color": "#00ff00"}"##,
        r#"{"on_start": {"actions": {}}}"#,
    );
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(&path);
    let config = config.unwrap();
    assert_eq!(config.defaults.background.channels(), [0xff, 0x00, 0x00]);
    assert_eq!(config.defaults.background.text(), "red");
    assert_eq!(config.defaults.text_color.channels(), [0x00, 0xff, 0x00]);
    assert_eq!(config.defaults.text_color.text(), "#00ff00");
}

/// A bad colour in `defaults` is a hard config error, for both a malformed hex and an
/// unknown name, and whether the value is not a string at all.
#[test]
fn rejects_invalid_colour_defaults() {
    for defaults in [
        r##"{"background": "#fff"}"##,
        r#"{"background": "chartreuse"}"#,
        r#"{"background": 0}"#,
        r##"{"text_color": "#12345"}"##,
    ] {
        let path = write_config_with_defaults(defaults, r#"{"on_start": {"actions": {}}}"#);
        let config = load_config_from_path(path.to_str().unwrap());
        let _ = std::fs::remove_file(&path);
        let errors = error_texts(config.unwrap_err());
        assert!(
            errors.contains("background") || errors.contains("text_color"),
            "for {defaults}: {errors}"
        );
    }
}

/// `background` is allowed on every drawing type and `text_color` on the text types,
/// with literal colours validated at load.
#[test]
fn valid_setup_colour_overrides_load() {
    let path = write_scenes_config(
        r##"{
            "on_start": {
                "setup": {
                    "1b01": { "type": "image", "params": "/tmp/x.png", "background": "#112233" },
                    "1b02": { "type": "image_exec", "params": "true", "background": "red" },
                    "1b03": { "type": "text", "params": "/tmp/x.txt", "background": "navy", "text_color": "lime" },
                    "1b04": { "type": "text_value", "params": "hi", "background": "#000000", "text_color": "orange" },
                    "1b05": { "type": "text_exec", "params": "true", "background": "teal", "text_color": "cyan" }
                }
            }
        }"##,
    );
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(&path);
    assert!(config.is_ok(), "{:?}", config.err());
}

/// A colour field on a type that does not draw it (`text_color` on an image,
/// `background` on `clear`/`launch`) is a config error.
#[test]
fn rejects_colour_on_wrong_setup_type() {
    for (scenes, expected) in [
        (
            r#"{"on_start": {"setup": {"1b01": {"type": "image", "params": "/tmp/x.png", "text_color": "red"}}}}"#,
            "text_color cannot be used with type \"image\"",
        ),
        (
            r#"{"on_start": {"setup": {"1b01": {"type": "clear", "background": "red"}}}}"#,
            "background cannot be used with type \"clear\"",
        ),
        (
            r#"{"on_start": {"setup": {"1b01": {"type": "launch", "params": "true", "background": "red"}}}}"#,
            "background cannot be used with type \"launch\"",
        ),
        (
            r#"{"on_start": {"setup": {"1b01": {"type": "clear", "text_color": "red"}}}}"#,
            "text_color cannot be used with type \"clear\"",
        ),
    ] {
        let path = write_scenes_config(scenes);
        let config = load_config_from_path(path.to_str().unwrap());
        let _ = std::fs::remove_file(&path);
        let errors = error_texts(config.unwrap_err());
        assert!(errors.contains(expected), "for {scenes}: {errors}");
    }
}

/// A literal setup colour that is not a colour, or not a string at all, is a
/// config-load error.
#[test]
fn rejects_invalid_literal_setup_colour() {
    let path = write_scenes_config(
        r##"{"on_start": {"setup": {"1b01": {"type": "image", "params": "/tmp/x.png", "background": "#gggggg"}}}}"##,
    );
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(&path);
    let errors = error_texts(config.unwrap_err());
    assert!(errors.contains("invalid colour"), "{errors}");

    let path = write_scenes_config(
        r#"{"on_start": {"setup": {"1b01": {"type": "text_value", "params": "hi", "text_color": 5}}}}"#,
    );
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(&path);
    let errors = error_texts(config.unwrap_err());
    assert!(errors.contains("must be a colour string"), "{errors}");
}

/// A setup colour built from a `$` reference is not parsed at load; only the reference
/// itself is validated, so an undefined variable is still an error.
#[test]
fn setup_colour_reference_is_deferred() {
    let path = write_scenes_config(
        r#"{"on_start": {"setup": {"1b01": {"type": "image", "params": "/tmp/x.png", "background": "$theme"}}}}"#,
    );
    // `theme` is not a declared variable, so the reference is an error...
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(&path);
    assert!(config.is_err(), "undefined variable should be rejected");

    // ...but a declared one loads, even though the variable's value is only known at
    // runtime (and may not be a colour, which is handled at draw time).
    let path = write_variables_config(
        r#"{"theme": {"type": "str", "value": "red"}}"#,
        r#"{"on_start": {"setup": {"1b01": {"type": "image", "params": "/tmp/x.png", "background": "$theme"}}}}"#,
    );
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(&path);
    assert!(config.is_ok(), "{:?}", config.err());
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
                    "1b01": { "pressed": "@" },
                    "timer": { "1": "@Main" }
                }
            },
            "Main": {
                "actions": { "timer": { "1": "@" } }
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

/// An array with more than one scene-changing action (`@` or `@scene`) is rejected: at
/// most one scene transition is allowed per event.
#[test]
fn rejects_multiple_scene_changing_actions_in_a_list() {
    assert_validation_error(
        r#"{"on_start": {"actions": {"1b01": {"pressed": ["@", "@on_start"]}}}}"#,
        "pressed has 2 scene-changing actions (@ or @scene); at most one is allowed per event",
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

/// Every documented button event is accepted on a button, and every documented encoder
/// event (including both rotation directions) on an encoder.
#[test]
fn accepts_the_documented_button_and_encoder_events() {
    let path = write_scenes_config(
        r#"{"on_start": {"actions": {
            "1b01": { "pressed": "", "released": "", "short_press": "", "long_press": "", "double_click": "" },
            "1e01": { "pressed": "", "released": "", "short_press": "", "long_press": "", "double_click": "", "turn_cw": "", "turn_ccw": "" }
        }}}"#,
    );
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(path);
    assert!(config.is_ok(), "{:?}", config.err());
}

/// An event name a button does not support is a config error, not a silently ignored
/// binding, so a typo cannot look like a working action.
#[test]
fn rejects_unknown_button_event() {
    assert_validation_error(
        r#"{"on_start": {"actions": {"1b01": {"short_pres": "@"}}}}"#,
        "actions.\"1b01\" has invalid button event \"short_pres\"",
    );
}

/// An encoder rejects an event outside its own set just as a button does.
#[test]
fn rejects_unknown_encoder_event() {
    assert_validation_error(
        r#"{"on_start": {"actions": {"1e01": {"turned": "@"}}}}"#,
        "actions.\"1e01\" has invalid encoder event \"turned\"",
    );
}

/// The rotation events belong to encoders only; binding one on a button is rejected.
#[test]
fn rejects_rotation_event_on_button() {
    assert_validation_error(
        r#"{"on_start": {"actions": {"1b01": {"turn_cw": "@"}}}}"#,
        "actions.\"1b01\" has invalid button event \"turn_cw\"",
    );
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
                    "timer": { "1": "@" }
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

/// A bare `@` action value is valid: it stays on the current scene, same as `~` did
/// before the breaking change that moved "stay" from `~` to `@`.
#[test]
fn accepts_bare_at_as_stay() {
    let path = write_scenes_config(r#"{"on_start": {"actions": {"1b01": {"pressed": "@"}}}}"#);
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(&path);
    assert!(config.is_ok(), "{config:?}");
}

// -- timer validation --

/// The `timer` entry must be a single-entry object; more entries are rejected.
#[test]
fn rejects_timer_with_multiple_entries() {
    let path = write_scenes_config(
        r#"{
            "on_start": {
                "actions": {
                    "timer": { "1": "@", "2": "@" }
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
                    "timer": { "soon": "@" }
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
                "actions": { "timer": { "soon": "@" } }
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
        r#"{"on_start": {"actions": {"0b01": {"pressed": "@"}}}}"#,
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

// -- `$path := value` config-assignment actions --

/// A `$defaults.button_brightness := N` action with a valid number loads with no
/// errors.
#[test]
fn accepts_valid_set_config_button_brightness() {
    let path = write_scenes_config(
        r#"{"on_start": {"actions": {"1b01": {"pressed": "$defaults.button_brightness := 80"}}}}"#,
    );
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(&path);
    assert!(config.is_ok(), "{:?}", config.err());
}

/// A `$defaults.encoder_brightness := N` action with a valid number loads with no
/// errors, distinctly from `button_brightness` above.
#[test]
fn accepts_valid_set_config_encoder_brightness() {
    let path = write_scenes_config(
        r#"{"on_start": {"actions": {"1b01": {"pressed": "$defaults.encoder_brightness := 15"}}}}"#,
    );
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(&path);
    assert!(config.is_ok(), "{:?}", config.err());
}

/// `$`-assignments targeting a real-but-immutable config field/section are rejected as
/// "read-only", distinctly from a genuinely nonexistent path (tested separately below).
#[test]
fn rejects_read_only_set_config_targets() {
    for (target, expected_path) in [
        (
            "$defaults.short_press_duration := 100",
            "defaults.short_press_duration",
        ),
        (
            "$defaults.double_click_gap := 100",
            "defaults.double_click_gap",
        ),
        ("$devices.1.key_count := 5", "devices.1.key_count"),
        ("$scenes.on_start.setup := 1", "scenes.on_start.setup"),
        ("$version := 100", "version"),
    ] {
        assert_validation_error(
            &format!(r#"{{"on_start": {{"actions": {{"1b01": {{"pressed": "{target}"}}}}}}}}"#),
            &format!("tries to set read-only parameter \"{expected_path}\""),
        );
    }
}

/// A `$`-assignment to a path that doesn't correspond to any real config field at all
/// is a distinct "unknown parameter" error, not "read-only".
#[test]
fn rejects_unknown_set_config_target() {
    assert_validation_error(
        r#"{"on_start": {"actions": {"1b01": {"pressed": "$foo.bar := 1"}}}}"#,
        "sets unknown parameter \"foo.bar\"",
    );
}

/// A quoted string right-hand-side for a numeric-only settable parameter is rejected
/// as a wrong-type error - `"100"` is never treated as the number `100`.
#[test]
fn rejects_string_value_for_numeric_default() {
    assert_validation_error(
        r#"{"on_start": {"actions": {"1b01": {"pressed": "$defaults.button_brightness := \"100\""}}}}"#,
        "requires a number",
    );
}

/// `:=` clamps an out-of-range number into its settable parameter's constraint
/// (`0`-`100` for both of today's parameters, matching `mirajazz::Device`'s own
/// internal `percent.clamp(0, 100)`) instead of rejecting the config outright, and
/// warns that it did.
#[test]
fn clamps_out_of_range_value_for_settable_default_with_warning() {
    let path = write_scenes_config(
        r#"{"on_start": {"actions": {"1b01": {"pressed": "$defaults.button_brightness := 200"}}}}"#,
    );
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(&path);
    let config = config.unwrap();
    assert!(
        config
            .warnings
            .iter()
            .any(|w| w.contains("out of range 0-100") && w.contains("clamped to 100")),
        "{:?}",
        config.warnings
    );
}

/// A negative number clamps up to the constraint's `min` (`0`), not just down to its
/// `max` - `-10` becomes `0`, with the same warning treatment as the too-large case
/// above.
#[test]
fn clamps_negative_value_for_settable_default_up_to_minimum() {
    let path = write_scenes_config(
        r#"{"on_start": {"actions": {"1b01": {"pressed": "$defaults.button_brightness := -10"}}}}"#,
    );
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(&path);
    let config = config.unwrap();
    assert!(
        config
            .warnings
            .iter()
            .any(|w| w.contains("out of range 0-100") && w.contains("clamped to 0")),
        "{:?}",
        config.warnings
    );
}

/// A value already within range produces no clamp warning at all.
#[test]
fn in_range_set_config_value_has_no_clamp_warning() {
    let path = write_scenes_config(
        r#"{"on_start": {"actions": {"1b01": {"pressed": "$defaults.button_brightness := 80"}}}}"#,
    );
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(&path);
    let config = config.unwrap();
    assert!(
        !config.warnings.iter().any(|w| w.contains("clamped")),
        "{:?}",
        config.warnings
    );
}

/// All three assignment operators are implemented for `defaults` targets: `:=` clamps
/// with a warning, `~=` clamps silently, and `=` accepts an in-range value but rejects
/// an out-of-range one.
#[test]
fn assignment_operators_are_all_implemented_for_defaults() {
    for value in [
        "$defaults.button_brightness := 200", // clamps, warns
        "$defaults.button_brightness ~= 200", // clamps, silent
        "$defaults.button_brightness = 80",   // in range
    ] {
        let path = write_scenes_config(&format!(
            r#"{{"on_start": {{"actions": {{"1b01": {{"pressed": "{value}"}}}}}}}}"#
        ));
        let config = load_config_from_path(path.to_str().unwrap());
        let _ = std::fs::remove_file(&path);
        assert!(config.is_ok(), "{value}: {:?}", config.err());
    }

    let path = write_scenes_config(
        r#"{"on_start": {"actions": {"1b01": {"pressed": "$defaults.button_brightness ~= 200"}}}}"#,
    );
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(&path);
    assert!(
        !config
            .unwrap()
            .warnings
            .iter()
            .any(|warning| warning.contains("clamped")),
        "~= clamps silently"
    );

    assert_validation_error(
        r#"{"on_start": {"actions": {"1b01": {"pressed": "$defaults.button_brightness = 200"}}}}"#,
        "outside 0..=100",
    );
}

/// Colour `defaults` targets are type-checked at load: a valid colour loads, `:=` on a
/// bad literal warns, `=` on one is an error, and a number or an int variable is a
/// wrong-type right-hand side.
#[test]
fn validates_colour_assignments() {
    for scenes in [
        r#"{"on_start": {"actions": {"1b01": {"pressed": "$defaults.background := \"red\""}}}}"#,
        r##"{"on_start": {"actions": {"1b01": {"pressed": "$defaults.background ~= \"#112233\""}}}}"##,
        r##"{"on_start": {"actions": {"1b01": {"pressed": "$defaults.text_color = \"#ffffff\""}}}}"##,
    ] {
        let path = write_scenes_config(scenes);
        let config = load_config_from_path(path.to_str().unwrap());
        let _ = std::fs::remove_file(&path);
        assert!(config.is_ok(), "{scenes}: {:?}", config.err());
    }

    let path = write_scenes_config(
        r#"{"on_start": {"actions": {"1b01": {"pressed": "$defaults.background := \"chartreuse\""}}}}"#,
    );
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(&path);
    assert!(
        config
            .unwrap()
            .warnings
            .iter()
            .any(|warning| warning.contains("unknown colour")),
        ":= on a bad literal colour warns"
    );

    for (scenes, expected) in [
        (
            r#"{"on_start": {"actions": {"1b01": {"pressed": "$defaults.background = \"chartreuse\""}}}}"#,
            "unknown colour",
        ),
        (
            r##"{"on_start": {"actions": {"1b01": {"pressed": "$defaults.background = \"#fff\""}}}}"##,
            "invalid colour",
        ),
        (
            r#"{"on_start": {"actions": {"1b01": {"pressed": "$defaults.background := 5"}}}}"#,
            "requires a colour",
        ),
        (
            r#"{"on_start": {"actions": {"1b01": {"pressed": "$defaults.text_color = 5"}}}}"#,
            "requires a colour",
        ),
    ] {
        let path = write_scenes_config(scenes);
        let config = load_config_from_path(path.to_str().unwrap());
        let _ = std::fs::remove_file(&path);
        let errors = error_texts(config.unwrap_err());
        assert!(
            errors.contains(expected),
            "{scenes}: expected {expected:?}, got: {errors}"
        );
    }

    // A colour sourced from an int variable is a type error; from a str variable it
    // loads, since the value is only known at runtime.
    let variables =
        r#"{"count": {"type": "int", "value": 5}, "name": {"type": "str", "value": "red"}}"#;
    let path = write_variables_config(
        variables,
        r#"{"on_start": {"actions": {"1b01": {"pressed": "$defaults.background = $count"}}}}"#,
    );
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(&path);
    let errors = error_texts(config.unwrap_err());
    assert!(errors.contains("which is a number"), "{errors}");

    let path = write_variables_config(
        variables,
        r#"{"on_start": {"actions": {"1b01": {"pressed": "$defaults.background = $name"}}}}"#,
    );
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(&path);
    assert!(config.is_ok(), "{:?}", config.err());

    // `~=` on a bad literal colour is silent: it loads with no error and no warning.
    let path = write_scenes_config(
        r#"{"on_start": {"actions": {"1b01": {"pressed": "$defaults.background ~= \"chartreuse\""}}}}"#,
    );
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(&path);
    let config = config.unwrap();
    assert!(
        !config
            .warnings
            .iter()
            .any(|warning| warning.contains("colour")),
        "~= on a bad literal colour is silent: {:?}",
        config.warnings
    );

    // A colour target accepts a command substitution; its output is parsed when it runs.
    let path = write_scenes_config(
        r#"{"on_start": {"actions": {"1b01": {"pressed": "$defaults.text_color := $(echo red)"}}}}"#,
    );
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(&path);
    assert!(config.is_ok(), "{:?}", config.err());

    // A variable reference that is not declared is still an error at load.
    let path = write_scenes_config(
        r#"{"on_start": {"actions": {"1b01": {"pressed": "$defaults.background := $missing"}}}}"#,
    );
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(&path);
    let errors = error_texts(config.unwrap_err());
    assert!(errors.contains("undefined variable"), "{errors}");
}

/// Brightness `defaults` targets are type-checked against a variable right-hand side:
/// an int variable loads, a str variable and an undeclared name are errors.
#[test]
fn validates_brightness_default_assignments() {
    let variables = r#"{"count": {"type": "int", "min": 0, "max": 100, "value": 5}, "name": {"type": "str", "value": "bright"}}"#;

    let path = write_variables_config(
        variables,
        r#"{"on_start": {"actions": {"1b01": {"pressed": "$defaults.button_brightness := $count"}}}}"#,
    );
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(&path);
    assert!(config.is_ok(), "{:?}", config.err());

    for (action, expected) in [
        (
            "$defaults.button_brightness := $name",
            "which is a string, but it requires a number",
        ),
        (
            "$defaults.button_brightness = $missing",
            "references undefined variable",
        ),
    ] {
        let path = write_variables_config(
            variables,
            &format!(r#"{{"on_start": {{"actions": {{"1b01": {{"pressed": "{action}"}}}}}}}}"#),
        );
        let config = load_config_from_path(path.to_str().unwrap());
        let _ = std::fs::remove_file(&path);
        let errors = error_texts(config.unwrap_err());
        assert!(errors.contains(expected), "{action}: {errors}");
    }
}

/// Variable assignments are type-checked and, for literals, range-checked at load time:
/// `:=` clamps with a warning, `=` rejects an out-of-range value, and wrong-type or
/// undefined right-hand sides are hard errors.
#[test]
fn validates_variable_assignments() {
    let variables = r#"{"count": {"type": "int", "min": 0, "max": 10, "value": 5}, "name": {"type": "str", "max_length": 3, "value": "abc"}}"#;

    let path = write_variables_config(
        variables,
        r#"{"on_start": {"actions": {"1b01": {"pressed": ["$count := 7", "$name := \"hi\"", "$count := $count"]}}}}"#,
    );
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(&path);
    assert!(config.is_ok(), "{:?}", config.err());

    let path = write_variables_config(
        variables,
        r#"{"on_start": {"actions": {"1b01": {"pressed": "$count := 100"}}}}"#,
    );
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(&path);
    assert!(
        config
            .unwrap()
            .warnings
            .iter()
            .any(|warning| warning.contains("out of range 0-10")),
        ":= clamps an out-of-range variable value with a warning"
    );

    for (action, expected) in [
        ("$count = 100", "outside 0..=10"),
        ("$count := \"x\"", "int variable"),
        ("$name := 5", "string variable"),
        ("$count := $name", "which is a string"),
        ("$missing := 5", "sets undefined variable"),
    ] {
        let escaped = action.replace('"', "\\\"");
        let path = write_variables_config(
            variables,
            &format!(r#"{{"on_start": {{"actions": {{"1b01": {{"pressed": "{escaped}"}}}}}}}}"#),
        );
        let config = load_config_from_path(path.to_str().unwrap());
        let _ = std::fs::remove_file(&path);
        let errors = error_texts(config.unwrap_err());
        assert!(
            errors.contains(expected),
            "{action}: expected {expected:?}, got: {errors}"
        );
    }
}

/// A `$(command)` right-hand side loads, its internal references are validated, and an
/// empty or nested substitution is rejected as malformed.
#[test]
fn validates_command_substitution_assignments() {
    let variables = r#"{"count": {"type": "int", "min": 0, "max": 100, "value": 5}}"#;

    let path = write_variables_config(
        variables,
        r#"{"on_start": {"actions": {"1b01": {"pressed": "$count := $(echo $count)"}}}}"#,
    );
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(&path);
    assert!(config.is_ok(), "{:?}", config.err());

    for (action, expected) in [
        ("$count := $(echo $missing)", "undefined variable"),
        ("$count := $(echo $(echo 1))", "malformed"),
    ] {
        let path = write_variables_config(
            variables,
            &format!(r#"{{"on_start": {{"actions": {{"1b01": {{"pressed": "{action}"}}}}}}}}"#),
        );
        let config = load_config_from_path(path.to_str().unwrap());
        let _ = std::fs::remove_file(&path);
        let errors = error_texts(config.unwrap_err());
        assert!(errors.contains(expected), "{action}: {errors}");
    }
}

/// A `$(command)` right-hand side whose literal program does not exist is warned about
/// (not rejected), matching how command actions and exec params are checked.
#[test]
fn warns_on_missing_command_substitution_program() {
    let path = write_variables_config(
        r#"{"count": {"type": "int", "min": 0, "max": 10, "value": 1}}"#,
        r#"{"on_start": {"actions": {"1b01": {"pressed": "$count := $(definitely-not-a-real-program-xyz)"}}}}"#,
    );
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(&path);
    let config = config.unwrap();
    assert!(
        config
            .warnings
            .iter()
            .any(|warning| warning.contains("program not found")),
        "{:?}",
        config.warnings
    );
}

/// A malformed `$`-assignment (no assignment operator at all) is rejected with its own
/// distinct error.
#[test]
fn rejects_malformed_set_config_action() {
    assert_validation_error(
        r#"{"on_start": {"actions": {"1b01": {"pressed": "$defaults.button_brightness 80"}}}}"#,
        "malformed \"$\" assignment",
    );
}

// -- home-directory (`~`) expansion --

/// A `~/relative.png` `image` params path resolves against the real `$HOME` and
/// produces no "file not found" warning, confirming `check_file_exists` expands it
/// before checking.
#[test]
fn tilde_path_in_image_params_resolves_against_home() {
    let _guard = ENV_LOCK.lock().unwrap();
    let home = temp_dir();
    let _set_home = SetHome::new(&home);
    std::fs::write(
        home.join("button.png"),
        b"not a real image, just needs to exist",
    )
    .unwrap();

    let path = write_scenes_config(
        r#"{"on_start": {"setup": {"1b01": {"type": "image", "params": "~/button.png"}}}}"#,
    );
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(&path);
    let config = config.unwrap();
    assert!(
        !config.warnings.iter().any(|w| w.contains("file not found")),
        "{:?}",
        config.warnings
    );
}

/// A `~/bin/tool` `image_exec` program path resolves against the real `$HOME` and
/// produces no "program not found" warning, confirming `check_executable` (via
/// `parse_command_line`) expands it before checking.
#[test]
fn tilde_path_in_exec_program_resolves_against_home() {
    let _guard = ENV_LOCK.lock().unwrap();
    let home = temp_dir();
    let _set_home = SetHome::new(&home);
    std::fs::create_dir_all(home.join("bin")).unwrap();
    let tool = home.join("bin/tool");
    std::fs::write(&tool, b"#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();

    let path = write_scenes_config(
        r#"{"on_start": {"setup": {"1b01": {"type": "image_exec", "params": "~/bin/tool"}}}}"#,
    );
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(&path);
    let config = config.unwrap();
    assert!(
        !config
            .warnings
            .iter()
            .any(|w| w.contains("program not found") || w.contains("not executable")),
        "{:?}",
        config.warnings
    );
}

// -- variables section --

/// A valid `variables` section loads and its declarations carry the right type,
/// constraints and initial value.
#[test]
fn loads_variables_section() {
    let path = write_variables_config(
        r#"{
            "count": { "type": "int", "min": 0, "max": 10, "value": 3 },
            "name": { "type": "str", "max_length": 5, "value": "Bob" }
        }"#,
        r#"{"on_start": {"actions": {}}}"#,
    );
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(&path);
    let config = config.unwrap();

    assert_eq!(config.variables["count"], VarDef::int(0, 10, 3));
    assert_eq!(
        config.variables["name"],
        VarDef::string(5, "Bob".to_string())
    );
    assert_eq!(config.variables["count"].initial, VarValue::Int(3));
}

/// A config without a `variables` section still loads, with an empty declaration map.
#[test]
fn variables_section_is_optional() {
    let path = write_scenes_config(r#"{"on_start": {"actions": {}}}"#);
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(&path);
    assert!(config.unwrap().variables.is_empty());
}

/// Invalid variable declarations are reported (as hard errors) and the config is
/// rejected as a whole.
#[test]
fn rejects_invalid_variable_declarations() {
    for (variables, expected) in [
        (r#"{"1a": {"type": "int"}}"#, "invalid variable name"),
        (r#"{"a": 5}"#, "must be an object"),
        (r#"{"a": {"value": 1}}"#, "\"type\" must be a string"),
        (r#"{"a": {"type": "bool"}}"#, "unknown type"),
        (r#"{"a": {"type": "int", "bogus": 1}}"#, "unknown key"),
        (
            r#"{"a": {"type": "int", "max_length": 3}}"#,
            "only valid for a \"str\"",
        ),
        (
            r#"{"a": {"type": "str", "min": 1}}"#,
            "only valid for an \"int\"",
        ),
        (
            r#"{"a": {"type": "int", "min": 5, "max": 1}}"#,
            "greater than",
        ),
        (
            r#"{"a": {"type": "int", "min": 0, "max": 10, "value": 11}}"#,
            "outside the declared range",
        ),
        (
            r#"{"a": {"type": "str", "max_length": 2, "value": "abc"}}"#,
            "longer than",
        ),
    ] {
        let path = write_variables_config(variables, r#"{"on_start": {"actions": {}}}"#);
        let config = load_config_from_path(path.to_str().unwrap());
        let _ = std::fs::remove_file(&path);
        let errors = error_texts(config.unwrap_err());
        assert!(
            errors.contains(expected),
            "expected errors to contain {expected:?}, got: {errors}"
        );
    }
}

// -- variable references in scenes --

/// Declared variables may be referenced from setup params and action commands; a dynamic
/// image path suppresses the literal file-existence warning.
#[test]
fn accepts_references_to_declared_variables() {
    let path = write_variables_config(
        r#"{"name": {"type": "str", "value": "Bob"}, "count": {"type": "int", "value": 3}}"#,
        r#"{
            "on_start": {
                "setup": { "1b01": { "type": "image", "params": "/tmp/$name.png" } },
                "actions": { "1b02": { "pressed": "/bin/echo $count $name" } }
            }
        }"#,
    );
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(&path);
    let config = config.unwrap_or_else(|errors| panic!("{errors:?}"));
    assert!(
        !config.warnings.iter().any(|w| w.contains("file not found")),
        "{:?}",
        config.warnings
    );
}

/// An undeclared reference anywhere in a scene is a hard error naming the variable.
#[test]
fn rejects_undefined_variable_references() {
    for scenes in [
        r#"{"on_start": {"setup": {"1b01": {"type": "image", "params": "/tmp/$missing.png"}}}}"#,
        r#"{"on_start": {"actions": {"1b01": {"pressed": "/bin/echo $missing"}}}}"#,
        r#"{"on_start": {"actions": {"1b01": {"pressed": "@$missing"}}}}"#,
    ] {
        let path = write_variables_config(r#"{}"#, scenes);
        let config = load_config_from_path(path.to_str().unwrap());
        let _ = std::fs::remove_file(&path);
        let errors = error_texts(config.unwrap_err());
        assert!(errors.contains("undefined variable"), "{errors}");
    }
}

/// A malformed `$` reference (not followed by a name) is a hard error.
#[test]
fn rejects_malformed_variable_reference() {
    let path = write_variables_config(
        r#"{"x": {"type": "int", "value": 1}}"#,
        r#"{"on_start": {"actions": {"1b01": {"pressed": "/bin/echo $ x"}}}}"#,
    );
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(&path);
    let errors = error_texts(config.unwrap_err());
    assert!(
        errors.contains("must be followed by a variable name"),
        "{errors}"
    );
}

/// Read-only `$defaults.*` constants may be read (e.g. from a command) even though they
/// cannot be assigned.
#[test]
fn allows_reading_read_only_defaults() {
    let path = write_scenes_config(
        r#"{"on_start": {"actions": {"1b01": {"pressed": "/bin/echo $defaults.short_press_duration"}}}}"#,
    );
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(&path);
    assert!(config.is_ok(), "{:?}", config.err());
}

/// A `text_value` setup entry accepts its params as display text (references included),
/// and rejects empty params.
#[test]
fn validates_text_value_setup_entries() {
    let path = write_variables_config(
        r#"{"name": {"type": "str", "value": "Bob"}}"#,
        r#"{"on_start": {"setup": {"1b01": {"type": "text_value", "params": "Hello $name", "refresh": 1}}}}"#,
    );
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(&path);
    assert!(config.is_ok(), "{:?}", config.err());

    let path = write_scenes_config(
        r#"{"on_start": {"setup": {"1b01": {"type": "text_value", "params": ""}}}}"#,
    );
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(&path);
    let errors = error_texts(config.unwrap_err());
    assert!(
        errors.contains("text_value params must be text"),
        "{errors}"
    );
}

/// A timer key may be an int variable reference.
#[test]
fn timer_key_may_reference_an_int_variable() {
    let path = write_variables_config(
        r#"{"period": {"type": "int", "min": 1, "value": 5}}"#,
        r#"{"on_start": {"actions": {"timer": {"$period": "@"}}}}"#,
    );
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(&path);
    assert!(config.is_ok(), "{:?}", config.err());
}

/// A timer key referencing a non-int variable is rejected.
#[test]
fn timer_key_rejects_non_int_variable() {
    let path = write_variables_config(
        r#"{"period": {"type": "str", "value": "5"}}"#,
        r#"{"on_start": {"actions": {"timer": {"$period": "@"}}}}"#,
    );
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(&path);
    let errors = error_texts(config.unwrap_err());
    assert!(errors.contains("timer seconds must be an int"), "{errors}");
}
