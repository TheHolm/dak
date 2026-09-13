//! Tests for `SceneRunner` scene application using a recording mock instead of a
//! physical keypad: operations map to the right 0-based device keys, async
//! `text_exec`/`image_exec` results land on their buttons, and reassigning a button
//! kills the running program and draws the red "Error" label.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use dak::actions::{set_image_from_file, ButtonDevice, ExecEvent, ExecOutputKind, SceneRunner};
use dak::log::Log;
use image::{DynamicImage, GenericImageView, Rgb, RgbImage};
use mirajazz::types::{ImageFormat, ImageMirroring, ImageMode, ImageRotation};
use serde_json::{json, Value};

const FORMAT: ImageFormat = ImageFormat {
    mode: ImageMode::JPEG,
    size: (60, 60),
    rotation: ImageRotation::Rot0,
    mirror: ImageMirroring::None,
};

/// Recording mock of the device surface; stores every call for later assertion.
#[derive(Debug, PartialEq, Clone)]
enum Call {
    /// An image staged for a 0-based device key.
    SetImage(u8, DynamicImage),
    /// An empty image staged for a 0-based device key.
    ClearImage(u8),
    /// The staged images were sent to the LCDs.
    Flush,
}

#[derive(Default)]
struct MockButtonDevice {
    calls: Mutex<Vec<Call>>,
}

impl MockButtonDevice {
    /// Snapshot of every recorded call, in order.
    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    /// The kind of each recorded call, e.g. `["Flush"]`.
    fn kinds(&self, calls: &[Call]) -> Vec<&'static str> {
        calls
            .iter()
            .map(|call| match call {
                Call::SetImage(..) => "SetImage",
                Call::ClearImage(..) => "ClearImage",
                Call::Flush => "Flush",
            })
            .collect()
    }

    /// The 0-based device key of every image-bearing call, in order.
    fn keys(&self, calls: &[Call]) -> Vec<u8> {
        calls
            .iter()
            .filter_map(|call| match call {
                Call::SetImage(key, _) | Call::ClearImage(key) => Some(*key),
                Call::Flush => None,
            })
            .collect()
    }

    /// The most recently staged image for `key`, if any.
    fn last_image(&self, key: u8) -> Option<DynamicImage> {
        self.calls().into_iter().rev().find_map(|call| match call {
            Call::SetImage(k, image) if k == key => Some(image),
            _ => None,
        })
    }
}

impl ButtonDevice for MockButtonDevice {
    type Error = std::convert::Infallible;

    async fn set_button_image(
        &self,
        key: u8,
        _image_format: ImageFormat,
        image: DynamicImage,
    ) -> Result<(), Self::Error> {
        self.calls.lock().unwrap().push(Call::SetImage(key, image));
        Ok(())
    }

    async fn clear_button_image(&self, key: u8) -> Result<(), Self::Error> {
        self.calls.lock().unwrap().push(Call::ClearImage(key));
        Ok(())
    }

    async fn flush(&self) -> Result<(), Self::Error> {
        self.calls.lock().unwrap().push(Call::Flush);
        Ok(())
    }

    fn key_count(&self) -> u8 {
        9
    }
}

static TMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Writes a 4x4 test image to a unique temp file and returns its path.
fn write_temp_image() -> PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::SeqCst);
    let path = format!("/tmp/dak_runner_img_{}_{n}.png", std::process::id());
    let image = RgbImage::from_pixel(4, 4, Rgb([200, 100, 50]));
    image.save(&path).unwrap();
    PathBuf::from(path)
}

/// Writes `contents` to a unique temp file and returns its path.
fn write_temp_text(contents: &str) -> PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::SeqCst);
    let path = format!("/tmp/dak_runner_txt_{}_{n}.txt", std::process::id());
    std::fs::write(&path, contents).unwrap();
    PathBuf::from(path)
}

/// A config whose `main` scene carries the given numbered button operations.
fn scenes_with_buttons(buttons: Value) -> Value {
    json!({ "main": { "setup": buttons } })
}

/// True when `image` shows the red "Error" label (any red-dominant pixel).
fn is_red_label(image: &DynamicImage) -> bool {
    image
        .to_rgb8()
        .enumerate_pixels()
        .any(|(_, _, pixel)| pixel.0[0] > 200 && pixel.0[1] < 80 && pixel.0[2] < 80)
}

