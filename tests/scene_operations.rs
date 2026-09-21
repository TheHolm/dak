//! Tests for `scene_operations` — building device operation plans from scene definitions.

mod common;

use dak::actions::{scene_operations, CommandSpec, SceneOp};
use dak::baseplane::Reference;
use serde_json::json;

use crate::common::write_scenes_config;

/// Static image buttons become ordered SetImage operations.
#[test]
fn scene_operations_extract_static_images() {
    let path = write_scenes_config(
        r#"{
            "on_start": {
                "setup": {
                    "1b01": { "type": "image", "params": "/path/one.ico" },
                    "1b02": { "type": "image", "params": "/path/two.png" }
                }
            }
        }"#,
    );
    let config = ::dak::actions::load_config_from_path(path.to_str().unwrap()).unwrap();
    let _ = std::fs::remove_file(path);

    let operations = scene_operations("on_start", &config.scenes).unwrap();
    assert_eq!(
        operations,
        vec![
            SceneOp::SetImage {
                reference: Reference::button(1, 1),
                path: "/path/one.ico".to_string(),
                refresh_seconds: 0,

                background: None,
            },
            SceneOp::SetImage {
                reference: Reference::button(1, 2),
                path: "/path/two.png".to_string(),
                refresh_seconds: 0,

                background: None,
            },
        ]
    );
}

/// A "refresh" field on the setup entry is read into the operation's
/// `refresh_seconds`; an entry without one defaults to 0.
#[test]
fn scene_operations_extract_refresh_seconds() {
    let path = write_scenes_config(
        r#"{
            "on_start": {
                "setup": {
                    "1b01": { "type": "image", "params": "/path/one.ico", "refresh": 5 },
                    "1b02": { "type": "image", "params": "/path/two.png" }
                }
            }
        }"#,
    );
    let config = ::dak::actions::load_config_from_path(path.to_str().unwrap()).unwrap();
    let _ = std::fs::remove_file(path);

    let operations = scene_operations("on_start", &config.scenes).unwrap();
    assert_eq!(
        operations,
        vec![
            SceneOp::SetImage {
                reference: Reference::button(1, 1),
                path: "/path/one.ico".to_string(),
                refresh_seconds: 5,

                background: None,
            },
            SceneOp::SetImage {
                reference: Reference::button(1, 2),
                path: "/path/two.png".to_string(),
                refresh_seconds: 0,

                background: None,
            },
        ]
    );
}

/// Unknown type names stay marked unsupported.
#[test]
fn scene_operations_mark_unsupported_commands() {
    // Unknown types are rejected by config validation, so feed `scene_operations`
    // the raw scene value it would never see from a valid config.
    let scenes = json!({
        "on_start": {
            "setup": {
                "1b02": { "type": "frobnicate", "params": "/usr/bin/text2gif -t Test" }
            }
        }
    });

    let operations = scene_operations("on_start", &scenes).unwrap();
    assert_eq!(
        operations,
        vec![SceneOp::Unsupported {
            kind: "frobnicate".to_string()
        }]
    );
}

/// image_exec buttons are parsed into ImageExec operations with the program split
/// from its collapsed params command line.
#[test]
fn scene_operations_extract_image_exec() {
    let path = write_scenes_config(
        r#"{
            "on_start": {
                "setup": {
                    "1b02": { "type": "image_exec", "params": "/usr/bin/convert input.png png:-" }
                }
            }
        }"#,
    );
    let config = ::dak::actions::load_config_from_path(path.to_str().unwrap()).unwrap();
    let _ = std::fs::remove_file(path);

    let operations = scene_operations("on_start", &config.scenes).unwrap();
    assert_eq!(
        operations,
        vec![SceneOp::ImageExec {
            reference: Reference::button(1, 2),
            command: CommandSpec {
                program: "/usr/bin/convert".to_string(),
                args: vec!["input.png".to_string(), "png:-".to_string()]
            },
            refresh_seconds: 0,

            background: None,
        }]
    );
}

