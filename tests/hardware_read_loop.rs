//! Integration test for the raw HID input read path, isolated into its own
//! binary (see `AGENTS.md`'s "Layout" entry for `tests/common/` on why shared
//! test helpers live in `tests/hardware_common/`, and note this file's `mod`
//! declaration below for the same pattern).
//!
//! This test **must** run in its own process, separate from every other
//! hardware-gated test in `tests/hardware.rs`. Cargo already gives every
//! integration test file (every `tests/*.rs`, as opposed to files under
//! `tests/*/`) its own binary/process, so simply keeping this test in a
//! separate file is sufficient - do not move it back into `tests/hardware.rs`
//! or otherwise merge it into a shared process with other hardware tests.
//!
//! Why: reading starts, on FreeBSD (see
//! `vendor/async-hid-freebsd/src/backend/hidraw_bsd/mod.rs`'s module doc
//! comment), a dedicated background OS thread holding the `/dev/hidrawN` fd, and
//! `hidraw(4)` refuses a second concurrent open of the same node (`EBUSY`). That
//! thread used to sit in a blocking `read(2)` that only a report or a device error
//! ended, so on an idle keypad it kept the node open for the rest of the process's
//! life, and every later hardware test in the same process falsely reported
//! `skipping: no Ajazz device attached` (a failed connect counts as "no device").
//! The same leak broke dak's SIGHUP reload on FreeBSD (found against real
//! hardware in v0.14.1). The thread now polls with a timeout and is joined when
//! the reader is dropped, and [`reader_releases_the_device_when_dropped`] checks
//! exactly that; the tests stay in their own binary so a regression cannot spoil
//! `tests/hardware.rs` again.

// See src/lib.rs for why this is needed on FreeBSD only: each integration test file
// compiles as its own crate, so it needs its own copy of the rename.
#[cfg(target_os = "freebsd")]
extern crate mirajazz_freebsd as mirajazz;

mod hardware_common;

use std::time::Duration;

use hardware_common::{connect, lock_hardware, skip_without_hardware};
use mirajazz::types::DeviceInput;

/// Opens the raw input reader and confirms a short, unattended read attempt
/// either times out or returns without a transport-level error - i.e. the read
/// path `run_device`'s main loop and the wizard's capture steps rely on
/// (`get_reader` + `raw_read_data`) is wired correctly, without requiring
/// anyone to actually press a button during an unattended test run.
///
/// See this file's module doc comment for why this test must stay alone in
/// its own binary rather than living alongside the other hardware tests.
#[tokio::test(flavor = "multi_thread")]
async fn read_loop_does_not_error_without_input() {
    let _guard = lock_hardware().await;
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

/// Reads from an idle keypad (starting the FreeBSD backend's reader thread), drops
/// the connection, and connects again straight away in the same process: the
/// dropped reader must have released the device node. On FreeBSD a leaked reader
/// thread kept `/dev/hidrawN` open, so this second connect failed and dak's SIGHUP
/// reload lost its keypad; on Linux it always worked.
#[tokio::test(flavor = "multi_thread")]
async fn reader_releases_the_device_when_dropped() {
    let _guard = lock_hardware().await;
    if skip_without_hardware().await {
        return;
    }

    for round in 1..=3 {
        let device = connect().await;
        let reader = device.get_reader(|_, _| Ok(DeviceInput::NoData));
        let _ = tokio::time::timeout(Duration::from_millis(300), reader.raw_read_data(512)).await;
        drop(reader);
        device.shutdown().await.expect("shutdown should succeed");
        drop(device);
        assert!(
            dak::hardware::is_present().await,
            "round {round}: the device could not be opened again after its reader was dropped"
        );
    }
}
