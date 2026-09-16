# Vendored FreeBSD-only dependencies

This directory exists solely to make `dak` build and run on FreeBSD, where the real
`async-hid` crate (pulled in via `mirajazz`) has no HID backend at all - see
`AGENTS.md` for the background and the `v0.5.1-FreeBSD` branch history for how this
was investigated and validated against real Ajazz AKP03E hardware.

**Every other platform, including Linux, is completely unaffected.** `dak`'s
top-level `Cargo.toml` only references these path dependencies from a
`[target.'cfg(target_os = "freebsd")'.dependencies]` table; every other target keeps
using the real, unmodified `mirajazz`/`async-hid` releases straight from crates.io
(see `[target.'cfg(not(target_os = "freebsd"))'.dependencies]` in the same file).
Building for Linux never resolves, downloads, or compiles anything in this directory.

## What's here

- `async-hid-freebsd/`: a fork of `async-hid` 0.5.3 with one addition: a
  `hidraw(4)`-based backend (`src/backend/hidraw_bsd/`) for FreeBSD, registered
  alongside the existing Linux/Windows/macOS backends. Every existing file for those
  other platforms is untouched; the only shared-code change is that the HID report
  descriptor parser (`src/backend/descriptor.rs`) was promoted from a
  Linux-`hidraw`-private module to a crate-shared one, since its byte-parsing logic
  is not actually OS-specific and the new FreeBSD backend needs it too. This
  backend was inspired by [`ocochard/async-hid`](https://github.com/ocochard/async-hid/tree/freebsd)'s
  own FreeBSD branch (also forked from upstream `sidit77/async-hid`), though the
  implementation here differs in several respects - notably no `devd(4)`-based
  enumeration - and was independently investigated/validated against real hardware
  as described below.
- `mirajazz-freebsd/`: an unmodified copy of `mirajazz` 0.16.0. Its `.rs` source is
  byte-for-byte identical to the crates.io release (diff it against the registry copy
  to confirm); only its `Cargo.toml` differs, pointing `async-hid` at
  `../async-hid-freebsd` (a path dependency) instead of crates.io. mirajazz has no
  platform-specific code of its own beyond one `target_os = "windows"` check
  unrelated to FreeBSD, so once `async-hid` builds for FreeBSD, mirajazz works there
  completely unmodified - including its firmware-version feature-report read.

Both crates declare their own empty `[workspace]` table so they are never absorbed
into `dak`'s own workspace.

## Why `hidraw(4)`, not `uhid(4)`

FreeBSD has two generic HID drivers that can claim a USB HID interface no more
specific driver (`ukbd`, `ums`, ...) wants: the classic `uhid(4)`, and the newer
(FreeBSD 13+) `hidraw(4)`, explicitly built to be Linux-`hidraw`-compatible. By
default `uhid(4)` wins the attach race, and it's what a stock FreeBSD box will hand
you at `/dev/uhidN`. An earlier attempt targeted `uhid(4)` for exactly that reason -
no extra kernel configuration - but hit two real, hardware-confirmed problems:

- `uhid(4)`'s `write(2)` does not use Linux's "leading report-id byte" convention for
  devices without numbered reports, unlike `hidraw(4)`. `mirajazz` (like every
  `async-hid` caller) always prepends that byte per the `AsyncHidWrite` contract; on
  `uhid(4)` this silently shifted every command/image write one byte out of
  alignment. The device never errored - it just never drew anything.
- `uhid(4)`'s fixed-struct `USB_GET_REPORT` (feature reports) returned `ENXIO`
  ("Device not configured") for every report id tried, reproduced with a minimal C
  program calling the same ioctl directly - a kernel/device interaction issue, not a
  Rust or async-hid bug.

Switching to `hidraw(4)` fixed both: its `write(2)` matches Linux's hidraw exactly
(no byte-shift translation needed), and its *Linux-compatible* `HIDIOCGFEATURE`
ioctl (a different code path from the uhid-compatible `GET_REPORT` also exposed on
the same device node) successfully reads the real firmware version string. The cost
is that `hidraw(4)` requires two things `uhid(4)` doesn't: the `hidraw` kernel module
loaded, and the `hw.usb.usbhid.enable=1` sysctl/tunable set (both off by default) -
see the top of `README.markdown`'s FreeBSD section for the one-time setup this
requires on a real machine.

One more real, hardware-confirmed wrinkle: on this FreeBSD, registering a
`/dev/hidrawN` fd with `tokio::io::unix::AsyncFd` (kqueue-based readiness polling)
fails with `EINVAL` - plain blocking `read`/`write`/`ioctl` on the same fd work fine.
So `hidraw_bsd`'s writes and feature reports run as genuine blocking syscalls on
`tokio::task::spawn_blocking` threads (bounded operations - they always complete on
their own, so a thread-per-call is fine and matches `spawn_blocking`'s intended use).

Reads needed a different design, for a reason specific to how dak/mirajazz actually
call them: `read_input_report` waits for the *next* report with no timeout, and
`main.rs`'s `tokio::select!` input loop routinely *abandons* a pending read the
moment any other branch (a timer, a click confirmation, ...) resolves first - normal,
documented `select!` behavior. On Linux that's free (dropping an `AsyncFd`-based read
future just stops polling; nothing was running in the background). Confirmed against
real hardware, doing the same `spawn_blocking`-per-call thing for reads is not free
here: a dropped read future leaves its thread parked forever inside the kernel's
`read(2)`, and a *new* one leaks on every abandoned read (which, with a 1-second
scene timer, means dozens of permanently stuck threads within seconds - confirmed via
`procstat -t`, piled up on the driver's internal `hidrawio` wait channel behind the
one truly-blocked `hidrawrd` thread). Separately, tokio's runtime teardown waits for
every outstanding blocking task to finish before the process can exit, so on Ctrl-C
the whole program would just hang forever (also confirmed - the reported symptom
that started this investigation).

So each `HidrawDevice` opened for reading instead lazily starts exactly one
dedicated, plain `std::thread` (not tracked by tokio's blocking pool at all, so
process exit never waits on it) the first time `read_input_report` is actually
called, looping blocking-`read`s and forwarding each report over a channel;
`read_input_report` just awaits the next channel message, and dropping that await is
always safe and cheap. The lazy start matters too, not just the thread-vs-pool
choice: starting it eagerly in `open()` - even for a handle that's opened with read
permission but never actually read from, as `open_feature_handle` does to satisfy a
`HIDIOCGFEATURE` permission check - captures an `Arc` clone of the fd that keeps it
open for the thread's entire (indefinite) lifetime, and a stray handle like that
holding the fd open is exactly what made FreeBSD's `hidraw(4)` refuse a second,
legitimate open of the same node with `EBUSY` (also confirmed against real
hardware, in exactly this scenario).

## Why this shape

`dak`'s own source code (`main.rs`, `map.rs`, `actions.rs`) needs **zero**
FreeBSD-specific branches for its actual logic: the new FreeBSD backend reuses the
exact `DevPath` name Linux's `DeviceId::DevPath` variant already uses (holding a
`/dev/hidrawN` path instead of a Linux sysfs path), so code matching on
`mirajazz`/`async_hid` types compiles and behaves identically regardless of which
physical dependency got resolved for the current target.