/// Text buttons become Text operations.
#[test]
fn scene_operations_extract_text() {
    let path = write_scenes_config(
        r#"{
            "on_start": {
                "setup": {
                    "1b03": { "type": "text", "params": "/tmp/notes.txt" }
                }
            }
        }"#,
    );
    let config = ::dak::actions::load_config_from_path(path.to_str().unwrap()).unwrap();
    let _ = std::fs::remove_file(path);

    let operations = scene_operations("on_start", &config.scenes).unwrap();
    assert_eq!(
        operations,
        vec![SceneOp::Text {
            reference: Reference::button(1, 3),
            path: "/tmp/notes.txt".to_string(),
            refresh_seconds: 0,

            background: None,
            text_color: None,
        }]
    );
}

/// A `~/rest` `image`/`text` params path is expanded to `$HOME/rest` in the resulting
/// operation, not kept literal - `scene_operations` is what actually opens the file at
/// runtime, so it must see the resolved path.
#[test]
fn scene_operations_expands_tilde_in_image_and_text_params() {
    let _guard = common::ENV_LOCK.lock().unwrap();
    let home = common::temp_dir();
    let _set_home = common::SetHome::new(&home);

    let path = write_scenes_config(
        r#"{
            "on_start": {
                "setup": {
                    "1b01": { "type": "image", "params": "~/pics/button.png" },
                    "1b02": { "type": "text", "params": "~/notes.txt" }
                }
            }
        }"#,
    );
    let config = ::dak::actions::load_config_from_path(path.to_str().unwrap()).unwrap();
    let _ = std::fs::remove_file(path);

    let operations = scene_operations("on_start", &config.scenes).unwrap();
    assert_eq!(
        operations,
        vec![
            SceneOp::SetImage {
                reference: Reference::button(1, 1),
                path: home.join("pics/button.png").to_string_lossy().into_owned(),
                refresh_seconds: 0,

                background: None,
            },
            SceneOp::Text {
                reference: Reference::button(1, 2),
                path: home.join("notes.txt").to_string_lossy().into_owned(),
                refresh_seconds: 0,

                background: None,
                text_color: None,
            },
        ]
    );
}

/// text_exec buttons are parsed into TextExec operations with the program split
/// from its collapsed params command line.
#[test]
fn scene_operations_extract_text_exec() {
    let path = write_scenes_config(
        r#"{
            "on_start": {
                "setup": {
                    "1b03": { "type": "text_exec", "params": "/usr/bin/date +%H:%M" }
                }
            }
        }"#,
    );
    let config = ::dak::actions::load_config_from_path(path.to_str().unwrap()).unwrap();
    let _ = std::fs::remove_file(path);

    let operations = scene_operations("on_start", &config.scenes).unwrap();
    assert_eq!(
        operations,
        vec![SceneOp::TextExec {
            reference: Reference::button(1, 3),
            command: CommandSpec {
                program: "/usr/bin/date".to_string(),
                args: vec!["+%H:%M".to_string()]
            },
            refresh_seconds: 0,

            background: None,
            text_color: None,
        }]
    );
}

/// A `text_exec` with "refresh" set keeps re-running the command on its own, so this
/// is the config that lets a clock button skip the whole-scene `timer` trick.
#[test]
fn scene_operations_extract_text_exec_with_refresh() {
    let path = write_scenes_config(
        r#"{
            "on_start": {
                "setup": {
                    "1b03": { "type": "text_exec", "params": "/usr/bin/date +%H:%M", "refresh": 1 }
                }
            }
        }"#,
    );
    let config = ::dak::actions::load_config_from_path(path.to_str().unwrap()).unwrap();
    let _ = std::fs::remove_file(path);

    let operations = scene_operations("on_start", &config.scenes).unwrap();
    assert_eq!(
        operations,
        vec![SceneOp::TextExec {
            reference: Reference::button(1, 3),
            command: CommandSpec {
                program: "/usr/bin/date".to_string(),
                args: vec!["+%H:%M".to_string()]
            },
            refresh_seconds: 1,

            background: None,
            text_color: None,
        }]
    );
}