/// An `image` operation stages the file under the 0-based key and flushes it.
#[tokio::test]
async fn set_image_op_stages_zero_based_key_and_flushes() {
    let mock = MockButtonDevice::default();
    let image_path = write_temp_image();
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(1, &mock, FORMAT, tx, Log::default());

    let scenes = scenes_with_buttons(json!({
        "1b03": { "type": "image", "params": image_path.to_str().unwrap() }
    }));
    runner.enter_scene("main", &scenes).await.unwrap();

    let calls = mock.calls();
    assert_eq!(mock.kinds(&calls), ["SetImage", "Flush"]);
    assert_eq!(mock.keys(&calls), [2]);
    let image = mock.last_image(2).expect("image was not staged");
    assert_eq!(image.dimensions(), (4, 4));
    assert_eq!(image.to_rgb8().get_pixel(2, 2).0, [200, 100, 50]);
    let _ = std::fs::remove_file(&image_path);
}

/// A `text` operation renders the file's first lines and stages the button image.
#[tokio::test]
async fn text_op_renders_button_text_and_flushes() {
    let mock = MockButtonDevice::default();
    let text_path = write_temp_text("hello\nworld");
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(1, &mock, FORMAT, tx, Log::default());

    let scenes = scenes_with_buttons(json!({
        "1b01": { "type": "text", "params": text_path.to_str().unwrap() }
    }));
    runner.enter_scene("main", &scenes).await.unwrap();

    let calls = mock.calls();
    assert_eq!(mock.kinds(&calls), ["SetImage", "Flush"]);
    assert_eq!(mock.keys(&calls), [0]);
    let image = mock.last_image(0).expect("text was not staged");
    assert_eq!(image.dimensions(), (60, 60));
    let _ = std::fs::remove_file(&text_path);
}

/// A `clear` operation stages the empty image under the 0-based key and flushes.
#[tokio::test]
async fn clear_op_calls_zero_based_key_and_flushes() {
    let mock = MockButtonDevice::default();
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(1, &mock, FORMAT, tx, Log::default());

    let scenes = scenes_with_buttons(json!({ "1b05": { "type": "clear" } }));
    runner.enter_scene("main", &scenes).await.unwrap();

    let calls = mock.calls();
    assert_eq!(mock.kinds(&calls), ["ClearImage", "Flush"]);
    assert_eq!(mock.keys(&calls), [4]);
}

/// Unknown commands touch no button; only the final flush runs.
#[tokio::test]
async fn unsupported_op_only_flushes() {
    let mock = MockButtonDevice::default();
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(1, &mock, FORMAT, tx, Log::default());

    let scenes = scenes_with_buttons(json!({
        "1b01": { "type": "frobnicate", "params": "/bin/sh" }
    }));
    runner.enter_scene("main", &scenes).await.unwrap();

    assert_eq!(mock.kinds(&mock.calls()), ["Flush"]);
}

/// The output of a current-generation `text_exec` is rendered on its button.
#[tokio::test]
async fn text_exec_output_drawn_on_button() {
    let mock = MockButtonDevice::default();
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(1, &mock, FORMAT, tx, Log::default());

    // Spawn a slow program so its own result cannot race our explicit event; the
    // first apply on key 2 reserves cancel-bump 1, then the task runs at generation 2.
    let scenes = scenes_with_buttons(
        json!({ "1b02": { "type": "text_exec", "params": "/usr/bin/sleep 10" } }),
    );
    runner.enter_scene("main", &scenes).await.unwrap();

    runner
        .handle_exec_event(ExecEvent::Output {
            key: 2,
            generation: 2,
            kind: ExecOutputKind::Text,
            stdout: b"demo\n".to_vec(),
        })
        .await;

    let calls = mock.calls();
    assert_eq!(mock.kinds(&calls), ["Flush", "SetImage", "Flush"]);
    assert_eq!(mock.keys(&calls), [1]);
    let image = mock.last_image(1).expect("output was not staged");
    assert_eq!(image.dimensions(), (60, 60));
}

/// A stale `text_exec` output (button was reassigned meanwhile) is discarded.
#[tokio::test]
async fn text_exec_stale_output_dropped() {
    let mock = MockButtonDevice::default();
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(1, &mock, FORMAT, tx, Log::default());

    let scenes = scenes_with_buttons(
        json!({ "1b02": { "type": "text_exec", "params": "/usr/bin/sleep 10" } }),
    );
    runner.enter_scene("main", &scenes).await.unwrap();

    runner
        .handle_exec_event(ExecEvent::Output {
            key: 2,
            generation: 1,
            kind: ExecOutputKind::Text,
            stdout: b"stale\n".to_vec(),
        })
        .await;

    assert_eq!(mock.kinds(&mock.calls()), ["Flush"]);
}

