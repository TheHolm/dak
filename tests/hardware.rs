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
//! Every test uses `#[tokio::test(flavor = "multi_thread")]`, matching the
//! multi-threaded runtime `#[tokio::main]` gives the real binary: `mirajazz`'s
//! image conversion path (used by [`uploads_and_clears_a_button_image`]) calls
//! `spawn_blocking`, which panics ("can call blocking only when running on the
//! multi-threaded runtime") under the single-threaded runtime `#[tokio::test]`
//! defaults to - a real bug this test caught by actually running against
//! hardware with the default flavor before this was added.

use std::time::Duration;

// See src/lib.rs for why this is needed on FreeBSD only: each integration test file
// compiles as its own crate, so it needs its own copy of the rename.
#[cfg(target_os = "freebsd")]
extern crate mirajazz_freebsd as mirajazz;

use dak::hardware;
use mirajazz::device::Device;
use mirajazz::types::DeviceInput;

/// Prints a skip notice and returns `true` when no supported device is attached.
///
/// Every test in this file starts with `if skip_without_hardware().await { return; }`
/// so it passes trivially instead of failing when run without real hardware.
async fn skip_without_hardware() -> bool {
    if hardware::is_present().await {
        false
    } else {
        eprintln!("skipping: no Ajazz AKP03E/AKP03R device attached");
        true
    }
}

/// Connects to the first attached device using the same protocol version and
/// default key/encoder counts `map.rs` uses before the real counts are known.
/// Shared by every test below that needs a live connection.
async fn connect() -> Device {
    let devices = hardware::discover()
        .await
        .expect("enumeration should succeed once is_present() reported a device");
    Device::connect(
        &devices[0],
        hardware::PROTOCOL_VERSION,
        hardware::DEFAULT_KEY_COUNT,
        hardware::DEFAULT_ENCODER_COUNT,
    )
    .await
    .expect("connect should succeed against real hardware")
}

/// Confirms [`hardware::discover`] finds at least one device, and that every
/// discovered device actually matches the Ajazz vendor/product ID DAK targets -
/// i.e. [`hardware::QUERY`] isn't accidentally matching unrelated HID hardware
/// also attached to the test machine.
#[tokio::test(flavor = "multi_thread")]
async fn discovers_only_ajazz_devices() {
    if skip_without_hardware().await {
        return;
    }

    let devices = hardware::discover()
        .await
        .expect("enumeration should succeed");
    assert!(!devices.is_empty());
    for device in &devices {
        assert_eq!(device.vendor_id, 0x0300);
        assert_eq!(device.product_id, 0x3002);
    }
}

/// Connects to the first discovered device, confirms it reports the expected
/// vendor/product ID and a non-empty serial number, then shuts it down cleanly -
/// the same connect/identify/shutdown sequence `run_device` and the `--map`
/// wizard both perform.
#[tokio::test(flavor = "multi_thread")]
async fn connects_and_reports_identity() {
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
    if skip_without_hardware().await {
        return;
    }

    let device = connect().await;
    device
        .clear_all_button_images()
        .await
        .expect("clear_all_button_images should succeed");

    let image = dak::text::render_text(&["HW".to_string()], hardware::IMAGE_FORMAT)
        .expect("render_text should succeed");
    device
        .set_button_image(0, hardware::IMAGE_FORMAT, image)
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

/// Opens the raw input reader and confirms a short, unattended read attempt
/// either times out or returns without a transport-level error - i.e. the read
/// path `run_device`'s main loop and the wizard's capture steps rely on
/// (`get_reader` + `raw_read_data`) is wired correctly, without requiring
/// anyone to actually press a button during an unattended test run.
#[tokio::test(flavor = "multi_thread")]
async fn read_loop_does_not_error_without_input() {
    if skip_without_hardware().await {
        return;
    }

    let device = connect().await;
    let reader = device.get_reader(|_, _| Ok(DeviceInput::NoData));

    match tokio::time::timeout(Duration::from_millis(300), reader.raw_read_data(512)).await {
        Ok(Ok(_)) => {} // a report arrived (e.g. a keepalive); fine, no error
        Ok(Err(error)) => panic!("raw_read_data returned a transport error: {error}"),
        Err(_) => {} // no data within the timeout; expected when nothing is pressed
    }

    device.shutdown().await.expect("shutdown should succeed");
}
