//! Integration tests that exercise a real Ajazz AKP03E / AKP03R over USB HID,
//! through `mirajazz` directly rather than `dak`'s scene/action layer (which is
//! already covered by mocked tests in `tests/scene_runner.rs` and `src/main.rs`'s
//! own `#[cfg(test)]` module). These close the gap `AGENTS.md` documents as a
//! known limitation: `main.rs`'s device/input code has no test coverage because
//! it requires physical hardware.
//!
//! Every test here needs a real device physically attached and accessible (see
//! `INSTALL.md` for the required udev/hidraw permissions). Each test checks
//! [`dak::hardware::is_present`] first and skips itself - printing a note to
//! stderr and returning without assertions, which `cargo test` counts as a pass -
//! when no device is found, so this file (and `cargo test` as a whole) still
//! succeeds unattended on a machine with no keypad connected.
//!
//! Every test also starts by acquiring
//! [`hardware_common::lock_hardware`] before that presence check, and holds
//! the guard for its whole body: `cargo test`'s default parallelism runs
//! multiple tests in this file concurrently, but the real device only allows
//! one open handle at a time, so unguarded concurrent tests intermittently
//! either false-skip (losing a connect race during the presence check) or
//! fail outright (losing it just after) - confirmed by hand on a 2-CPU
//! machine. See `hardware_common`'s doc comment for detail.
//!
//! Every test uses `#[tokio::test(flavor = "multi_thread")]`, matching the
//! multi-threaded runtime `#[tokio::main]` gives the real binary: `mirajazz`'s
//! image conversion path (used by [`uploads_and_clears_a_button_image`]) calls
//! `spawn_blocking`, which panics ("can call blocking only when running on the
//! multi-threaded runtime") under the single-threaded runtime `#[tokio::test]`
//! defaults to - a real bug this test caught by actually running against
//! hardware with the default flavor before this was added.
//!
//! `read_loop_does_not_error_without_input` deliberately lives in its own
//! `tests/hardware_read_loop.rs` binary rather than here - see that file's doc
//! comment for why sharing a process with it corrupts every hardware test that
//! runs afterwards.

// See src/lib.rs for why this is needed on FreeBSD only: each integration test file
// compiles as its own crate, so it needs its own copy of the rename.
#[cfg(target_os = "freebsd")]
extern crate mirajazz_freebsd as mirajazz;

mod hardware_common;

use dak::actions::ButtonDevice;
use dak::hardware;
use hardware_common::{connect, lock_hardware, skip_without_hardware};

/// Confirms [`hardware::discover`] finds at least one device, and that every
/// discovered device actually matches one of the known family members -
/// i.e. [`hardware::QUERIES`] isn't accidentally matching unrelated HID hardware
/// also attached to the test machine.
#[tokio::test(flavor = "multi_thread")]
async fn discovers_only_ajazz_devices() {
    let _guard = lock_hardware().await;
    if skip_without_hardware().await {
        return;
    }

    let devices = hardware::discover()
        .await
        .expect("enumeration should succeed");
    assert!(!devices.is_empty());
    for device in &devices {
        assert!(
            hardware::Kind::from_vid_pid(device.vendor_id, device.product_id).is_some(),
            "discovered device {:04x}:{:04x} does not match any known Kind",
            device.vendor_id,
            device.product_id
        );
    }
}

/// Connects to the first discovered device, confirms it reports the expected
/// vendor/product ID and a non-empty serial number, then shuts it down cleanly -
/// the same connect/identify/shutdown sequence `run_device` and the `--map`
/// wizard both perform.
#[tokio::test(flavor = "multi_thread")]
async fn connects_and_reports_identity() {
    let _guard = lock_hardware().await;
    if skip_without_hardware().await {
        return;
    }

    let device = connect().await;
    assert_eq!(device.vid, 0x0300);
    assert_eq!(device.pid, 0x3002);
    assert!(!device.serial_number.is_empty());

    device.shutdown().await.expect("shutdown should succeed");
}

/// Sets brightness on a real device - the first write `run_device` and the
/// `--map` wizard both send right after connecting, doubling as the
/// initialization handshake the keypad needs before it responds to anything.
#[tokio::test(flavor = "multi_thread")]
async fn sets_brightness() {
    let _guard = lock_hardware().await;
    if skip_without_hardware().await {
        return;
    }

    let device = connect().await;
    device
        .set_brightness(50)
        .await
        .expect("set_brightness should succeed");

    device.shutdown().await.expect("shutdown should succeed");
}

/// Uploads a real rendered label image, flushes it to the display, then clears
/// it - exercising the same `set_button_image`/`flush`/`clear_button_image`/
/// `clear_all_button_images` path `run_device`'s scene setup and the wizard's
/// display sanity check both rely on.
#[tokio::test(flavor = "multi_thread")]
async fn uploads_and_clears_a_button_image() {
    let _guard = lock_hardware().await;
    if skip_without_hardware().await {
        return;
    }

    let device = connect().await;
    device
        .clear_all_button_images()
        .await
        .expect("clear_all_button_images should succeed");

    let image = dak::text::render_text(
        &["HW".to_string()],
        hardware::Kind::Akp03ERev2.image_format(),
    )
    .expect("render_text should succeed");
    device
        .set_button_image(0, hardware::Kind::Akp03ERev2.image_format(), image)
        .await
        .expect("set_button_image should succeed");
    device.flush().await.expect("flush should succeed");

    device
        .clear_button_image(0)
        .await
        .expect("clear_button_image should succeed");
    device
        .clear_all_button_images()
        .await
        .expect("clear_all_button_images should succeed");

    device.shutdown().await.expect("shutdown should succeed");
}

/// Drives a device through the [`ButtonDevice`] trait surface `SceneRunner` uses
/// in production, generic over the implementor so the call sites below resolve
/// through the trait rather than `Device`'s own identically-named inherent
/// methods. Used to exercise `impl ButtonDevice for mirajazz::device::Device`
/// (`src/actions.rs`), which every other test reaches only through a mock, and
/// which calling these same-named methods directly on a concrete `Device` value
/// would silently bypass in favor of its inherent methods.
async fn drive_through_button_device<D: ButtonDevice>(
    device: &D,
    key_count: usize,
) -> Result<(), D::Error> {
    assert_eq!(device.key_count(), key_count as u8);

    let image = dak::text::render_text(
        &["HW".to_string()],
        hardware::Kind::Akp03ERev2.image_format(),
    )
    .expect("render_text should succeed");
    device
        .set_button_image(0, hardware::Kind::Akp03ERev2.image_format(), image)
        .await?;
    device.flush().await?;
    device.clear_button_image(0).await?;
    device.flush().await?;
    Ok(())
}

/// Exercises `impl ButtonDevice for mirajazz::device::Device` end-to-end against
/// real hardware: every other test either mocks the trait out entirely
/// (`tests/scene_runner.rs`) or drives `Device` through its own inherent methods
/// directly (the other tests in this file), neither of which reaches this impl.
#[tokio::test(flavor = "multi_thread")]
async fn button_device_trait_drives_a_real_device() {
    let _guard = lock_hardware().await;
    if skip_without_hardware().await {
        return;
    }

    let device = connect().await;
    drive_through_button_device(&device, hardware::DEFAULT_KEY_COUNT)
        .await
        .expect("ButtonDevice methods should succeed against real hardware");

    device.shutdown().await.expect("shutdown should succeed");
}