/// A text_exec params command line with a quoted argument keeps it intact.
#[test]
fn scene_operations_extract_text_exec_quoted_args() {
    let scenes = json!({
        "on_start": {
            "setup": {
                "1b03": { "type": "text_exec", "params": "/usr/bin/find . -name '*.rs'" }
            }
        }
    });
    let operations = scene_operations("on_start", &scenes).unwrap();
    assert_eq!(
        operations,
        vec![SceneOp::TextExec {
            reference: Reference::button(1, 3),
            command: CommandSpec {
                program: "/usr/bin/find".to_string(),
                args: vec![".".to_string(), "-name".to_string(), "*.rs".to_string(),]
            },
            refresh_seconds: 0,

            background: None,
            text_color: None,
        }]
    );
}

/// A text_exec with empty params is rejected.
#[test]
fn scene_operations_reject_text_exec_empty_params() {
    let scenes = json!({
        "on_start": {
            "setup": {
                "1b01": { "type": "text_exec", "params": "" }
            }
        }
    });
    let error = scene_operations("on_start", &scenes).unwrap_err();
    assert!(
        error.contains("text_exec params must be a program command line"),
        "{error}"
    );
}

/// A text_exec with malformed params is rejected.
#[test]
fn scene_operations_reject_text_exec_unbalanced_quotes() {
    let scenes = json!({
        "on_start": {
            "setup": {
                "1b01": { "type": "text_exec", "params": "/bin/sh -c 'oops" }
            }
        }
    });
    let error = scene_operations("on_start", &scenes).unwrap_err();
    assert!(error.contains("unbalanced quotes"), "{error}");
}

/// A button entry with a non-string type is rejected.
#[test]
fn scene_operations_reject_missing_type() {
    let scenes = json!({
        "on_start": {
            "setup": {
                "1b01": { "params": "/tmp/notes.txt" }
            }
        }
    });
    let error = scene_operations("on_start", &scenes).unwrap_err();
    assert!(
        error.contains("key \"1b01\" type must be a string"),
        "{error}"
    );
}

/// A button entry with a non-string params value is rejected.
#[test]
fn scene_operations_reject_params_not_a_string() {
    let scenes = json!({
        "on_start": {
            "setup": {
                "1b01": { "type": "image", "params": 42 }
            }
        }
    });
    let error = scene_operations("on_start", &scenes).unwrap_err();
    assert!(
        error.contains("key \"1b01\" params must be a string"),
        "{error}"
    );
}

/// A button entry that is not an object is rejected.
#[test]
fn scene_operations_reject_button_entry_not_an_object() {
    let scenes = json!({
        "on_start": {
            "setup": {
                "1b01": "image"
            }
        }
    });
    let error = scene_operations("on_start", &scenes).unwrap_err();
    assert!(error.contains("key \"1b01\" must be an object"), "{error}");
}

/// launch buttons are parsed into Launch operations with the program split from
/// the collapsed params command line; the reference is only a config slot.
#[test]
fn scene_operations_extract_launch() {
    let scenes = json!({
        "on_start": {
            "setup": {
                "1b04": { "type": "launch", "params": "/usr/bin/systemctl suspend" }
            }
        }
    });
    let operations = scene_operations("on_start", &scenes).unwrap();
    assert_eq!(
        operations,
        vec![SceneOp::Launch {
            reference: Reference::button(1, 4),
            command: CommandSpec {
                program: "/usr/bin/systemctl".to_string(),
                args: vec!["suspend".to_string()]
            }
        }]
    );
}

/// A launch params command line with a quoted argument keeps it intact.
#[test]
fn scene_operations_extract_launch_quoted_args() {
    let scenes = json!({
        "on_start": {
            "setup": {
                "1b02": { "type": "launch", "params": "/bin/sh -c 'echo detached'" }
            }
        }
    });
    let operations = scene_operations("on_start", &scenes).unwrap();
    assert_eq!(
        operations,
        vec![SceneOp::Launch {
            reference: Reference::button(1, 2),
            command: CommandSpec {
                program: "/bin/sh".to_string(),
                args: vec!["-c".to_string(), "echo detached".to_string()]
            }
        }]
    );
}

