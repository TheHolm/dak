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
//! Why: [`read_loop_does_not_error_without_input`] opens the device's raw
//! input reader and starts a read. On FreeBSD (see
//! `vendor/async-hid-freebsd/src/backend/hidraw_bsd/mod.rs`'s module doc
//! comment), reading starts a dedicated background OS thread that loops on a
//! blocking `read(2)` of the `/dev/hidrawN` fd and - deliberately, to avoid a
//! worse bug where an abandoned read leaves a thread stuck forever and hangs
//! process shutdown - that thread is never cancelled or joined on drop, only
//! on a transport error (i.e. the device disconnecting). Since this test
//! doesn't press a button, no report ever arrives, no read error ever occurs,
//! and the thread (and the fd it holds via `Arc`) stays alive for the rest of
//! the process's life. FreeBSD's `hidraw(4)` refuses a second concurrent open
//! of the same node (`EBUSY`), so any *other* hardware test that tried to
//! `Device::connect()` afterwards **in the same process** would fail to open
//! that node - and since [`dak::hardware::is_present`] treats a failed connect
//! exactly like "no device attached", every test after this one would silently
//! and misleadingly report `skipping: no Ajazz device attached` and pass
//! trivially, instead of actually exercising the hardware.
//!
//! Confirmed by hand against real hardware: running every test in
//! `tests/hardware.rs` in one process (this test included) left 2 of 6 tests
//! falsely skipped every single time, while running any subset that excludes
//! this test - or running this test alone - always passed for real. Giving
//! this test its own binary/process is the fix: whatever it leaks dies with
//! that process, and every other hardware test keeps running in a clean one.

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
