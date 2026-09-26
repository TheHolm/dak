//! Tests for `SceneRunner` scene application using a recording mock instead of a
//! physical keypad: operations map to the right 0-based device keys, async
//! `text_exec`/`image_exec` results land on their buttons, and reassigning a button
//! kills the running program and draws the red "Error" label.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

// See src/lib.rs for why this is needed on FreeBSD only: each integration test file
// compiles as its own crate, so it needs its own copy of the rename.
#[cfg(target_os = "freebsd")]
extern crate mirajazz_freebsd as mirajazz;

use dak::actions::{set_image_from_file, ButtonDevice, ExecEvent, ExecOutputKind, SceneRunner};
use dak::color::Color;
use dak::log::Log;
use dak::press::Defaults;
use dak::variables::{VarDef, VarValue, Variables};
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
    /// `set_brightness` was called with this percent.
    SetBrightness(u8),
    /// `set_led_brightness` was called with this percent.
    SetLedBrightness(u8),
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
                Call::SetBrightness(_) => "SetBrightness",
                Call::SetLedBrightness(_) => "SetLedBrightness",
            })
            .collect()
    }

    /// The 0-based device key of every image-bearing call, in order.
    fn keys(&self, calls: &[Call]) -> Vec<u8> {
        calls
            .iter()
            .filter_map(|call| match call {
                Call::SetImage(key, _) | Call::ClearImage(key) => Some(*key),
                Call::Flush | Call::SetBrightness(_) | Call::SetLedBrightness(_) => None,
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

    async fn set_brightness(&self, percent: u8) -> Result<(), Self::Error> {
        self.calls
            .lock()
            .unwrap()
            .push(Call::SetBrightness(percent));
        Ok(())
    }

    async fn set_led_brightness(&self, percent: u8) -> Result<(), Self::Error> {
        self.calls
            .lock()
            .unwrap()
            .push(Call::SetLedBrightness(percent));
        Ok(())
    }
}

/// The error type of [`FailingButtonDevice`]: a fixed message describing the failure.
#[derive(Debug)]
struct WriteError(&'static str);

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for WriteError {}

/// A device mock whose image writes, flushes, and brightness calls can fail on
/// demand, for exercising the error-logging branches of the scene runner. Every
/// operation is recorded so tests can assert the failing branch actually ran.
#[derive(Default)]
struct FailingButtonDevice {
    fail_set_image: AtomicBool,
    fail_clear_image: AtomicBool,
    fail_flush: AtomicBool,
    fail_brightness: AtomicBool,
    attempts: Mutex<Vec<&'static str>>,
}

impl FailingButtonDevice {
    /// Snapshot of every attempted operation, in order.
    fn attempts(&self) -> Vec<&'static str> {
        self.attempts.lock().unwrap().clone()
    }

    /// Makes every future `set_button_image` call fail.
    fn fail_image_writes(&self, yes: bool) {
        self.fail_set_image.store(yes, Ordering::SeqCst);
    }

    /// Makes every future `clear_button_image` call fail.
    fn fail_clear_writes(&self, yes: bool) {
        self.fail_clear_image.store(yes, Ordering::SeqCst);
    }

    /// Makes every future `flush` call fail.
    fn fail_flushes(&self, yes: bool) {
        self.fail_flush.store(yes, Ordering::SeqCst);
    }

    /// Makes every future `set_brightness`/`set_led_brightness` call fail.
    fn fail_brightness_calls(&self, yes: bool) {
        self.fail_brightness.store(yes, Ordering::SeqCst);
    }
}

impl ButtonDevice for FailingButtonDevice {
    type Error = WriteError;

    async fn set_button_image(
        &self,
        _key: u8,
        _image_format: ImageFormat,
        _image: DynamicImage,
    ) -> Result<(), Self::Error> {
        self.attempts.lock().unwrap().push("SetImage");
        if self.fail_set_image.load(Ordering::SeqCst) {
            Err(WriteError("device write refused"))
        } else {
            Ok(())
        }
    }

    async fn clear_button_image(&self, _key: u8) -> Result<(), Self::Error> {
        self.attempts.lock().unwrap().push("ClearImage");
        if self.fail_clear_image.load(Ordering::SeqCst) {
            Err(WriteError("device clear refused"))
        } else {
            Ok(())
        }
    }

    async fn flush(&self) -> Result<(), Self::Error> {
        self.attempts.lock().unwrap().push("Flush");
        if self.fail_flush.load(Ordering::SeqCst) {
            Err(WriteError("device flush refused"))
        } else {
            Ok(())
        }
    }

    fn key_count(&self) -> u8 {
        9
    }

    async fn set_brightness(&self, _percent: u8) -> Result<(), Self::Error> {
        self.attempts.lock().unwrap().push("SetBrightness");
        if self.fail_brightness.load(Ordering::SeqCst) {
            Err(WriteError("device brightness write refused"))
        } else {
            Ok(())
        }
    }

    async fn set_led_brightness(&self, _percent: u8) -> Result<(), Self::Error> {
        self.attempts.lock().unwrap().push("SetLedBrightness");
        if self.fail_brightness.load(Ordering::SeqCst) {
            Err(WriteError("device LED brightness write refused"))
        } else {
            Ok(())
        }
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

/// Writes a 4x4 fully transparent PNG (white under zero alpha) to a unique temp file
/// and returns its path, for checking that transparent pixels pick up the background.
fn write_transparent_temp_image() -> PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::SeqCst);
    let path = format!("/tmp/dak_runner_alpha_{}_{n}.png", std::process::id());
    image::RgbaImage::from_pixel(4, 4, image::Rgba([255, 255, 255, 0]))
        .save(&path)
        .unwrap();
    PathBuf::from(path)
}

/// Encodes a 60x60 green PNG, used as fake `image_exec` output for exec tests.
fn green_png_bytes() -> Vec<u8> {
    let mut png = Vec::new();
    RgbImage::from_pixel(60, 60, Rgb([10, 200, 30]))
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .expect("encoding png failed");
    png
}

/// Encodes a 4x4 fully transparent PNG (white under zero alpha), used as fake
/// `image_exec` output to check compositing.
fn transparent_png_bytes() -> Vec<u8> {
    let mut png = Vec::new();
    image::RgbaImage::from_pixel(4, 4, image::Rgba([255, 255, 255, 0]))
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .expect("encoding png failed");
    png
}

/// Writes `contents` to a unique temp file and returns its path.
fn write_temp_text(contents: &str) -> PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::SeqCst);
    let path = format!("/tmp/dak_runner_txt_{}_{n}.txt", std::process::id());
    std::fs::write(&path, contents).unwrap();
    PathBuf::from(path)
}