/// A scene whose `setup` value is not an object returns an error.
#[test]
fn scene_operations_reject_setup_not_an_object() {
    let scenes = json!({
        "on_start": {
            "setup": []
        }
    });
    let error = scene_operations("on_start", &scenes).unwrap_err();
    assert!(
        error.contains("scene \"on_start\": setup is not an object"),
        "{error}"
    );
}

/// Clear buttons become Clear operations with the given reference, params optional.
#[test]
fn scene_operations_extract_clear() {
    let path = write_scenes_config(
        r#"{
            "on_start": {
                "setup": {
                    "1b03": { "type": "clear" }
                }
            }
        }"#,
    );
    let config = ::dak::actions::load_config_from_path(path.to_str().unwrap()).unwrap();
    let _ = std::fs::remove_file(path);

    let operations = scene_operations("on_start", &config.scenes).unwrap();
    assert_eq!(
        operations,
        vec![SceneOp::Clear {
            reference: Reference::button(1, 3)
        }]
    );
}

/// A scene with no numbered buttons yields no operations.
#[test]
fn scene_operations_return_empty_for_scene_without_buttons() {
    let path = write_scenes_config(r#"{"on_start": {"actions": {}}}"#);
    let config = ::dak::actions::load_config_from_path(path.to_str().unwrap()).unwrap();
    let _ = std::fs::remove_file(path);

    let operations = scene_operations("on_start", &config.scenes).unwrap();
    assert!(operations.is_empty());
}

/// Building operations for an undefined scene returns an error.
#[test]
fn scene_operations_reject_undefined_scene() {
    let path = write_scenes_config(r#"{"on_start": {"actions": {}}}"#);
    let config = ::dak::actions::load_config_from_path(path.to_str().unwrap()).unwrap();
    let _ = std::fs::remove_file(path);

    let error = scene_operations("Missing", &config.scenes).unwrap_err();
    assert!(
        error.contains("scene \"Missing\" is not defined"),
        "{error}"
    );
}

/// A scene whose value is not an object returns an error.
#[test]
fn scene_operations_reject_scene_not_an_object() {
    let scenes = json!({ "on_start": [] });
    let error = scene_operations("on_start", &scenes).unwrap_err();
    assert!(
        error.contains("scene \"on_start\" is not an object"),
        "{error}"
    );
}

/// A non-control-reference top-level key returns an error rooted at the scene and key.
#[test]
fn scene_operations_reject_non_reference_key() {
    let scenes = json!({
        "on_start": {
            "setup": {
                "foo": { "type": "image", "params": "/a" }
            }
        }
    });
    let error = scene_operations("on_start", &scenes).unwrap_err();
    assert!(
        error.contains("scene \"on_start\": key \"foo\" is not a valid control reference"),
        "{error}"
    );
}

/// Device and control numbers outside the reference limits are rejected when building
/// the operation plan, exactly as at validation time.
#[test]
fn scene_operations_reject_out_of_range_references() {
    for bad in ["0b01", "1b00", "1b100"] {
        let scenes = json!({
            "on_start": {
                "setup": {
                    bad: { "type": "image", "params": "/a" }
                }
            }
        });
        let error = scene_operations("on_start", &scenes).unwrap_err();
        assert!(
            error.contains("is not a valid control reference"),
            "for reference {bad}: {error}"
        );
    }
}

/// Encoder references are legitimate control references and build operations that name
/// the encoder; driving them is a later step.
#[test]
fn scene_operations_accept_encoder_references() {
    let scenes = json!({
        "on_start": {
            "setup": {
                "2e01": { "type": "text", "params": "/tmp/notes.txt" }
            }
        }
    });
    let operations = scene_operations("on_start", &scenes).unwrap();
    assert_eq!(
        operations,
        vec![SceneOp::Text {
            reference: Reference::encoder(2, 1),
            path: "/tmp/notes.txt".to_string(),
            refresh_seconds: 0,

            background: None,
            text_color: None,
        }]
    );
}
