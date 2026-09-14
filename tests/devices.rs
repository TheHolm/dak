//! Tests for the config `devices` section: logical device ids, device mapping parsing
//! and integration with the loaded config.

mod common;

use dak::actions::load_config_from_path;

use crate::common::{error_texts, write_temp_config};

/// A config with a full device definition loads, and the mapping is available under its
/// logical device id.
#[test]
fn loads_config_with_devices_section() {
    let path = write_temp_config(
        r#"{
            "scenes": { "on_start": { "actions": {} } },
            "devices": {
                "1": {
                    "device_id": "0300:3002",
                    "device_name": "Ajazz HOTSPOTEKUSB HID DEMO",
                    "serial": "ABC123",
                    "key_count": 9,
                    "encoder_count": 3,
                    "screens": 6,
                    "buttons": [
                        { "number": 1, "press": 1, "release": 1, "screen": true, "draw_id": 1 },
                        { "number": 2, "press": 2, "release": 2, "screen": false, "draw_id": -1 }
                    ],
                    "encoders": [
                        { "number": 1, "cw": 144, "ccw": 145 }
                    ]
                }
            }
        }"#,
    );
    let config = load_config_from_path(path.to_str().unwrap()).unwrap();
    let _ = std::fs::remove_file(path);

    let definition = config
        .devices
        .by_id
        .get(&1)
        .expect("device 1 should be defined");
    assert_eq!(definition.device_id, "0300:3002");
    assert_eq!(definition.serial, "ABC123");
    assert_eq!(definition.key_count, 9);
    assert_eq!(definition.buttons.len(), 2);
    assert_eq!(definition.buttons[0].number, 1);
    assert!(!definition.buttons[1].screen);
    assert_eq!(definition.encoders[0].ccw, 145);
    assert!(!config.devices.by_id.contains_key(&2));
}

/// An encoder definition with push/release codes loads them into the mapping.
#[test]
fn loads_encoder_push_codes() {
    let path = write_temp_config(
        r#"{
            "scenes": { "on_start": { "actions": {} } },
            "devices": {
                "1": {
                    "device_id": "0300:3002",
                    "device_name": "pad",
                    "serial": "ABC123",
                    "key_count": 9,
                    "encoder_count": 1,
                    "screens": 6,
                    "buttons": [],
                    "encoders": [
                        { "number": 1, "cw": 144, "ccw": 145, "press": 146, "release": 146 }
                    ]
                }
            }
        }"#,
    );
    let config = load_config_from_path(path.to_str().unwrap()).unwrap();
    let _ = std::fs::remove_file(path);

    let encoder = &config.devices.by_id[&1].encoders[0];
    assert_eq!(encoder.press, 146);
    assert_eq!(encoder.release, 146);
}

/// An older encoder definition without push codes still loads, with the push
/// codes defaulting to zero (no captured push).
#[test]
fn loads_encoder_without_push_codes() {
    let path = write_temp_config(
        r#"{
            "scenes": { "on_start": { "actions": {} } },
            "devices": {
                "1": {
                    "device_id": "0300:3002",
                    "device_name": "pad",
                    "serial": "ABC123",
                    "key_count": 9,
                    "encoder_count": 1,
                    "screens": 6,
                    "buttons": [],
                    "encoders": [
                        { "number": 1, "cw": 144, "ccw": 145 }
                    ]
                }
            }
        }"#,
    );
    let config = load_config_from_path(path.to_str().unwrap()).unwrap();
    let _ = std::fs::remove_file(path);

    let encoder = &config.devices.by_id[&1].encoders[0];
    assert_eq!(encoder.press, 0);
    assert_eq!(encoder.release, 0);
}

/// Multiple devices can be declared; each lands under its own id in ascending order.
#[test]
fn loads_multiple_device_definitions() {
    let path = write_temp_config(
        r#"{
            "scenes": { "on_start": { "actions": {} } },
            "devices": {
                "3": {
                    "device_id": "0300:3002",
                    "device_name": "pad",
                    "serial": "C",
                    "key_count": 9,
                    "encoder_count": 3,
                    "screens": 6,
                    "buttons": [],
                    "encoders": []
                },
                "1": {
                    "device_id": "0300:3002",
                    "device_name": "pad",
                    "serial": "A",
                    "key_count": 9,
                    "encoder_count": 3,
                    "screens": 6,
                    "buttons": [],
                    "encoders": []
                }
            }
        }"#,
    );
    let config = load_config_from_path(path.to_str().unwrap()).unwrap();
    let _ = std::fs::remove_file(path);

    let ids: Vec<u8> = config.devices.by_id.keys().copied().collect();
    assert_eq!(ids, vec![1, 3]);
    assert_eq!(config.devices.by_id[&3].serial, "C");
}

/// An empty `devices` dictionary is valid: no device is declared.
#[test]
fn accepts_empty_devices_section() {
    let path = write_temp_config(r#"{"scenes": {"on_start": {"actions": {}}}, "devices": {}}"#);
    let config = load_config_from_path(path.to_str().unwrap()).unwrap();
    let _ = std::fs::remove_file(path);
    assert!(config.devices.by_id.is_empty());
}

/// Device ids are the single digits 1..=9 used in references; other keys are rejected.
#[test]
fn rejects_invalid_device_ids() {
    for id in ["0", "10", "abc", ""] {
        let path = write_temp_config(&format!(
            r#"{{"scenes": {{}}, "devices": {{"{id}": {{}}}}}}"#
        ));
        let config = load_config_from_path(path.to_str().unwrap());
        let _ = std::fs::remove_file(&path);
        let errors = error_texts(config.unwrap_err());
        assert!(
            errors.contains("is not a valid logical device id"),
            "for id {id:?}: {errors}"
        );
    }
}

/// The devices section must be an object of device ids.
#[test]
fn rejects_non_object_devices_section() {
    let path = write_temp_config(r#"{"scenes": {}, "devices": []}"#);
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(path);
    let errors = error_texts(config.unwrap_err());
    assert!(
        errors.contains("config \"devices\" must be an object of logical device ids"),
        "{errors}"
    );
}

/// A device definition missing required mapping fields is rejected with its id.
#[test]
fn rejects_malformed_device_mapping() {
    let path = write_temp_config(
        r#"{
            "scenes": {},
            "devices": {
                "1": { "device_id": "0300:3002" }
            }
        }"#,
    );
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(path);
    let errors = error_texts(config.unwrap_err());
    assert!(
        errors.contains("device \"1\" is not a valid device mapping"),
        "{errors}"
    );
    assert!(errors.contains("missing field"), "{errors}");
}

/// Every invalid device entry is reported, not just the first one.
#[test]
fn reports_all_invalid_devices() {
    let path = write_temp_config(
        r#"{
            "scenes": {},
            "devices": {
                "1": {},
                "5": "pad"
            }
        }"#,
    );
    let config = load_config_from_path(path.to_str().unwrap());
    let _ = std::fs::remove_file(path);
    let errors = error_texts(config.unwrap_err());
    assert!(
        errors.contains("device \"1\" is not a valid device mapping"),
        "{errors}"
    );
    assert!(
        errors.contains("device \"5\" is not a valid device mapping"),
        "{errors}"
    );
}
