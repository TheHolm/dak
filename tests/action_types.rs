//! Tests for action value parsing (`parse_action`), per-key action lookup for the
//! `pressed`/`released` events (`action_for_event`), and timer lookup (`timer_for_scene`).

mod common;

use dak::actions::{action_for_event, parse_action, timer_for_scene, Action};
use dak::baseplane::Reference;

use crate::common::write_scenes_config;

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

/// `action_for_event` resolves the "pressed" action for a control reference, None when unbound
/// or the scene is undefined.
#[test]
fn action_for_event_reads_pressed_action() {
    let path = write_scenes_config(
        r#"{
            "on_start": {
                "actions": {
                    "1b01": { "pressed": "~" },
                    "1b02": { "pressed": "@Test" },
                    "1b03": { "pressed": "/usr/bin/date +%H:%M" }
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
        action_for_event(
            "on_start",
            None,
            &Reference::button(1, 1),
            "pressed",
            &config.scenes
        ),
        Some("~")
    );
    assert_eq!(
        action_for_event(
            "on_start",
            None,
            &Reference::button(1, 2),
            "pressed",
            &config.scenes
        ),
        Some("@Test")
    );
    assert_eq!(
        action_for_event(
            "on_start",
            None,
            &Reference::button(1, 3),
            "pressed",
            &config.scenes
        ),
        Some("/usr/bin/date +%H:%M")
    );
    assert_eq!(
        action_for_event(
            "on_start",
            None,
            &Reference::button(1, 9),
            "pressed",
            &config.scenes
        ),
        None
    );
    assert_eq!(
        action_for_event(
            "Missing",
            None,
            &Reference::button(1, 1),
            "pressed",
            &config.scenes
        ),
        None
    );
}

/// An action missing from the current scene is looked up in the previous scene.
#[test]
fn action_for_event_inherits_from_previous_scene() {
    let path = write_scenes_config(
        r#"{
            "on_start": {
                "actions": {
                    "1b02": { "pressed": "@Test" }
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
        action_for_event(
            "Main",
            Some("on_start"),
            &Reference::button(1, 2),
            "pressed",
            &config.scenes
        ),
        Some("@Test")
    );
    // Without a previous scene the same lookup finds nothing.
    assert_eq!(
        action_for_event(
            "Main",
            None,
            &Reference::button(1, 2),
            "pressed",
            &config.scenes
        ),
        None
    );
}

/// A scene without an `actions` field at all still falls through to the previous scene.
#[test]
fn action_for_event_inherits_when_scene_has_no_actions() {
    let path = write_scenes_config(
        r#"{
            "on_start": {
                "actions": {
                    "1b02": { "pressed": "@Test" }
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
        action_for_event(
            "Main",
            Some("on_start"),
            &Reference::button(1, 2),
            "pressed",
            &config.scenes
        ),
        Some("@Test")
    );
    assert_eq!(
        action_for_event(
            "Main",
            None,
            &Reference::button(1, 2),
            "pressed",
            &config.scenes
        ),
        None
    );
}

/// A scene that explicitly configures a reference wins over the previous scene, and an
/// empty `pressed` value means bound-but-no-action, so it ends the search.
#[test]
fn action_for_event_explicit_binding_overrides_inheritance() {
    let path = write_scenes_config(
        r#"{
            "on_start": {
                "actions": {
                    "1b02": { "pressed": "@Test" }
                }
            },
            "Main": {
                "actions": {
                    "1b02": { "pressed": "" }
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
        action_for_event(
            "Main",
            Some("on_start"),
            &Reference::button(1, 2),
            "pressed",
            &config.scenes
        ),
        None
    );
    assert_eq!(
        action_for_event(
            "Main",
            Some("on_start"),
            &Reference::button(1, 9),
            "pressed",
            &config.scenes
        ),
        None
    );
}

/// `action_for_event` resolves the `released` action for a control reference, independently
/// of the `pressed` binding; None when unbound.
#[test]
fn action_for_event_reads_released_action() {
    let path = write_scenes_config(
        r#"{
            "on_start": {
                "actions": {
                    "1b01": { "pressed": "~", "released": "@Test" },
                    "1b02": { "released": "/usr/bin/date +%H:%M" }
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
        action_for_event(
            "on_start",
            None,
            &Reference::button(1, 1),
            "released",
            &config.scenes
        ),
        Some("@Test")
    );
    // A pressed binding does not leak into the released lookup.
    assert_eq!(
        action_for_event(
            "on_start",
            None,
            &Reference::button(1, 1),
            "pressed",
            &config.scenes
        ),
        Some("~")
    );
    assert_eq!(
        action_for_event(
            "on_start",
            None,
            &Reference::button(1, 2),
            "released",
            &config.scenes
        ),
        Some("/usr/bin/date +%H:%M")
    );
    // No released binding at all.
    assert_eq!(
        action_for_event(
            "on_start",
            None,
            &Reference::button(1, 9),
            "released",
            &config.scenes
        ),
        None
    );
}

/// A `released` action missing from the current scene is looked up in the previous scene.
#[test]
fn action_for_event_released_inherits_from_previous_scene() {
    let path = write_scenes_config(
        r#"{
            "on_start": {
                "actions": {
                    "1b02": { "released": "@Test" }
                }
            },
            "Main": {
                "actions": {}
            },
            "Test": {
                "actions": {}
            }
        }"#,
    );
    let config = ::dak::actions::load_config_from_path(path.to_str().unwrap()).unwrap();
    let _ = std::fs::remove_file(path);

    assert_eq!(
        action_for_event(
            "Main",
            Some("on_start"),
            &Reference::button(1, 2),
            "released",
            &config.scenes
        ),
        Some("@Test")
    );
    // Without a previous scene the same lookup finds nothing.
    assert_eq!(
        action_for_event(
            "Main",
            None,
            &Reference::button(1, 2),
            "released",
            &config.scenes
        ),
        None
    );
}

/// An empty `released` value in the current scene means bound-but-no-action, so it ends
/// the search instead of falling through to the previous scene.
#[test]
fn action_for_event_empty_released_binding_suppresses_inheritance() {
    let path = write_scenes_config(
        r#"{
            "on_start": {
                "actions": {
                    "1b02": { "released": "@Test" }
                }
            },
            "Main": {
                "actions": {
                    "1b02": { "released": "" }
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
        action_for_event(
            "Main",
            Some("on_start"),
            &Reference::button(1, 2),
            "released",
            &config.scenes
        ),
        None
    );
}

/// `timer_for_scene` returns the seconds and action for a scene with a timer.
#[test]
fn timer_for_scene_returns_seconds_and_action() {
    let path = write_scenes_config(
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
    let path = write_scenes_config(
        r#"{
            "on_start": {
                "actions": {
                    "1b01": { "pressed": "~" }
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
    let path = write_scenes_config(r#"{"on_start": {"actions": {}}}"#);
    let config = ::dak::actions::load_config_from_path(path.to_str().unwrap()).unwrap();
    let _ = std::fs::remove_file(path);

    assert_eq!(timer_for_scene("Missing", &config.scenes), None);
}

/// `timer_for_scene` returns None when actions is empty.
#[test]
fn timer_for_scene_returns_none_with_empty_actions() {
    let path = write_scenes_config(r#"{"on_start": {"actions": {}}}"#);
    let config = ::dak::actions::load_config_from_path(path.to_str().unwrap()).unwrap();
    let _ = std::fs::remove_file(path);

    assert_eq!(timer_for_scene("on_start", &config.scenes), None);
}

/// `action_for_event` resolves the per-notch twist events of an encoder reference
/// (`turn_cw` / `turn_ccw`) on their own keys.
#[test]
fn action_for_event_reads_encoder_turn_actions() {
    let path = write_scenes_config(
        r#"{
            "on_start": {
                "actions": {
                    "1e01": { "turn_cw": "~", "turn_ccw": "@Test" }
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
        action_for_event(
            "on_start",
            None,
            &Reference::encoder(1, 1),
            "turn_cw",
            &config.scenes
        ),
        Some("~")
    );
    assert_eq!(
        action_for_event(
            "on_start",
            None,
            &Reference::encoder(1, 1),
            "turn_ccw",
            &config.scenes
        ),
        Some("@Test")
    );
    // Unbound twist events resolve to nothing.
    assert_eq!(
        action_for_event(
            "on_start",
            None,
            &Reference::encoder(1, 2),
            "turn_cw",
            &config.scenes
        ),
        None
    );
    // Twist actions never leak from a button's press binding.
    assert_eq!(
        action_for_event(
            "on_start",
            None,
            &Reference::button(1, 1),
            "turn_cw",
            &config.scenes
        ),
        None
    );
}

/// `action_for_event` resolves an encoder push exactly like a button press: the same
/// `pressed` / `released` events on the encoder reference.
#[test]
fn action_for_event_reads_encoder_push_actions() {
    let path = write_scenes_config(
        r#"{
            "on_start": {
                "actions": {
                    "1e01": { "pressed": "~", "released": "@Test" },
                    "1e02": { "short_press": "/usr/bin/aplay beep.wav" }
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
        action_for_event(
            "on_start",
            None,
            &Reference::encoder(1, 1),
            "pressed",
            &config.scenes
        ),
        Some("~")
    );
    assert_eq!(
        action_for_event(
            "on_start",
            None,
            &Reference::encoder(1, 1),
            "released",
            &config.scenes
        ),
        Some("@Test")
    );
    assert_eq!(
        action_for_event(
            "on_start",
            None,
            &Reference::encoder(1, 2),
            "short_press",
            &config.scenes
        ),
        Some("/usr/bin/aplay beep.wav")
    );
}