/// Whether a process with the given pid is still running, checked the portable POSIX
/// way (`kill -0`, which every unix delivers no signal for but still reports ESRCH if
/// the pid is gone) rather than via `/proc`, which Linux mounts by default but
/// FreeBSD does not. Stdio is silenced: a gone pid is the expected, common case while
/// polling below, not something `kill`'s own "No such process" message should narrate.
fn is_process_alive(pid: i32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
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
    let (refresh_tx, mut _refresh_rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(
        1,
        &mock,
        FORMAT,
        tx,
        refresh_tx,
        Log::default(),
        &HashSet::new(),
        Defaults::default().background,
        Defaults::default().text_color,
    );

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

/// A transparent image is composited onto the runner's default background before it is
/// staged, so the white hidden under the alpha never reaches the device.
#[tokio::test]
async fn transparent_image_uses_default_background() {
    let mock = MockButtonDevice::default();
    let image_path = write_transparent_temp_image();
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let (refresh_tx, mut _refresh_rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(
        1,
        &mock,
        FORMAT,
        tx,
        refresh_tx,
        Log::default(),
        &HashSet::new(),
        Defaults::default().background,
        Defaults::default().text_color,
    );

    let scenes = scenes_with_buttons(json!({
        "1b01": { "type": "image", "params": image_path.to_str().unwrap() }
    }));
    runner.enter_scene("main", &scenes).await.unwrap();

    let image = mock.last_image(0).expect("image was not staged").to_rgb8();
    assert_eq!(image.get_pixel(0, 0).0, [0x00, 0x00, 0x00]);
    let _ = std::fs::remove_file(&image_path);
}

/// `SceneRunner::set_text_color` changes the glyph colour used for the next text draw.
#[tokio::test]
async fn set_text_color_affects_next_draw() {
    let mock = MockButtonDevice::default();
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let (refresh_tx, mut _refresh_rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(
        1,
        &mock,
        FORMAT,
        tx,
        refresh_tx,
        Log::default(),
        &HashSet::new(),
        Defaults::default().background,
        Defaults::default().text_color,
    );
    runner.set_text_color(Color::parse("lime").unwrap());

    let scenes = scenes_with_buttons(json!({
        "1b01": { "type": "text_value", "params": "M" }
    }));
    runner.enter_scene("main", &scenes).await.unwrap();

    let image = mock.last_image(0).expect("image was not staged").to_rgb8();
    assert!(
        image
            .pixels()
            .any(|p| p.0[1] > 200 && p.0[0] < 60 && p.0[2] < 60),
        "expected a lime glyph pixel"
    );
}

/// A per-button `background` overrides the global default for that button's transparent
/// image, and the override is per button, not per scene.
#[tokio::test]
async fn per_button_background_override_is_used() {
    let mock = MockButtonDevice::default();
    let image_path = write_transparent_temp_image();
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let (refresh_tx, mut _refresh_rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(
        1,
        &mock,
        FORMAT,
        tx,
        refresh_tx,
        Log::default(),
        &HashSet::new(),
        Defaults::default().background,
        Defaults::default().text_color,
    );

    let scenes = scenes_with_buttons(json!({
        "1b01": { "type": "image", "params": image_path.to_str().unwrap(), "background": "#ff0000" }
    }));
    runner.enter_scene("main", &scenes).await.unwrap();

    let image = mock.last_image(0).expect("image was not staged").to_rgb8();
    assert_eq!(image.get_pixel(0, 0).0, [0xff, 0x00, 0x00]);
    let _ = std::fs::remove_file(&image_path);
}

/// A dynamically-resolved per-button colour that is not a colour falls back to the
/// global default with a warning instead of failing the scene.
#[tokio::test]
async fn invalid_dynamic_background_falls_back_to_default() {
    let mock = MockButtonDevice::default();
    let image_path = write_transparent_temp_image();
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let (refresh_tx, mut _refresh_rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(
        1,
        &mock,
        FORMAT,
        tx,
        refresh_tx,
        Log::default(),
        &HashSet::new(),
        Color::rgb(0x11, 0x22, 0x33),
        Defaults::default().text_color,
    );

    let scenes = scenes_with_buttons(json!({
        "1b01": { "type": "image", "params": image_path.to_str().unwrap(), "background": "not-a-colour" }
    }));
    // A literal would be rejected at load; feed the runner directly to model a runtime
    // value that slipped through (e.g. from a `$` reference).
    runner.enter_scene("main", &scenes).await.unwrap();

    let image = mock.last_image(0).expect("image was not staged").to_rgb8();
    assert_eq!(image.get_pixel(0, 0).0, [0x11, 0x22, 0x33]);
    let _ = std::fs::remove_file(&image_path);
}

/// `SceneRunner::set_background` changes the colour used for the next draw, so a runtime
/// `$defaults.background` assignment takes effect without repainting existing buttons.
#[tokio::test]
async fn set_background_affects_next_draw() {
    let mock = MockButtonDevice::default();
    let image_path = write_transparent_temp_image();
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let (refresh_tx, mut _refresh_rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(
        1,
        &mock,
        FORMAT,
        tx,
        refresh_tx,
        Log::default(),
        &HashSet::new(),
        Defaults::default().background,
        Defaults::default().text_color,
    );
    runner.set_background(Color::parse("red").unwrap());

    let scenes = scenes_with_buttons(json!({
        "1b01": { "type": "image", "params": image_path.to_str().unwrap() }
    }));
    runner.enter_scene("main", &scenes).await.unwrap();

    let image = mock.last_image(0).expect("image was not staged").to_rgb8();
    assert_eq!(image.get_pixel(0, 0).0, [0xff, 0x00, 0x00]);
    let _ = std::fs::remove_file(&image_path);
}

/// A `text_value` button is drawn with its configured background and text colour.
#[tokio::test]
async fn text_value_uses_configured_colours() {
    let mock = MockButtonDevice::default();
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let (refresh_tx, mut _refresh_rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(
        1,
        &mock,
        FORMAT,
        tx,
        refresh_tx,
        Log::default(),
        &HashSet::new(),
        Defaults::default().background,
        Defaults::default().text_color,
    );

    let scenes = scenes_with_buttons(json!({
        "1b01": { "type": "text_value", "params": "M", "background": "#0000ff", "text_color": "#00ff00" }
    }));
    runner.enter_scene("main", &scenes).await.unwrap();

    let image = mock.last_image(0).expect("image was not staged").to_rgb8();
    assert_eq!(image.get_pixel(0, 0).0, [0, 0, 255]);
    assert!(
        image
            .pixels()
            .any(|p| p.0[1] > 200 && p.0[0] < 60 && p.0[2] < 60),
        "expected a green glyph pixel"
    );
}

/// `SceneRunner::set_button_brightness` delegates straight to the device's
/// `set_brightness`, with no image staging/flushing involved.
#[tokio::test]
async fn set_button_brightness_delegates_to_device() {
    let mock = MockButtonDevice::default();
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let (refresh_tx, mut _refresh_rx) = tokio::sync::mpsc::channel(4);
    let runner = SceneRunner::new(
        1,
        &mock,
        FORMAT,
        tx,
        refresh_tx,
        Log::default(),
        &HashSet::new(),
        Defaults::default().background,
        Defaults::default().text_color,
    );

    runner.set_button_brightness(80).await.unwrap();

    assert_eq!(mock.calls(), vec![Call::SetBrightness(80)]);
}

/// `SceneRunner::set_encoder_brightness` delegates to `set_led_brightness` instead,
/// distinctly from `set_button_brightness` above.
#[tokio::test]
async fn set_encoder_brightness_delegates_to_device() {
    let mock = MockButtonDevice::default();
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let (refresh_tx, mut _refresh_rx) = tokio::sync::mpsc::channel(4);
    let runner = SceneRunner::new(
        1,
        &mock,
        FORMAT,
        tx,
        refresh_tx,
        Log::default(),
        &HashSet::new(),
        Defaults::default().background,
        Defaults::default().text_color,
    );

    runner.set_encoder_brightness(15).await.unwrap();

    assert_eq!(mock.calls(), vec![Call::SetLedBrightness(15)]);
}

/// Both brightness setters propagate the device's error instead of swallowing it,
/// mirroring how every other `SceneRunner` operation reports device failures.
#[tokio::test]
async fn brightness_setters_propagate_device_errors() {
    let device = FailingButtonDevice::default();
    device.fail_brightness_calls(true);
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let (refresh_tx, mut _refresh_rx) = tokio::sync::mpsc::channel(4);
    let runner = SceneRunner::new(
        1,
        &device,
        FORMAT,
        tx,
        refresh_tx,
        Log::default(),
        &HashSet::new(),
        Defaults::default().background,
        Defaults::default().text_color,
    );

    assert!(runner.set_button_brightness(80).await.is_err());
    assert!(runner.set_encoder_brightness(15).await.is_err());
    assert_eq!(device.attempts(), vec!["SetBrightness", "SetLedBrightness"]);
}

/// A `text` operation renders the file's first lines and stages the button image.
#[tokio::test]
async fn text_op_renders_button_text_and_flushes() {
    let mock = MockButtonDevice::default();
    let text_path = write_temp_text("hello\nworld");
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let (refresh_tx, mut _refresh_rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(
        1,
        &mock,
        FORMAT,
        tx,
        refresh_tx,
        Log::default(),
        &HashSet::new(),
        Defaults::default().background,
        Defaults::default().text_color,
    );

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

/// A `text` operation reading from an infinite source (`/dev/zero`) does not hang or
/// grow memory without bound: the read is capped at `MAX_TEXT_FILE_BYTES`, so it
/// completes quickly and successfully (null bytes are valid UTF-8; there is nothing
/// here to fail on) instead of blocking forever waiting for an EOF `/dev/zero` never
/// produces.
#[tokio::test]
async fn text_op_from_dev_zero_completes_quickly_instead_of_hanging() {
    let mock = MockButtonDevice::default();
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let (refresh_tx, mut _refresh_rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(
        1,
        &mock,
        FORMAT,
        tx,
        refresh_tx,
        Log::default(),
        &HashSet::new(),
        Defaults::default().background,
        Defaults::default().text_color,
    );

    let scenes = scenes_with_buttons(json!({
        "1b01": { "type": "text", "params": "/dev/zero" }
    }));
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        runner.enter_scene("main", &scenes),
    )
    .await
    .expect("reading /dev/zero must not hang past MAX_TEXT_FILE_BYTES")
    .unwrap();

    // A capped read of null bytes is not a failure - the button draws (a boring,
    // effectively blank) image rather than the red "Error" label.
    let calls = mock.calls();
    assert_eq!(mock.kinds(&calls), ["SetImage", "Flush"]);
}

/// A `clear` operation stages the empty image under the 0-based key and flushes.
#[tokio::test]
async fn clear_op_calls_zero_based_key_and_flushes() {
    let mock = MockButtonDevice::default();
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let (refresh_tx, mut _refresh_rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(
        1,
        &mock,
        FORMAT,
        tx,
        refresh_tx,
        Log::default(),
        &HashSet::new(),
        Defaults::default().background,
        Defaults::default().text_color,
    );

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
    let (refresh_tx, mut _refresh_rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(
        1,
        &mock,
        FORMAT,
        tx,
        refresh_tx,
        Log::default(),
        &HashSet::new(),
        Defaults::default().background,
        Defaults::default().text_color,
    );

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
    let (refresh_tx, mut _refresh_rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(
        1,
        &mock,
        FORMAT,
        tx,
        refresh_tx,
        Log::default(),
        &HashSet::new(),
        Defaults::default().background,
        Defaults::default().text_color,
    );

    // Spawn a slow program so its own result cannot race our explicit event; the
    // first apply on key 2 reserves cancel-bump 1, then the task runs at generation 2.
    let scenes =
        scenes_with_buttons(json!({ "1b02": { "type": "text_exec", "params": "sleep 10" } }));
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

/// Two `text_exec`/`image_exec` entries in the same scene do not block each other:
/// starting one does not wait for the other (or for either program) to finish.
/// `start_exec_task` only spawns the program and returns immediately, so applying both
/// operations - and the whole scene - completes almost instantly even though both
/// spawned commands are still sleeping.
#[tokio::test]
async fn text_exec_and_image_exec_entries_do_not_block_each_other() {
    let mock = MockButtonDevice::default();
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let (refresh_tx, mut _refresh_rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(
        1,
        &mock,
        FORMAT,
        tx,
        refresh_tx,
        Log::default(),
        &HashSet::new(),
        Defaults::default().background,
        Defaults::default().text_color,
    );

    let scenes = scenes_with_buttons(json!({
        "1b01": { "type": "text_exec", "params": "sleep 5" },
        "1b02": { "type": "image_exec", "params": "sleep 5" }
    }));
    tokio::time::timeout(
        std::time::Duration::from_millis(500),
        runner.enter_scene("main", &scenes),
    )
    .await
    .expect("applying both exec operations must not wait on either sleeping program")
    .unwrap();
}

/// A stale `text_exec` output (button was reassigned meanwhile) is discarded.
#[tokio::test]
async fn text_exec_stale_output_dropped() {
    let mock = MockButtonDevice::default();
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let (refresh_tx, mut _refresh_rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(
        1,
        &mock,
        FORMAT,
        tx,
        refresh_tx,
        Log::default(),
        &HashSet::new(),
        Defaults::default().background,
        Defaults::default().text_color,
    );

    let scenes =
        scenes_with_buttons(json!({ "1b02": { "type": "text_exec", "params": "sleep 10" } }));
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
    let (refresh_tx, mut _refresh_rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(
        1,
        &mock,
        FORMAT,
        tx,
        refresh_tx,
        Log::default(),
        &HashSet::new(),
        Defaults::default().background,
        Defaults::default().text_color,
    );

    let scenes =
        scenes_with_buttons(json!({ "1b02": { "type": "image_exec", "params": "sleep 10" } }));
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

/// An `image_exec` result is composited onto the button's configured background, so a
/// transparent program output does not show the white hidden under the alpha.
#[tokio::test]
async fn image_exec_output_uses_configured_background() {
    let mock = MockButtonDevice::default();
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let (refresh_tx, mut _refresh_rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(
        1,
        &mock,
        FORMAT,
        tx,
        refresh_tx,
        Log::default(),
        &HashSet::new(),
        Defaults::default().background,
        Defaults::default().text_color,
    );

    let scenes = scenes_with_buttons(json!({
        "1b02": { "type": "image_exec", "params": "sleep 10", "background": "#ff0000" }
    }));
    runner.enter_scene("main", &scenes).await.unwrap();

    runner
        .handle_exec_event(ExecEvent::Output {
            key: 2,
            generation: 2,
            kind: ExecOutputKind::Image,
            stdout: transparent_png_bytes(),
        })
        .await;

    let image = mock
        .last_image(1)
        .expect("output image was not staged")
        .to_rgb8();
    assert_eq!(image.get_pixel(0, 0).0, [0xff, 0x00, 0x00]);
}

/// A `text_exec` result is drawn with the button's configured background and text
/// colour.
#[tokio::test]
async fn text_exec_output_uses_configured_colours() {
    let mock = MockButtonDevice::default();
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let (refresh_tx, mut _refresh_rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(
        1,
        &mock,
        FORMAT,
        tx,
        refresh_tx,
        Log::default(),
        &HashSet::new(),
        Defaults::default().background,
        Defaults::default().text_color,
    );

    let scenes = scenes_with_buttons(json!({
        "1b02": { "type": "text_exec", "params": "sleep 10", "background": "#0000ff", "text_color": "#00ff00" }
    }));
    runner.enter_scene("main", &scenes).await.unwrap();

    runner
        .handle_exec_event(ExecEvent::Output {
            key: 2,
            generation: 2,
            kind: ExecOutputKind::Text,
            stdout: b"M\n".to_vec(),
        })
        .await;

    let image = mock.last_image(1).expect("output was not staged").to_rgb8();
    assert_eq!(image.get_pixel(0, 0).0, [0, 0, 255]);
    assert!(
        image
            .pixels()
            .any(|p| p.0[1] > 200 && p.0[0] < 60 && p.0[2] < 60),
        "expected a green glyph pixel"
    );
}

/// A current-generation `image_exec` program not emitting an image draws "Error".
#[tokio::test]
async fn image_exec_non_image_output_draws_error_label() {
    let mock = MockButtonDevice::default();
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let (refresh_tx, mut _refresh_rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(
        1,
        &mock,
        FORMAT,
        tx,
        refresh_tx,
        Log::default(),
        &HashSet::new(),
        Defaults::default().background,
        Defaults::default().text_color,
    );

    let scenes =
        scenes_with_buttons(json!({ "1b02": { "type": "image_exec", "params": "sleep 10" } }));
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
    let (refresh_tx, mut _refresh_rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(
        1,
        &mock,
        FORMAT,
        tx,
        refresh_tx,
        Log::default(),
        &HashSet::new(),
        Defaults::default().background,
        Defaults::default().text_color,
    );

    let scenes =
        scenes_with_buttons(json!({ "1b02": { "type": "text_exec", "params": "sleep 10" } }));
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
    let (refresh_tx, mut _refresh_rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(
        1,
        &mock,
        FORMAT,
        tx,
        refresh_tx,
        Log::default(),
        &HashSet::new(),
        Defaults::default().background,
        Defaults::default().text_color,
    );

    let scenes =
        scenes_with_buttons(json!({ "1b02": { "type": "text_exec", "params": "sleep 10" } }));
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
    let (refresh_tx, mut _refresh_rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(
        1,
        &mock,
        FORMAT,
        tx,
        refresh_tx,
        Log::default(),
        &HashSet::new(),
        Defaults::default().background,
        Defaults::default().text_color,
    );

    let scenes =
        scenes_with_buttons(json!({ "1b02": { "type": "text_exec", "params": "sleep 10" } }));
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
    let (refresh_tx, mut _refresh_rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(
        1,
        &mock,
        FORMAT,
        tx,
        refresh_tx,
        Log::default(),
        &HashSet::new(),
        Defaults::default().background,
        Defaults::default().text_color,
    );

    // Unique per test (not just per process): two tests sharing one hardcoded pid
    // file path race on the same file when the test binary runs them concurrently.
    let n = TMP_COUNTER.fetch_add(1, Ordering::SeqCst);
    let pid_file = format!("/tmp/dak_runner_pid_{}_{n}", std::process::id());
    let _ = std::fs::remove_file(&pid_file);
    let scenes = scenes_with_buttons(json!({
        "1b02": {
            "type": "text_exec",
            "params": format!("/bin/sh -c 'echo \\$\\$ > {pid_file}; exec sleep 60'")
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
        is_process_alive(pid),
        "sanity check: process {pid} should be running"
    );

    let reassigned = scenes_with_buttons(json!({ "1b02": { "type": "clear" } }));
    runner.enter_scene("main", &reassigned).await.unwrap();

    let mut gone = false;
    for _ in 0..100 {
        if !is_process_alive(pid) {
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
    let (refresh_tx, mut _refresh_rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(
        1,
        &mock,
        FORMAT,
        tx,
        refresh_tx,
        Log::default(),
        &HashSet::new(),
        Defaults::default().background,
        Defaults::default().text_color,
    );

    let scenes = scenes_with_buttons(json!({ "1b02": { "type": "text_exec", "params": "true" } }));
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
    let (refresh_tx, mut _refresh_rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(
        1,
        &mock,
        FORMAT,
        tx,
        refresh_tx,
        Log::default(),
        &HashSet::new(),
        Defaults::default().background,
        Defaults::default().text_color,
    );

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

    set_image_from_file(
        &mock,
        3,
        FORMAT,
        image_path.to_str().unwrap(),
        &Defaults::default().background,
    )
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
    let (refresh_tx, mut _refresh_rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(
        1,
        &mock,
        FORMAT,
        tx,
        refresh_tx,
        Log::default(),
        &HashSet::new(),
        Defaults::default().background,
        Defaults::default().text_color,
    );

    let n = TMP_COUNTER.fetch_add(1, Ordering::SeqCst);
    let pid_file = format!("/tmp/dak_runner_launch_{}_{n}.pid", std::process::id());
    let done_file = format!("/tmp/dak_runner_launch_{}_{n}.done", std::process::id());
    let _ = std::fs::remove_file(&pid_file);
    let _ = std::fs::remove_file(&done_file);

    let scenes = scenes_with_buttons(json!({
        "1b01": {
            "type": "launch",
            "params": json!(format!(
                "/bin/sh -c 'echo \\$\\$ > {pid_file}; sleep 2; echo done > {done_file}'"
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

/// A `text` operation with a missing file does not fail the scene: it draws the red
/// "Error" label on that button instead, and the batch's own trailing flush still runs.
#[tokio::test]
async fn text_op_missing_file_draws_error_label() {
    let mock = MockButtonDevice::default();
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let (refresh_tx, mut _refresh_rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(
        1,
        &mock,
        FORMAT,
        tx,
        refresh_tx,
        Log::default(),
        &HashSet::new(),
        Defaults::default().background,
        Defaults::default().text_color,
    );

    let scenes = scenes_with_buttons(json!({
        "1b01": { "type": "text", "params": "/does/not/exist.txt" }
    }));
    runner.enter_scene("main", &scenes).await.unwrap();

    let calls = mock.calls();
    assert_eq!(mock.kinds(&calls), ["SetImage", "Flush", "Flush"]);
    assert_eq!(mock.keys(&calls), [0]);
    let image = mock.last_image(0).expect("error label was not staged");
    assert!(is_red_label(&image), "expected the red \"Error\" label");
}

/// An `image` operation with a missing file does not fail the scene: it draws the red
/// "Error" label on that button instead, and the batch's own trailing flush still runs.
#[tokio::test]
async fn image_op_missing_file_draws_error_label() {
    let mock = MockButtonDevice::default();
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let (refresh_tx, mut _refresh_rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(
        1,
        &mock,
        FORMAT,
        tx,
        refresh_tx,
        Log::default(),
        &HashSet::new(),
        Defaults::default().background,
        Defaults::default().text_color,
    );

    let scenes = scenes_with_buttons(json!({
        "1b01": { "type": "image", "params": "/does/not/exist.png" }
    }));
    runner.enter_scene("main", &scenes).await.unwrap();

    let calls = mock.calls();
    assert_eq!(mock.kinds(&calls), ["SetImage", "Flush", "Flush"]);
    assert_eq!(mock.keys(&calls), [0]);
    let image = mock.last_image(0).expect("error label was not staged");
    assert!(is_red_label(&image), "expected the red \"Error\" label");
}

/// A sibling button with valid content still gets drawn even when another button in
/// the same scene fails: one bad operation must not block the rest of the batch.
#[tokio::test]
async fn missing_file_on_one_button_does_not_block_siblings() {
    let mock = MockButtonDevice::default();
    let image_path = write_temp_image();
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let (refresh_tx, mut _refresh_rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(
        1,
        &mock,
        FORMAT,
        tx,
        refresh_tx,
        Log::default(),
        &HashSet::new(),
        Defaults::default().background,
        Defaults::default().text_color,
    );

    let scenes = scenes_with_buttons(json!({
        "1b01": { "type": "image", "params": image_path.to_str().unwrap() },
        "1b02": { "type": "image", "params": "/does/not/exist.png" },
        "1b03": { "type": "image", "params": image_path.to_str().unwrap() }
    }));
    runner.enter_scene("main", &scenes).await.unwrap();

    let calls = mock.calls();
    // Buttons are applied in ascending key order (1b01, 1b02, 1b03): the two valid
    // images stage and draw normally, the missing one draws the red "Error" label
    // (its own immediate flush from `draw_error_label`), and the batch's own trailing
    // flush runs last.
    assert_eq!(
        mock.kinds(&calls),
        ["SetImage", "SetImage", "Flush", "SetImage", "Flush"]
    );
    assert_eq!(mock.keys(&calls), [0, 1, 2]);
    assert!(
        !is_red_label(&mock.last_image(0).unwrap()),
        "1b01 should show its real image, not the error label"
    );
    assert!(
        is_red_label(&mock.last_image(1).unwrap()),
        "1b02 should show the error label"
    );
    assert!(
        !is_red_label(&mock.last_image(2).unwrap()),
        "1b03 should show its real image, not the error label"
    );
    let _ = std::fs::remove_file(&image_path);
}

/// Render failures besides a missing file are handled the same way: with a zero-size
/// image format, a `text` operation whose file reads fine still cannot be rendered -
/// and neither can the fallback "Error" label, so nothing is staged for that button,
/// but the scene application itself still succeeds and still flushes.
#[tokio::test]
async fn text_op_unrenderable_text_degrades_gracefully() {
    let mock = MockButtonDevice::default();
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let (refresh_tx, mut _refresh_rx) = tokio::sync::mpsc::channel(4);
    let zero_format = ImageFormat {
        size: (0, 0),
        ..FORMAT
    };
    let mut runner = SceneRunner::new(
        1,
        &mock,
        zero_format,
        tx,
        refresh_tx,
        Log::default(),
        &HashSet::new(),
        Defaults::default().background,
        Defaults::default().text_color,
    );

    let path = write_temp_text("hello");
    let scenes = scenes_with_buttons(json!({
        "1b01": { "type": "text", "params": path.to_str().unwrap() }
    }));

    runner.enter_scene("main", &scenes).await.unwrap();
    let _ = std::fs::remove_file(&path);

    // Neither the real text nor the fallback error label could be rendered at a
    // zero-size format, so nothing was ever staged for the button - only the batch's
    // own trailing flush ran.
    assert_eq!(mock.kinds(&mock.calls()), ["Flush"]);
}

/// A `text_exec` whose output cannot be rendered is logged and dropped: the error
/// path is exercised without drawing anything new on the button.
#[tokio::test]
async fn text_exec_unrenderable_output_is_logged_and_skipped() {
    let mock = MockButtonDevice::default();
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let (refresh_tx, mut _refresh_rx) = tokio::sync::mpsc::channel(4);
    let zero_format = ImageFormat {
        size: (0, 0),
        ..FORMAT
    };
    let mut runner = SceneRunner::new(
        1,
        &mock,
        zero_format,
        tx,
        refresh_tx,
        Log::default(),
        &HashSet::new(),
        Defaults::default().background,
        Defaults::default().text_color,
    );

    let scenes = scenes_with_buttons(json!({
        "1b02": { "type": "text_exec", "params": "sleep 10" }
    }));
    runner.enter_scene("main", &scenes).await.unwrap();

    runner
        .handle_exec_event(ExecEvent::Output {
            key: 2,
            generation: 2,
            kind: ExecOutputKind::Text,
            stdout: b"hello".to_vec(),
        })
        .await;

    assert_eq!(
        mock.kinds(&mock.calls()),
        ["Flush"],
        "nothing may be drawn when the output cannot be rendered"
    );
}

/// Operations referencing another device's number are skipped with a notice; nothing
/// is drawn, only the scene-level flush runs.
#[tokio::test]
async fn ops_for_absent_device_are_skipped() {
    let mock = MockButtonDevice::default();
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let (refresh_tx, mut _refresh_rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(
        1,
        &mock,
        FORMAT,
        tx,
        refresh_tx,
        Log::default(),
        &HashSet::new(),
        Defaults::default().background,
        Defaults::default().text_color,
    );

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
    let (refresh_tx, mut _refresh_rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(
        1,
        &mock,
        FORMAT,
        tx,
        refresh_tx,
        Log::default(),
        &HashSet::new(),
        Defaults::default().background,
        Defaults::default().text_color,
    );

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
    let (refresh_tx, mut _refresh_rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(
        1,
        &mock,
        FORMAT,
        tx,
        refresh_tx,
        Log::default(),
        &HashSet::new(),
        Defaults::default().background,
        Defaults::default().text_color,
    );

    let scenes = scenes_with_buttons(json!({
        "1b99": { "type": "clear" }
    }));
    runner.enter_scene("main", &scenes).await.unwrap();

    assert_eq!(mock.kinds(&mock.calls()), ["Flush"]);
}

/// Image operations targeting buttons without a display are skipped with a warning:
/// nothing is drawn, the file is not even opened (a missing path cannot fail the
/// scene), and only the scene-level flush runs.
#[tokio::test]
async fn screenless_buttons_skip_image_ops() {
    let mock = MockButtonDevice::default();
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let (refresh_tx, mut _refresh_rx) = tokio::sync::mpsc::channel(4);
    let screenless = HashSet::from([7, 8]);
    let mut runner = SceneRunner::new(
        1,
        &mock,
        FORMAT,
        tx,
        refresh_tx,
        Log::default(),
        &screenless,
        Defaults::default().background,
        Defaults::default().text_color,
    );

    let scenes = scenes_with_buttons(json!({
        "1b07": { "type": "image", "params": "/no/such/image.png" },
        "1b08": { "type": "text", "params": "/no/such/text.txt" }
    }));
    runner.enter_scene("main", &scenes).await.unwrap();

    assert_eq!(
        mock.kinds(&mock.calls()),
        ["Flush"],
        "no button should have been touched"
    );
}

/// The no-display skip also applies to `text_exec`/`image_exec`: the program is
/// never even spawned on a screenless button, unlike `image`/`text` (covered by
/// [`screenless_buttons_skip_image_ops`]) where only the file read is skipped.
#[tokio::test]
async fn screenless_buttons_skip_exec_ops() {
    let mock = MockButtonDevice::default();
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let (refresh_tx, mut _refresh_rx) = tokio::sync::mpsc::channel(4);
    let screenless = HashSet::from([7, 8]);
    let mut runner = SceneRunner::new(
        1,
        &mock,
        FORMAT,
        tx,
        refresh_tx,
        Log::default(),
        &screenless,
        Defaults::default().background,
        Defaults::default().text_color,
    );

    let scenes = scenes_with_buttons(json!({
        "1b07": { "type": "text_exec", "params": "sleep 10" },
        "1b08": { "type": "image_exec", "params": "sleep 10" }
    }));
    runner.enter_scene("main", &scenes).await.unwrap();

    assert_eq!(
        mock.kinds(&mock.calls()),
        ["Flush"],
        "no button should have been touched"
    );
    assert!(
        rx.try_recv().is_err(),
        "no program should have been spawned, so no exec event should ever arrive"
    );
}

/// The no-display skip only applies to the screenless buttons declared; drawable
/// buttons keep working normally.
#[tokio::test]
async fn drawable_buttons_are_not_skipped() {
    let mock = MockButtonDevice::default();
    let image_path = write_temp_image();
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let (refresh_tx, mut _refresh_rx) = tokio::sync::mpsc::channel(4);
    let screenless = HashSet::from([7, 8]);
    let mut runner = SceneRunner::new(
        1,
        &mock,
        FORMAT,
        tx,
        refresh_tx,
        Log::default(),
        &screenless,
        Defaults::default().background,
        Defaults::default().text_color,
    );

    let scenes = scenes_with_buttons(json!({
        "1b01": { "type": "image", "params": image_path.to_str().unwrap() },
        "1b07": { "type": "image", "params": "/no/such/image.png" }
    }));
    runner.enter_scene("main", &scenes).await.unwrap();

    let calls = mock.calls();
    assert_eq!(mock.kinds(&calls), ["SetImage", "Flush"]);
    assert_eq!(mock.keys(&calls), [0]);
    let _ = std::fs::remove_file(&image_path);
}

/// The no-display skip only checks the operation's type, not the button: a `clear`
/// on a screenless button is not an image assignment, so it is not skipped and
/// still reaches the device (harmless on a button with no screen to clear, but
/// exercising the "screenless but not an image op" branch that no other test hits).
#[tokio::test]
async fn screenless_buttons_are_not_skipped_for_clear() {
    let mock = MockButtonDevice::default();
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let (refresh_tx, mut _refresh_rx) = tokio::sync::mpsc::channel(4);
    let screenless = HashSet::from([7, 8]);
    let mut runner = SceneRunner::new(
        1,
        &mock,
        FORMAT,
        tx,
        refresh_tx,
        Log::default(),
        &screenless,
        Defaults::default().background,
        Defaults::default().text_color,
    );

    let scenes = scenes_with_buttons(json!({ "1b07": { "type": "clear" } }));
    runner.enter_scene("main", &scenes).await.unwrap();

    let calls = mock.calls();
    assert_eq!(mock.kinds(&calls), ["ClearImage", "Flush"]);
    assert_eq!(mock.keys(&calls), [6]);
}

/// A static `image` operation (not an `image_exec`) whose draw the device refuses is
/// logged and does not stop the scene from finishing: the trailing scene-level flush
/// still runs, distinctly from `exec_output_write_failure_is_logged` below, which
/// covers the same draw failure for `image_exec`'s async output instead of a plain
/// `image` operation applied directly by `enter_scene`.
#[tokio::test]
async fn set_image_op_write_failure_is_logged() {
    let device = FailingButtonDevice::default();
    device.fail_image_writes(true);
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let (refresh_tx, mut _refresh_rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(
        1,
        &device,
        FORMAT,
        tx,
        refresh_tx,
        Log::default(),
        &HashSet::new(),
        Defaults::default().background,
        Defaults::default().text_color,
    );

    let image_path = write_temp_image();
    let scenes = scenes_with_buttons(json!({
        "1b03": { "type": "image", "params": image_path.to_str().unwrap() }
    }));
    runner.enter_scene("main", &scenes).await.unwrap();

    let attempts = device.attempts();
    assert_eq!(
        attempts,
        vec!["SetImage", "Flush"],
        "the failing draw is attempted and the scene's trailing flush still runs: \
         {attempts:?}"
    );
    let _ = std::fs::remove_file(&image_path);
}

/// A static `text` operation whose rendered draw the device refuses is logged and
/// does not stop the scene from finishing, mirroring
/// `set_image_op_write_failure_is_logged` for the `text` operation kind instead of
/// `image`.
#[tokio::test]
async fn text_op_write_failure_is_logged() {
    let device = FailingButtonDevice::default();
    device.fail_image_writes(true);
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let (refresh_tx, mut _refresh_rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(
        1,
        &device,
        FORMAT,
        tx,
        refresh_tx,
        Log::default(),
        &HashSet::new(),
        Defaults::default().background,
        Defaults::default().text_color,
    );

    let text_path = write_temp_text("hello");
    let scenes = scenes_with_buttons(json!({
        "1b01": { "type": "text", "params": text_path.to_str().unwrap() }
    }));
    runner.enter_scene("main", &scenes).await.unwrap();

    let attempts = device.attempts();
    assert_eq!(
        attempts,
        vec!["SetImage", "Flush"],
        "the failing draw is attempted and the scene's trailing flush still runs: \
         {attempts:?}"
    );
    let _ = std::fs::remove_file(&text_path);
}

/// A `clear` operation whose device write is refused is logged and does not stop the
/// scene from finishing: the trailing scene-level flush still runs. No other test
/// makes `clear_button_image` itself fail (`screenless_buttons_are_not_skipped_for_clear`
/// above exercises the same operation kind but always against a device that succeeds).
#[tokio::test]
async fn clear_op_write_failure_is_logged() {
    let device = FailingButtonDevice::default();
    device.fail_clear_writes(true);
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let (refresh_tx, mut _refresh_rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(
        1,
        &device,
        FORMAT,
        tx,
        refresh_tx,
        Log::default(),
        &HashSet::new(),
        Defaults::default().background,
        Defaults::default().text_color,
    );

    let scenes = scenes_with_buttons(json!({ "1b01": { "type": "clear" } }));
    runner.enter_scene("main", &scenes).await.unwrap();

    let attempts = device.attempts();
    assert_eq!(
        attempts,
        vec!["ClearImage", "Flush"],
        "the failing clear is attempted and the scene's trailing flush still runs: \
         {attempts:?}"
    );
}

/// failure is logged and no further flush happens for that output.
#[tokio::test]
async fn exec_output_write_failure_is_logged() {
    let device = FailingButtonDevice::default();
    device.fail_image_writes(true);
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let (refresh_tx, mut _refresh_rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(
        1,
        &device,
        FORMAT,
        tx,
        refresh_tx,
        Log::default(),
        &HashSet::new(),
        Defaults::default().background,
        Defaults::default().text_color,
    );

    let scenes =
        scenes_with_buttons(json!({ "1b02": { "type": "image_exec", "params": "sleep 10" } }));
    runner.enter_scene("main", &scenes).await.unwrap();

    runner
        .handle_exec_event(ExecEvent::Output {
            key: 2,
            generation: 2,
            kind: ExecOutputKind::Image,
            stdout: green_png_bytes(),
        })
        .await;

    let attempts = device.attempts();
    assert!(
        attempts.contains(&"SetImage"),
        "the failing draw should have been attempted: {attempts:?}"
    );
    // The failed draw aborts before the trailing flush of the output.
    assert_eq!(
        attempts.iter().filter(|op| **op == "Flush").count(),
        1,
        "only the scene flush should have run: {attempts:?}"
    );
}

/// When staging `image_exec` output succeeds but flushing it to the LCDs fails, the
/// flushed failure is logged and the runner keeps going.
#[tokio::test]
async fn exec_output_flush_failure_is_logged() {
    let device = FailingButtonDevice::default();
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let (refresh_tx, mut _refresh_rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(
        1,
        &device,
        FORMAT,
        tx,
        refresh_tx,
        Log::default(),
        &HashSet::new(),
        Defaults::default().background,
        Defaults::default().text_color,
    );

    let scenes =
        scenes_with_buttons(json!({ "1b02": { "type": "image_exec", "params": "sleep 10" } }));
    // The scene-level flush succeeds first; only the exec-output flush fails.
    runner.enter_scene("main", &scenes).await.unwrap();
    device.fail_flushes(true);

    runner
        .handle_exec_event(ExecEvent::Output {
            key: 2,
            generation: 2,
            kind: ExecOutputKind::Image,
            stdout: green_png_bytes(),
        })
        .await;

    let attempts = device.attempts();
    assert_eq!(
        attempts.iter().filter(|op| **op == "Flush").count(),
        2,
        "scene flush plus failing output flush: {attempts:?}"
    );
    assert!(
        attempts.contains(&"SetImage"),
        "the output should have been staged before the failing flush: {attempts:?}"
    );
}

/// When a failed `image_exec`/`text_exec` program's red "Error" label cannot be drawn
/// because the device rejects writes, that is logged too.
#[tokio::test]
async fn exec_error_label_draw_failure_is_logged() {
    let device = FailingButtonDevice::default();
    device.fail_image_writes(true);
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let (refresh_tx, mut _refresh_rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(
        1,
        &device,
        FORMAT,
        tx,
        refresh_tx,
        Log::default(),
        &HashSet::new(),
        Defaults::default().background,
        Defaults::default().text_color,
    );

    let scenes =
        scenes_with_buttons(json!({ "1b02": { "type": "text_exec", "params": "sleep 10" } }));
    runner.enter_scene("main", &scenes).await.unwrap();

    runner
        .handle_exec_event(ExecEvent::Error {
            key: 2,
            generation: 2,
            error: "boom".to_string(),
        })
        .await;

    assert!(
        device.attempts().contains(&"SetImage"),
        "the error label should have been attempted: {:?}",
        device.attempts()
    );
}

/// A `text_exec` whose output is not UTF-8 draws "Error"; when the device rejects
/// that draw, the failure is logged and the runner keeps going.
#[tokio::test]
async fn exec_output_non_utf8_draw_failure_is_logged() {
    let device = FailingButtonDevice::default();
    device.fail_image_writes(true);
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let (refresh_tx, mut _refresh_rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(
        1,
        &device,
        FORMAT,
        tx,
        refresh_tx,
        Log::default(),
        &HashSet::new(),
        Defaults::default().background,
        Defaults::default().text_color,
    );

    let scenes =
        scenes_with_buttons(json!({ "1b02": { "type": "text_exec", "params": "sleep 10" } }));
    runner.enter_scene("main", &scenes).await.unwrap();

    runner
        .handle_exec_event(ExecEvent::Output {
            key: 2,
            generation: 2,
            kind: ExecOutputKind::Text,
            stdout: b"\xff\xfe".to_vec(),
        })
        .await;

    assert!(
        device.attempts().contains(&"SetImage"),
        "the error label should have been attempted: {:?}",
        device.attempts()
    );
}

/// An `image_exec` whose output is not a valid image draws "Error"; a device that
/// rejects that draw logs the failure.
#[tokio::test]
async fn exec_output_invalid_image_draw_failure_is_logged() {
    let device = FailingButtonDevice::default();
    device.fail_image_writes(true);
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let (refresh_tx, mut _refresh_rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(
        1,
        &device,
        FORMAT,
        tx,
        refresh_tx,
        Log::default(),
        &HashSet::new(),
        Defaults::default().background,
        Defaults::default().text_color,
    );

    let scenes =
        scenes_with_buttons(json!({ "1b02": { "type": "image_exec", "params": "sleep 10" } }));
    runner.enter_scene("main", &scenes).await.unwrap();

    runner
        .handle_exec_event(ExecEvent::Output {
            key: 2,
            generation: 2,
            kind: ExecOutputKind::Image,
            stdout: b"not an image".to_vec(),
        })
        .await;

    assert!(
        device.attempts().contains(&"SetImage"),
        "the error label should have been attempted: {:?}",
        device.attempts()
    );
}

/// Reassigning a button while its `text_exec` runs kills the program and draws "Error";
/// when the device rejects the error draw, that failure is logged but the killing and
/// the reassignment still proceed.
#[tokio::test]
async fn reassign_kill_error_label_draw_failure_is_logged() {
    let device = FailingButtonDevice::default();
    device.fail_image_writes(true);
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let (refresh_tx, mut _refresh_rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(
        1,
        &device,
        FORMAT,
        tx,
        refresh_tx,
        Log::default(),
        &HashSet::new(),
        Defaults::default().background,
        Defaults::default().text_color,
    );

    // Unique per test (not just per process): two tests sharing one hardcoded pid
    // file path race on the same file when the test binary runs them concurrently.
    let n = TMP_COUNTER.fetch_add(1, Ordering::SeqCst);
    let pid_file = format!("/tmp/dak_runner_pid_{}_{n}", std::process::id());
    let _ = std::fs::remove_file(&pid_file);
    let scenes = scenes_with_buttons(json!({
        "1b02": {
            "type": "text_exec",
            "params": format!("/bin/sh -c 'echo \\$\\$ > {pid_file}; exec sleep 60'")
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
        is_process_alive(pid),
        "sanity check: process {pid} should be running"
    );

    let reassigned = scenes_with_buttons(json!({ "1b02": { "type": "clear" } }));
    runner.enter_scene("main", &reassigned).await.unwrap();

    let mut gone = false;
    for _ in 0..100 {
        if !is_process_alive(pid) {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(gone, "process {pid} still alive after reassignment");

    let attempts = device.attempts();
    assert!(
        attempts.contains(&"SetImage"),
        "the kill error label should have been attempted: {attempts:?}"
    );
    // The failing error draw does not prevent the reassigned operation itself.
    assert!(
        attempts.contains(&"ClearImage"),
        "the reassigned clear should still have run: {attempts:?}"
    );
    let _ = std::fs::remove_file(&pid_file);
}

/// A button with no "refresh" (or 0) never gets a tick: `apply_one_operation` only
/// schedules one when `refresh_seconds` is nonzero.
#[tokio::test]
async fn refresh_absent_never_sends_a_tick() {
    let mock = MockButtonDevice::default();
    let image_path = write_temp_image();
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let (refresh_tx, mut refresh_rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(
        1,
        &mock,
        FORMAT,
        tx,
        refresh_tx,
        Log::default(),
        &HashSet::new(),
        Defaults::default().background,
        Defaults::default().text_color,
    );

    let scenes = scenes_with_buttons(json!({
        "1b01": { "type": "image", "params": image_path.to_str().unwrap() }
    }));
    runner.enter_scene("main", &scenes).await.unwrap();

    let arrived = tokio::time::timeout(Duration::from_millis(200), refresh_rx.recv()).await;
    assert!(
        arrived.is_err(),
        "no tick should ever be scheduled: {arrived:?}"
    );

    let _ = std::fs::remove_file(&image_path);
}

/// A button with a nonzero "refresh" sends its own key through the refresh channel
/// once the interval elapses; applying the resulting tick redraws just that button.
#[tokio::test]
async fn refresh_redraws_the_button_on_its_own_after_the_interval() {
    let mock = MockButtonDevice::default();
    let image_path = write_temp_image();
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let (refresh_tx, mut refresh_rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(
        1,
        &mock,
        FORMAT,
        tx,
        refresh_tx,
        Log::default(),
        &HashSet::new(),
        Defaults::default().background,
        Defaults::default().text_color,
    );

    let scenes = scenes_with_buttons(json!({
        "1b01": { "type": "image", "params": image_path.to_str().unwrap(), "refresh": 1 }
    }));
    runner.enter_scene("main", &scenes).await.unwrap();
    assert_eq!(mock.kinds(&mock.calls()), ["SetImage", "Flush"]);

    let key = tokio::time::timeout(Duration::from_millis(1500), refresh_rx.recv())
        .await
        .expect("refresh tick should arrive within the interval")
        .expect("channel should not be closed");
    assert_eq!(key, 1);

    runner.refresh_button(key).await.unwrap();
    let calls = mock.calls();
    assert_eq!(
        mock.kinds(&calls),
        ["SetImage", "Flush", "SetImage", "Flush"]
    );
    assert_eq!(mock.keys(&calls), [0, 0]);

    let _ = std::fs::remove_file(&image_path);
}

/// A button's refresh survives a scene switch that does not redefine it: it is tied to
/// the button's active setup operation (tracked in `active_setup`), not to whichever
/// scene happens to be current, exactly like action inheritance.
#[tokio::test]
async fn refresh_survives_a_scene_switch_that_does_not_redefine_the_button() {
    let mock = MockButtonDevice::default();
    let image_path = write_temp_image();
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let (refresh_tx, mut refresh_rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(
        1,
        &mock,
        FORMAT,
        tx,
        refresh_tx,
        Log::default(),
        &HashSet::new(),
        Defaults::default().background,
        Defaults::default().text_color,
    );

    let scenes = json!({
        "A": {
            "setup": {
                "1b01": {
                    "type": "image",
                    "params": image_path.to_str().unwrap(),
                    "refresh": 1
                }
            }
        },
        "B": { "setup": {} }
    });
    runner.enter_scene("A", &scenes).await.unwrap();
    runner.enter_scene("B", &scenes).await.unwrap();

    let key = tokio::time::timeout(Duration::from_millis(1500), refresh_rx.recv())
        .await
        .expect("refresh tick should still arrive after switching to a scene that never redefines the button")
        .expect("channel should not be closed");
    assert_eq!(key, 1);

    runner.refresh_button(key).await.unwrap();
    let calls = mock.calls();
    // Entering A drew once; entering B touched nothing (empty setup) but still flushed;
    // the tick redrew once more.
    assert_eq!(
        mock.kinds(&calls),
        ["SetImage", "Flush", "Flush", "SetImage", "Flush"]
    );

    let _ = std::fs::remove_file(&image_path);
}

/// Redefining a button cancels its old pending refresh tick, even if the new operation
/// has no refresh of its own: the cancelled tick must never arrive.
#[tokio::test]
async fn refresh_is_cancelled_when_the_button_is_redefined() {
    let mock = MockButtonDevice::default();
    let image_path = write_temp_image();
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let (refresh_tx, mut refresh_rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(
        1,
        &mock,
        FORMAT,
        tx,
        refresh_tx,
        Log::default(),
        &HashSet::new(),
        Defaults::default().background,
        Defaults::default().text_color,
    );

    let scene_a = scenes_with_buttons(json!({
        "1b01": { "type": "image", "params": image_path.to_str().unwrap(), "refresh": 1 }
    }));
    runner.enter_scene("main", &scene_a).await.unwrap();

    let scene_b = scenes_with_buttons(json!({ "1b01": { "type": "clear" } }));
    runner.enter_scene("main", &scene_b).await.unwrap();

    let arrived = tokio::time::timeout(Duration::from_millis(1500), refresh_rx.recv()).await;
    assert!(
        arrived.is_err(),
        "the cancelled tick must never arrive: {arrived:?}"
    );

    let _ = std::fs::remove_file(&image_path);
}

/// A refresh re-resolves a `setup` entry's references against the current variable
/// values, rather than reusing what was captured when the scene was entered.
#[tokio::test]
async fn refresh_re_resolves_referenced_params() {
    let mock = MockButtonDevice::default();
    let first = write_temp_text("first");
    let second = write_temp_text("second");
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let (refresh_tx, _refresh_rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(
        1,
        &mock,
        FORMAT,
        tx,
        refresh_tx,
        Log::default(),
        &HashSet::new(),
        Defaults::default().background,
        Defaults::default().text_color,
    );

    let mut defs = std::collections::BTreeMap::new();
    defs.insert(
        "file".to_string(),
        VarDef::string(255, first.to_str().unwrap().to_string()),
    );
    let variables = std::sync::Arc::new(Mutex::new(Variables::new(defs, &Defaults::default())));
    runner.set_variables(variables.clone());

    let scenes = scenes_with_buttons(json!({
        "1b01": { "type": "text", "params": "$file" }
    }));
    runner.enter_scene("main", &scenes).await.unwrap();
    let first_image = mock.last_image(0).expect("the first text should render");

    variables
        .lock()
        .unwrap()
        .store_mut()
        .set("file", VarValue::Str(second.to_str().unwrap().to_string()));
    runner.refresh_button(1).await.unwrap();
    let second_image = mock.last_image(0).expect("the second text should render");

    assert_ne!(
        first_image.to_rgb8().into_raw(),
        second_image.to_rgb8().into_raw(),
        "the refresh should have re-resolved the params from the new variable value"
    );

    let _ = std::fs::remove_file(&first);
    let _ = std::fs::remove_file(&second);
}

/// A `text_value` entry renders its expanded text directly - no file or program - and a
/// refresh re-resolves it from the current variable values.
#[tokio::test]
async fn text_value_renders_and_refreshes_from_variables() {
    let mock = MockButtonDevice::default();
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let (refresh_tx, _refresh_rx) = tokio::sync::mpsc::channel(4);
    let mut runner = SceneRunner::new(
        1,
        &mock,
        FORMAT,
        tx,
        refresh_tx,
        Log::default(),
        &HashSet::new(),
        Defaults::default().background,
        Defaults::default().text_color,
    );

    let mut defs = std::collections::BTreeMap::new();
    defs.insert("name".to_string(), VarDef::string(20, "one".to_string()));
    let variables = std::sync::Arc::new(Mutex::new(Variables::new(defs, &Defaults::default())));
    runner.set_variables(variables.clone());

    let scenes = scenes_with_buttons(json!({
        "1b01": { "type": "text_value", "params": "n=$name" }
    }));
    runner.enter_scene("main", &scenes).await.unwrap();
    assert_eq!(mock.kinds(&mock.calls()), ["SetImage", "Flush"]);
    let first = mock.last_image(0).expect("the value should render");

    variables
        .lock()
        .unwrap()
        .store_mut()
        .set("name", VarValue::Str("two".to_string()));
    runner.refresh_button(1).await.unwrap();
    let second = mock.last_image(0).expect("the value should re-render");

    assert_ne!(
        first.to_rgb8().into_raw(),
        second.to_rgb8().into_raw(),
        "the refresh should have re-resolved the value text"
    );
}

// -- redraw after reconnect --

/// Builds a runner over `device` with fresh exec/refresh channels, returning the exec
/// receiver so tests can observe restarted `*_exec` programs.
fn redraw_runner<D: ButtonDevice>(
    device: &D,
) -> (SceneRunner<'_, D>, tokio::sync::mpsc::Receiver<ExecEvent>) {
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    let (refresh_tx, _refresh_rx) = tokio::sync::mpsc::channel(8);
    let runner = SceneRunner::new(
        1,
        device,
        FORMAT,
        tx,
        refresh_tx,
        Log::default(),
        &HashSet::new(),
        Defaults::default().background,
        Defaults::default().text_color,
    );
    (runner, rx)
}

/// `redraw_all` repaints every button with an active operation - including one set by
/// an earlier scene and merely inherited by the current one - and flushes exactly once,
/// without touching buttons that never had content.
#[tokio::test]
async fn redraw_all_repaints_every_active_button_including_inherited_ones() {
    let mock = MockButtonDevice::default();
    let image_path = write_temp_image();
    let (mut runner, _exec_rx) = redraw_runner(&mock);
    let scenes = json!({
        "A": { "setup": {
            "1b01": { "type": "image", "params": image_path.to_str().unwrap() },
            "1b02": { "type": "text_value", "params": "hi" }
        }},
        "B": { "setup": {
            "1b04": { "type": "clear" }
        }}
    });
    runner.enter_scene("A", &scenes).await.unwrap();
    runner.enter_scene("B", &scenes).await.unwrap();
    mock.calls.lock().unwrap().clear();

    runner.redraw_all().await.unwrap();

    let calls = mock.calls();
    assert_eq!(
        mock.kinds(&calls),
        ["SetImage", "SetImage", "ClearImage", "Flush"]
    );
    assert_eq!(mock.keys(&calls), [0, 1, 3]);
    let _ = std::fs::remove_file(&image_path);
}

/// `redraw_all` with nothing ever drawn only flushes.
#[tokio::test]
async fn redraw_all_without_content_only_flushes() {
    let mock = MockButtonDevice::default();
    let (mut runner, _exec_rx) = redraw_runner(&mock);
    runner.redraw_all().await.unwrap();
    assert_eq!(mock.kinds(&mock.calls()), ["Flush"]);
}

/// A finished `text_exec` is run again by `redraw_all` (its earlier output never
/// reached the LCD of the new connection), and the old run's result becomes stale.
#[tokio::test]
async fn redraw_all_restarts_finished_exec_programs() {
    let mock = MockButtonDevice::default();
    let (mut runner, mut exec_rx) = redraw_runner(&mock);
    let scenes =
        scenes_with_buttons(json!({ "1b02": { "type": "text_exec", "params": "echo first" } }));
    runner.enter_scene("main", &scenes).await.unwrap();
    let first = tokio::time::timeout(Duration::from_secs(5), exec_rx.recv())
        .await
        .expect("first run should report")
        .unwrap();
    let ExecEvent::Output {
        generation: old, ..
    } = first
    else {
        panic!("unexpected event {first:?}");
    };
    // Let the task finish so it counts as "not running".
    while runner.tracker.is_running(2) {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    runner.redraw_all().await.unwrap();

    assert!(
        !runner.tracker.is_current(2, old),
        "old result must be stale"
    );
    let second = tokio::time::timeout(Duration::from_secs(5), exec_rx.recv())
        .await
        .expect("redraw should restart the program")
        .unwrap();
    let ExecEvent::Output {
        key, generation, ..
    } = second
    else {
        panic!("unexpected event {second:?}");
    };
    assert_eq!(key, 2);
    assert!(runner.tracker.is_current(2, generation));
}

/// A `text_exec` still running at redraw time is left alone: not killed, no red
/// "Error" drawn, and its pending result stays current.
#[tokio::test]
async fn redraw_all_leaves_running_exec_programs_alone() {
    let mock = MockButtonDevice::default();
    let (mut runner, _exec_rx) = redraw_runner(&mock);
    let scenes =
        scenes_with_buttons(json!({ "1b02": { "type": "text_exec", "params": "sleep 10" } }));
    runner.enter_scene("main", &scenes).await.unwrap();
    assert!(runner.tracker.is_running(2));
    mock.calls.lock().unwrap().clear();

    runner.redraw_all().await.unwrap();

    assert!(
        runner.tracker.is_running(2),
        "running program must not be killed"
    );
    assert_eq!(mock.kinds(&mock.calls()), ["Flush"]);
}

/// The runner keeps working across a disconnect through a `SwappableDevice`: while
/// disconnected, scene application fails fast without reaching any device, and after a
/// new connection is swapped in `redraw_all` repaints the pre-disconnect screen on it.
#[tokio::test]
async fn runner_redraws_on_a_swapped_in_connection() {
    use dak::reconnect::SwappableDevice;
    /// No error of the mock is a disconnect.
    fn never(_: &std::convert::Infallible) -> bool {
        false
    }
    let image_path = write_temp_image();
    let device = SwappableDevice::new(MockButtonDevice::default(), never);
    let (mut runner, _exec_rx) = redraw_runner(&device);
    let scenes = scenes_with_buttons(
        json!({ "1b03": { "type": "image", "params": image_path.to_str().unwrap() } }),
    );
    runner.enter_scene("main", &scenes).await.unwrap();

    assert!(runner.is_connected());
    device.mark_disconnected();
    assert!(!device.is_connected());
    assert!(!runner.is_connected());
    let error = runner.redraw_all().await.unwrap_err();
    assert_eq!(error.to_string(), "device disconnected");

    device.replace(MockButtonDevice::default());
    runner.redraw_all().await.unwrap();
    let fresh = device.current().unwrap();
    let calls = fresh.calls();
    assert_eq!(fresh.kinds(&calls), ["SetImage", "Flush"]);
    assert_eq!(fresh.keys(&calls), [2]);
    let _ = std::fs::remove_file(&image_path);
}