/// The image output of a current-generation `image_exec` is set on its button.
#[tokio::test]
async fn image_exec_output_drawn_on_button() {
    let mock = MockButtonDevice::default();
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(1, &mock, FORMAT, tx, Log::default());

    let scenes = scenes_with_buttons(
        json!({ "1b02": { "type": "image_exec", "params": "/usr/bin/sleep 10" } }),
    );
    runner.enter_scene("main", &scenes).await.unwrap();

    let mut png = Vec::new();
    RgbImage::from_pixel(60, 60, Rgb([10, 200, 30]))
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .expect("encoding png failed");
    runner
        .handle_exec_event(ExecEvent::Output {
            key: 2,
            generation: 2,
            kind: ExecOutputKind::Image,
            stdout: png,
        })
        .await;

    let calls = mock.calls();
    assert_eq!(mock.kinds(&calls), ["Flush", "SetImage", "Flush"]);
    assert_eq!(mock.keys(&calls), [1]);
    let image = mock.last_image(1).expect("output image was not staged");
    assert_eq!(image.dimensions(), (60, 60));
    assert_eq!(image.to_rgb8().get_pixel(30, 30).0, [10, 200, 30]);
}

/// A current-generation `image_exec` program not emitting an image draws "Error".
#[tokio::test]
async fn image_exec_non_image_output_draws_error_label() {
    let mock = MockButtonDevice::default();
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(1, &mock, FORMAT, tx, Log::default());

    let scenes = scenes_with_buttons(
        json!({ "1b02": { "type": "image_exec", "params": "/usr/bin/sleep 10" } }),
    );
    runner.enter_scene("main", &scenes).await.unwrap();

    runner
        .handle_exec_event(ExecEvent::Output {
            key: 2,
            generation: 2,
            kind: ExecOutputKind::Image,
            stdout: b"not an image".to_vec(),
        })
        .await;

    let calls = mock.calls();
    assert_eq!(mock.kinds(&calls), ["Flush", "SetImage", "Flush"]);
    let image = mock.last_image(1).expect("error label was not staged");
    assert!(is_red_label(&image), "error label should be red");
}

/// Non-UTF-8 `text_exec` output is not rendered as text but draws "Error".
#[tokio::test]
async fn text_exec_non_utf8_output_draws_error_label() {
    let mock = MockButtonDevice::default();
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(1, &mock, FORMAT, tx, Log::default());

    let scenes = scenes_with_buttons(
        json!({ "1b02": { "type": "text_exec", "params": "/usr/bin/sleep 10" } }),
    );
    runner.enter_scene("main", &scenes).await.unwrap();

    runner
        .handle_exec_event(ExecEvent::Output {
            key: 2,
            generation: 2,
            kind: ExecOutputKind::Text,
            stdout: b"\xff\xfe".to_vec(),
        })
        .await;

    let calls = mock.calls();
    assert_eq!(mock.kinds(&calls), ["Flush", "SetImage", "Flush"]);
    let image = mock.last_image(1).expect("error label was not staged");
    assert!(is_red_label(&image), "error label should be red");
}

/// A failed current-generation `text_exec` draws the red "Error" label.
#[tokio::test]
async fn text_exec_error_draws_red_label() {
    let mock = MockButtonDevice::default();
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(1, &mock, FORMAT, tx, Log::default());

    let scenes = scenes_with_buttons(
        json!({ "1b02": { "type": "text_exec", "params": "/usr/bin/sleep 10" } }),
    );
    runner.enter_scene("main", &scenes).await.unwrap();

    runner
        .handle_exec_event(ExecEvent::Error {
            key: 2,
            generation: 2,
            error: "something went wrong".to_string(),
        })
        .await;

    let calls = mock.calls();
    assert_eq!(mock.kinds(&calls), ["Flush", "SetImage", "Flush"]);
    let image = mock.last_image(1).expect("error label was not staged");
    assert!(is_red_label(&image), "error label should be red");
}

/// A stale `text_exec` error (button was reassigned meanwhile) is discarded.
#[tokio::test]
async fn text_exec_stale_error_dropped() {
    let mock = MockButtonDevice::default();
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(1, &mock, FORMAT, tx, Log::default());

    let scenes = scenes_with_buttons(
        json!({ "1b02": { "type": "text_exec", "params": "/usr/bin/sleep 10" } }),
    );
    runner.enter_scene("main", &scenes).await.unwrap();

    runner
        .handle_exec_event(ExecEvent::Error {
            key: 2,
            generation: 1,
            error: "stale failure".to_string(),
        })
        .await;

    assert_eq!(mock.kinds(&mock.calls()), ["Flush"]);
}

