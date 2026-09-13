//! Tests for action value parsing (`parse_action`), per-key action lookup (`action_for_key`),
//! and timer lookup (`timer_for_scene`).

mod common;

use dak::actions::{action_for_key, parse_action, timer_for_scene, Action};

use crate::common::write_temp_config;

/// Action values classify as Stay (`~`), SwitchScene (`@name`) or Command.
#[test]
fn parse_action_classifies_actions() {
    assert_eq!(parse_action("~"), Action::Stay);
    assert_eq!(
        parse_action("@Main"),
        Action::SwitchScene {
            scene: "Main".to_string()
        }
    );
    assert_eq!(
        parse_action("/usr/bin/aplay /usr/share/sounds/sound-icons/prompt.wav"),
        Action::Command {
            command: "/usr/bin/aplay /usr/share/sounds/sound-icons/prompt.wav".to_string()
        }
    );
}

/// `action_for_key` resolves the pressed action for a 1-based key, None when unbound
/// or the scene is undefined.
#[test]
fn action_for_key_reads_pressed_action() {
    let path = write_temp_config(
        r#"{
            "on_start": {
                "actions": {
                    "1": { "pressed": "~" },
                    "2": { "pressed": "@Test" },
                    "3": { "pressed": "/usr/bin/date +%H:%M" }
                }
            },
            "Test": {
                "actions": {}
            }
        }"#,
    );
    let config = ::dak::actions::load_config_from_path(path.to_str().unwrap()).unwrap();
    let _ = std::fs::remove_file(path);

    assert_eq!(
        action_for_key("on_start", None, 1, &config.scenes),
        Some("~")
    );
    assert_eq!(
        action_for_key("on_start", None, 2, &config.scenes),
        Some("@Test")
    );
    assert_eq!(
        action_for_key("on_start", None, 3, &config.scenes),
        Some("/usr/bin/date +%H:%M")
    );
    assert_eq!(action_for_key("on_start", None, 9, &config.scenes), None);
    assert_eq!(action_for_key("Missing", None, 1, &config.scenes), None);
}

/// An action missing from the current scene is looked up in the previous scene.
#[test]
fn action_for_key_inherits_from_previous_scene() {
    let path = write_temp_config(
        r#"{
            "on_start": {
                "actions": {
                    "2": { "pressed": "@Test" }
                }
            },
            "Main": {
                "actions": {
                    "timer": { "1": "~" }
                }
            },
            "Test": {
                "actions": {}
            }
        }"#,
    );
    let config = ::dak::actions::load_config_from_path(path.to_str().unwrap()).unwrap();
    let _ = std::fs::remove_file(path);

    assert_eq!(
        action_for_key("Main", Some("on_start"), 2, &config.scenes),
        Some("@Test")
    );
    // Without a previous scene the same lookup finds nothing.
    assert_eq!(action_for_key("Main", None, 2, &config.scenes), None);
}

/// A scene without an `actions` field at all still falls through to the previous scene.
#[test]
fn action_for_key_inherits_when_scene_has_no_actions() {
    let path = write_temp_config(
        r#"{
            "on_start": {
                "actions": {
                    "2": { "pressed": "@Test" }
                }
            },
            "Main": {},
            "Test": {
                "actions": {}
            }
        }"#,
    );
    let config = ::dak::actions::load_config_from_path(path.to_str().unwrap()).unwrap();
    let _ = std::fs::remove_file(path);

    assert_eq!(
        action_for_key("Main", Some("on_start"), 2, &config.scenes),
        Some("@Test")
    );
    assert_eq!(action_for_key("Main", None, 2, &config.scenes), None);
}

/// A scene that explicitly configures a key wins over the previous scene, and an
/// empty `pressed` value means bound-but-no-action, so it ends the search.
#[test]
fn action_for_key_explicit_binding_overrides_inheritance() {
    let path = write_temp_config(
        r#"{
            "on_start": {
                "actions": {
                    "2": { "pressed": "@Test" }
                }
            },
            "Main": {
                "actions": {
                    "2": { "pressed": "" }
                }
            },
            "Test": {
                "actions": {}
            }
        }"#,
    );
    let config = ::dak::actions::load_config_from_path(path.to_str().unwrap()).unwrap();
    let _ = std::fs::remove_file(path);

    assert_eq!(
        action_for_key("Main", Some("on_start"), 2, &config.scenes),
        None
    );
    assert_eq!(
        action_for_key("Main", Some("on_start"), 9, &config.scenes),
        None
    );
}

/// `timer_for_scene` returns the seconds and action for a scene with a timer.
#[test]
fn timer_for_scene_returns_seconds_and_action() {
    let path = write_temp_config(
        r#"{
            "on_start": {
                "actions": {
                    "timer": { "5": "@Main" }
                }
            },
            "Main": {
                "actions": {
                    "timer": { "1": "~" }
                }
            }
        }"#,
    );
    let config = ::dak::actions::load_config_from_path(path.to_str().unwrap()).unwrap();
    let _ = std::fs::remove_file(path);

    assert_eq!(
        timer_for_scene("on_start", &config.scenes),
        Some((5, "@Main"))
    );
    assert_eq!(timer_for_scene("Main", &config.scenes), Some((1, "~")));
}

/// `timer_for_scene` returns None when the scene has no timer.
#[test]
fn timer_for_scene_returns_none_without_timer() {
    let path = write_temp_config(
        r#"{
            "on_start": {
                "actions": {
                    "1": { "pressed": "~" }
                }
            }
        }"#,
    );
    let config = ::dak::actions::load_config_from_path(path.to_str().unwrap()).unwrap();
    let _ = std::fs::remove_file(path);

    assert_eq!(timer_for_scene("on_start", &config.scenes), None);
}

/// `timer_for_scene` returns None for an undefined scene.
#[test]
fn timer_for_scene_returns_none_for_undefined_scene() {
    let path = write_temp_config(r#"{"on_start": {"actions": {}}}"#);
    let config = ::dak::actions::load_config_from_path(path.to_str().unwrap()).unwrap();
    let _ = std::fs::remove_file(path);

    assert_eq!(timer_for_scene("Missing", &config.scenes), None);
}

/// `timer_for_scene` returns None when actions is empty.
#[test]
fn timer_for_scene_returns_none_with_empty_actions() {
    let path = write_temp_config(r#"{"on_start": {"actions": {}}}"#);
    let config = ::dak::actions::load_config_from_path(path.to_str().unwrap()).unwrap();
    let _ = std::fs::remove_file(path);

    assert_eq!(timer_for_scene("on_start", &config.scenes), None);
}
