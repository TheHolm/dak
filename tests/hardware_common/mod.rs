//! Shared helpers for hardware-gated integration tests
//! ([`tests/hardware.rs`](../hardware.rs) and
//! [`tests/hardware_read_loop.rs`](../hardware_read_loop.rs)).
//!
//! Each integration test file compiles as its own crate (a separate binary), so
//! anything shared has to live in a module included via `mod hardware_common;`
//! rather than a normal library import - matching the pattern `tests/common/`
//! already uses for the non-hardware integration tests. Each of those crate
//! roots must still declare its own `#[cfg(target_os = "freebsd")] extern crate
//! mirajazz_freebsd as mirajazz;` rename (see `src/lib.rs`); the plain
//! `mirajazz::` paths used in this module resolve through that per-binary
//! extern-prelude entry, so this module itself needs no rename of its own.

#![allow(dead_code)]

use std::sync::LazyLock;

use dak::hardware;
use mirajazz::device::Device;
use tokio::sync::{Mutex, MutexGuard};

/// Process-wide lock over the single physical device every hardware-gated test
/// in this binary shares.
///
/// `cargo test`'s default parallelism (`--test-threads` = available CPUs) runs
/// multiple `#[tokio::test]` functions within one test binary concurrently.
/// `hidraw(4)` only allows one open handle per device node at a time, so two
/// tests racing to [`Device::connect`] at once can make one of them fail
/// outright (a real panic, not just a skip) if it loses the race after
/// [`skip_without_hardware`] already reported the device present, or make it
/// falsely report "no device attached" if it loses the race during that check
/// itself - confirmed by hand: an unguarded run of `tests/hardware.rs`
/// intermittently produced both outcomes on a 2-CPU machine. Every
/// hardware-gated test must call [`lock_hardware`] first, before
/// [`skip_without_hardware`], and hold the returned guard for its entire body
/// (including through `shutdown()`) so at most one test touches the device at
/// a time.
///
/// This only serializes tests within *this* process. Cross-binary safety
/// (e.g. against `tests/hardware_read_loop.rs`, a separate process) instead
/// relies on Cargo running that as a distinct test target - see that file's
/// module doc comment.
static HARDWARE_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// Acquires the process-wide [`HARDWARE_LOCK`]. Call this first, before
/// [`skip_without_hardware`], in every hardware-gated test, and keep the
/// returned guard alive for the test's whole body.
pub async fn lock_hardware() -> MutexGuard<'static, ()> {
    HARDWARE_LOCK.lock().await
}

/// Prints a skip notice and returns `true` when no supported device is attached.
///
/// Every hardware-gated test starts with `if skip_without_hardware().await { return; }`
/// so it passes trivially instead of failing when run without real hardware.
pub async fn skip_without_hardware() -> bool {
    if hardware::is_present().await {
        false
    } else {
        eprintln!("skipping: no Ajazz AKP03E/AKP03R device attached");
        true
    }
}

/// Connects to the first attached device using the same protocol version and
/// default key/encoder counts `map.rs` uses before the real counts are known.
/// Shared by every hardware-gated test that needs a live connection.
pub async fn connect() -> Device {
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