/// Reassigning a button while its `text_exec` runs kills the program and draws "Error".
#[tokio::test]
async fn reassigning_key_kills_running_text_exec_and_draws_error() {
    let mock = MockButtonDevice::default();
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(1, &mock, FORMAT, tx, Log::default());

    let pid_file = format!("/tmp/dak_runner_pid_{}", std::process::id());
    let _ = std::fs::remove_file(&pid_file);
    let scenes = scenes_with_buttons(json!({
        "1b02": {
            "type": "text_exec",
            "params": format!("/bin/sh -c 'echo $$ > {pid_file}; exec sleep 60'")
        }
    }));
    runner.enter_scene("main", &scenes).await.unwrap();

    let pid = {
        let mut pid = None;
        for _ in 0..200 {
            if let Ok(content) = std::fs::read_to_string(&pid_file) {
                if let Ok(parsed) = content.trim().parse::<i32>() {
                    pid = Some(parsed);
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        pid.expect("program did not write its pid file")
    };
    assert!(
        std::path::Path::new(&format!("/proc/{pid}")).exists(),
        "sanity check: process {pid} should be running"
    );

    let reassigned = scenes_with_buttons(json!({ "1b02": { "type": "clear" } }));
    runner.enter_scene("main", &reassigned).await.unwrap();

    let mut gone = false;
    for _ in 0..100 {
        if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(gone, "process {pid} still alive after reassignment");

    let calls = mock.calls();
    // Initial apply flushed; the reassignment then drew the error label, cleared and flushed.
    assert_eq!(
        mock.kinds(&calls),
        ["Flush", "SetImage", "Flush", "ClearImage", "Flush"]
    );
    let image = mock.last_image(1).expect("error label was not staged");
    assert!(is_red_label(&image), "error label should be red");
    let _ = std::fs::remove_file(&pid_file);
}

/// Reassigning a button after its `text_exec` already finished draws no error.
#[tokio::test]
async fn reassigning_key_after_finished_text_exec_draws_no_error() {
    let mock = MockButtonDevice::default();
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(1, &mock, FORMAT, tx, Log::default());

    let scenes =
        scenes_with_buttons(json!({ "1b02": { "type": "text_exec", "params": "/bin/true" } }));
    runner.enter_scene("main", &scenes).await.unwrap();
    // The task's last action is sending its output; after it is received the task
    // finishes, so the subsequent reassignment sees a completed handle.
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("timed out waiting for the text_exec output")
        .expect("channel closed");
    tokio::time::sleep(Duration::from_millis(100)).await;

    let reassigned = scenes_with_buttons(json!({ "1b02": { "type": "clear" } }));
    runner.enter_scene("main", &reassigned).await.unwrap();

    let calls = mock.calls();
    assert_eq!(mock.kinds(&calls), ["Flush", "ClearImage", "Flush"]);
    assert!(mock.last_image(1).is_none(), "no error label expected");
}

/// `clear_changed_button_images` restores exactly the buttons the session changed, in
/// 0-based device keys, leaving untouched buttons alone.
#[tokio::test]
async fn clear_changed_button_images_restores_only_changed_buttons() {
    let mock = MockButtonDevice::default();
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(1, &mock, FORMAT, tx, Log::default());

    let image_path = write_temp_image();
    let scenes = scenes_with_buttons(json!({
        "1b06": { "type": "image", "params": image_path.to_str().unwrap() },
        "1b01": { "type": "clear" }
    }));
    runner.enter_scene("main", &scenes).await.unwrap();

    runner.clear_changed_button_images().await.unwrap();

    let calls = mock.calls();
    assert_eq!(
        mock.kinds(&calls),
        [
            "ClearImage",
            "SetImage",
            "Flush",
            "ClearImage",
            "ClearImage",
            "Flush"
        ]
    );
    assert_eq!(mock.keys(&calls), [0, 5, 0, 5]);
    let _ = std::fs::remove_file(&image_path);
}

/// `set_image_from_file` stages the image under the 0-based device key without flushing;
/// the flush happens once at the scene level.
#[tokio::test]
async fn set_image_from_file_stages_image_without_flushing() {
    let mock = MockButtonDevice::default();
    let image_path = write_temp_image();

    set_image_from_file(&mock, 3, FORMAT, image_path.to_str().unwrap())
        .await
        .expect("staging failed");

    let calls = mock.calls();
    assert_eq!(mock.kinds(&calls), ["SetImage"]);
    assert_eq!(mock.keys(&calls), [3]);
    let _ = std::fs::remove_file(&image_path);
}

/// A `launch` operation spawns the program detached and touches no button: only the
/// scene-level flush reaches the device, the launched process starts on its own, and
/// termination cleanup leaves nothing to restore.
#[tokio::test]
async fn launch_op_spawns_program_and_touches_no_button() {
    let mock = MockButtonDevice::default();
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(1, &mock, FORMAT, tx, Log::default());

    let n = TMP_COUNTER.fetch_add(1, Ordering::SeqCst);
    let pid_file = format!("/tmp/dak_runner_launch_{}_{n}.pid", std::process::id());
    let done_file = format!("/tmp/dak_runner_launch_{}_{n}.done", std::process::id());
    let _ = std::fs::remove_file(&pid_file);
    let _ = std::fs::remove_file(&done_file);

    let scenes = scenes_with_buttons(json!({
        "1b01": {
            "type": "launch",
            "params": json!(format!(
                "/bin/sh -c 'echo $$ > {pid_file}; sleep 2; echo done > {done_file}'"
            ))
        }
    }));
    runner.enter_scene("main", &scenes).await.unwrap();

    // The launch draws nothing; only the scene-level flush runs.
    assert_eq!(mock.kinds(&mock.calls()), ["Flush"]);

    // Termination cleanup restores only the flush, because launch touched no button.
    runner.clear_changed_button_images().await.unwrap();
    assert_eq!(mock.kinds(&mock.calls()), ["Flush", "Flush"]);

    // The program ran to completion on its own; it was neither waited on nor killed
    // by the scene application or the cleanup.
    let mut completed = false;
    for _ in 0..500 {
        if std::path::Path::new(&done_file).exists() {
            completed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(completed, "launched program did not run to completion");

    let _ = std::fs::remove_file(&pid_file);
    let _ = std::fs::remove_file(&done_file);
}

/// A `text` operation with a missing file fails the whole scene application.
#[tokio::test]
async fn text_op_missing_file_fails_scene() {
    let mock = MockButtonDevice::default();
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(1, &mock, FORMAT, tx, Log::default());

    let scenes = scenes_with_buttons(json!({
        "1b01": { "type": "text", "params": "/does/not/exist.txt" }
    }));
    let error = runner.enter_scene("main", &scenes).await.unwrap_err();
    assert!(
        !error.to_string().is_empty(),
        "expected a descriptive error"
    );
}

/// An `image` operation with a missing file fails the whole scene application.
#[tokio::test]
async fn image_op_missing_file_fails_scene() {
    let mock = MockButtonDevice::default();
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(1, &mock, FORMAT, tx, Log::default());

    let scenes = scenes_with_buttons(json!({
        "1b01": { "type": "image", "params": "/does/not/exist.png" }
    }));
    let error = runner.enter_scene("main", &scenes).await.unwrap_err();
    assert!(
        !error.to_string().is_empty(),
        "expected a descriptive error"
    );
}

/// Operations referencing another device's number are skipped with a notice; nothing
/// is drawn, only the scene-level flush runs.
#[tokio::test]
async fn ops_for_absent_device_are_skipped() {
    let mock = MockButtonDevice::default();
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(1, &mock, FORMAT, tx, Log::default());

    let scenes = scenes_with_buttons(json!({
        "2b01": { "type": "clear" }
    }));
    runner.enter_scene("main", &scenes).await.unwrap();

    assert_eq!(mock.kinds(&mock.calls()), ["Flush"]);
}

/// Encoder references are parsed but not driven yet: the operation is skipped with a
/// notice, only the scene-level flush runs.
#[tokio::test]
async fn encoder_ops_are_skipped_as_unsupported() {
    let mock = MockButtonDevice::default();
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(1, &mock, FORMAT, tx, Log::default());

    let scenes = scenes_with_buttons(json!({
        "1e01": { "type": "clear" }
    }));
    runner.enter_scene("main", &scenes).await.unwrap();

    assert_eq!(mock.kinds(&mock.calls()), ["Flush"]);
}

/// Buttons beyond the device's physical count are skipped with a notice; nothing is
/// drawn, only the scene-level flush runs.
#[tokio::test]
async fn out_of_range_buttons_are_skipped() {
    let mock = MockButtonDevice::default();
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(1, &mock, FORMAT, tx, Log::default());

    let scenes = scenes_with_buttons(json!({
        "1b99": { "type": "clear" }
    }));
    runner.enter_scene("main", &scenes).await.unwrap();

    assert_eq!(mock.kinds(&mock.calls()), ["Flush"]);
}