The one piece of glue `dak` *does* need: Cargo requires a single dependency name to
resolve to one source kind (registry vs path) across every target, so the FreeBSD
path dependencies are declared under different manifest keys
(`mirajazz-freebsd`/`async-hid-freebsd`, see the top-level `Cargo.toml`). A dependency
key - not `package =` - is what Rust code actually imports it as, so `src/lib.rs` and
`src/main.rs` each carry a small, `#[cfg(target_os = "freebsd")]`-gated
`extern crate mirajazz_freebsd as mirajazz;` (and `async_hid_freebsd as async_hid;`
in `lib.rs`) re-exposing them under the plain names the rest of the crate uses
unconditionally. Every other line of `dak`'s own source is untouched.

## Updating

If `mirajazz` or `async-hid` are ever bumped in the main `[target...not(freebsd)]`
table, re-vendor these directories from the new upstream version and re-apply the
same changes: the `Cargo.toml` path dependency for mirajazz (no `.rs` changes); and
for async-hid, the new backend module (`hidraw_bsd/`) + shared descriptor module +
`DeviceId`/`DynBackend` additions. Re-run the ioctl/struct-layout sanity checks
against a real FreeBSD box (a C program using the same headers, comparing
`sizeof`/`offsetof` and the encoded ioctl numbers against the Rust `#[repr(C)]`
structs and `nix::ioctl_*!` macro invocations) before trusting a new revision - see
the branch history for the exact commands used. Also re-run a real session against
real hardware with `Ctrl-C` (and `procstat -t <pid>` while it's running, watching for
any thread parked on a `hidraw*` wait channel beyond the one expected reader thread)
- the read-path design here exists entirely because of failure modes that only
showed up against real hardware, not in `cargo test`.
