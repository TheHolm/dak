# AGENTS.md

## Repository visibility

**This repository is pushed to a public GitHub repo.** Before committing,
pushing, or writing anything into a tracked file (code, docs, `NOTES.md`,
scripts, test fixtures, commit messages), make sure it contains no sensitive
or identifying information: no real hostnames/IPs, credentials/passwords/API
keys/private keys, serial numbers of specific physical devices, personal
file paths, or other details tied to a particular person's or machine's
identity. Genuinely throwaway material (e.g. ad hoc test scripts/VM
connection details written for a single session) belongs outside the repo
entirely (e.g. under `/tmp`), never committed - see the "Never commit
changes unless the user explicitly asks to commit" rule below, which exists
partly for this reason.

## Project overview

DAK (**D**ynamic **A**jazz **K**eyboard) is a Rust tool for controlling an **Ajazz AKP03E / AKP03R** USB macro keypad (HID device, vendor `0x0300`, product `0x3002`). It connects to the device, paints button images, controls brightness, and reacts to key/encoder input. The package, library and binary are all named `dak`. Version: v0.10.0 (declared as `0.10.0` in `Cargo.toml`, also printed on startup).

## Stack

- Rust 2021 edition, tokio (async runtime)
- `mirajazz` — device communication library
- `async-hid` — underlying HID transport (pulled in via `mirajazz`)
- `image` — image loading/JPEG encoding for button screens
- `ab_glyph` — font rendering for button text labels
- `clap` — command-line argument parsing
- `serde` / `serde_json` — config file parsing
- `chrono` — date/time (for timers)
- Docker (Debian `trixie` base) for building/testing

## Target platforms

Generic Linux and FreeBSD. FreeBSD support is implemented and validated against
real Ajazz AKP03E hardware via a vendored HID backend (see `vendor/README.md` and
`NOTES.md`). No effort is made (to be made) to make the code compile and run
under other platforms.

## Layout

- `src/main.rs` — device discovery, connection setup, image upload, input loop
- `src/actions.rs` — config loading and scene/action handling
- `src/variables.rs` — declared variables and their validation, `$` reference
  expansion/substitution, and the runtime variable/default state
  (`VariableStore`/`Variables`) shared by every device
- `src/baseplane.rs` — device addressing: the `Reference`/`Kind` model (device N,
  button B, encoder E) that scene configs and control references use
- `src/press.rs` — complex press-event detection: turns a button's raw
  press/release timeline into `short_press`/`long_press`/`double_click` events
- `src/text.rs` — text rendering for button LCDs using a font embedded in the binary
- `src/log.rs` — centralized, filterable debug output, gated per `Subsystem`
  (`device`/`scene`/`action`) by `-d`/`--debug`
- `src/map.rs` — interactive device-mapping wizard (`dak --map`)
- `src/hardware.rs` — device family identifiers (`QUERY`/protocol version/default
  key+encoder counts/image format) and `discover`/`is_present` enumeration helpers,
  used by `tests/hardware.rs` and `tests/hardware_read_loop.rs` to detect and drive
  real hardware; `main.rs` and `map.rs` keep their own private copies of the same
  constants for their own connection setup rather than depending on this module
- `src/lib.rs` — library crate exposing config loading/validation and the scene
  runner so both the binary and the integration tests can drive it
- `config.json` — the user's own runtime config (gitignored, not checked in):
  scenes, per-key actions (pressed/released/short/long press/double click), timers
- `config.json.example` — checked-in template new users copy to `config.json`
- `examples/` — complete, copyable example configs, indexed by `examples/EXAMPLES.md`
- `docker/` — Dockerfile and docker-compose for a local build environment
- `README.markdown` — user-facing usage/config docs
- `INSTALL.md` — building from source (both platforms, plus a FreeBSD-specific
  note about a stray cross-compile `.cargo/config.toml`) and one-time device/
  permissions setup (Linux udev rules, FreeBSD hidraw setup)
- `RELEASE_NOTES.md` — history of tagged releases; see the merge/release
  convention below
- `vendor/` — FreeBSD-only forks of `mirajazz`/`async-hid` (the real `async-hid` has no FreeBSD HID backend); only referenced from `Cargo.toml`'s `[target.'cfg(target_os = "freebsd")'.dependencies]`, so Linux and every other platform still resolve the real crates.io releases untouched. See `vendor/README.md`.
- `NOTES.md` — agent-to-agent knowledge base for cross-compiling/packaging/testing `dak` for FreeBSD from Linux (sysroot setup, building a `.pkg`, jail-based dependency testing). Read it before touching CI or cross-compilation; keep it updated as you learn more, don't let it go stale.
- `.woodpecker/release.yaml` — tag-triggered CI pipeline (`event: tag`, `ref: refs/tags/v*`) that builds a Debian trixie `.deb`, an Ubuntu 26.04 LTS `.deb`, and a FreeBSD `.pkg`, then publishes them to a GitHub Release; `.woodpecker/check-target-freshness.yaml` — monthly cron job flagging when the OS versions pinned in `release.yaml` go stale (see `scripts/check-target-freshness.sh`)
- `scripts/` — helpers used only by `.woodpecker/*.yaml`: `build-freebsd-pkg.py` (builds the FreeBSD `.pkg`, see `NOTES.md` section 2), `extract-release-notes.sh` (pulls one tag's user-facing section out of `RELEASE_NOTES.md` for the GitHub release body), `check-target-freshness.sh` (the actual staleness checks referenced above)

## Device notes

- `QUERY` in `main.rs` (vendor 0x0300, product 0x3002) filters the device list
- Images are 60x60 JPEG; the `image` crate computes them on the fly
- The device supports distinct press/release key and encoder states

## Status / known gaps

Work in progress. Current known issues:

- `main.rs` config errors are printed, but the program still exits with `MirajazzError::BadData` regardless of the specific failure
- `main.rs`'s scene/action dispatch loop (`run_device`) and the `--map` wizard's
  interactive I/O still have no test coverage (both need a physical device *and*
  driving actual button presses/encoder turns/typed answers, which
  `tests/hardware.rs`/`tests/hardware_read_loop.rs` deliberately don't attempt).
  Between them, those two files cover, against real hardware when attached
  (skipping themselves otherwise): enumeration, connect/identify/shutdown,
  `set_brightness`, the `set_button_image`/`flush`/`clear_button_image` image
  path, and opening the raw input reader without erroring. The raw-input-reader
  test lives in its own `tests/hardware_read_loop.rs` binary rather than
  alongside the others in `tests/hardware.rs`: on FreeBSD it starts a background
  reader thread that (deliberately, to avoid a worse shutdown-hang bug) never
  releases the device's `hidraw` node for the rest of the process's life once
  nothing more ever reads from it, which would otherwise make every hardware
  test that ran afterwards *in the same process* falsely report "no device
  attached" instead of a real pass. Both files also serialize their own tests
  against each other via `tests/hardware_common`'s `lock_hardware()` (see its
  doc comment): the real device only allows one open handle at a time, and
  `cargo test`'s default parallelism otherwise races multiple tests against it,
  intermittently causing that same false "no device" skip or, worse, genuine
  test failures

## Commands

- Build/check: `cargo build`
- Run: `cargo run` (requires the USB device and udev rules from README)
- Tests: `cargo test` (integration tests in `tests/` split by topic — validation, scene_operations, action_types — extracting shared helpers into `tests/common/`, plus unit tests for private helpers; `tests/hardware.rs` needs a real Ajazz device and skips itself when none is attached, so `cargo test` always succeeds either way)

## Conventions

- `cargo fmt` style, no external formatters
- Every function and every test is documented with a `///` doc comment describing its purpose and any non-obvious behavior
- All new code must be covered by tests — unit tests for private helpers and integration tests in `tests/` for public behaviour; never add production code without accompanying tests
- Commits include a detailed description of what changed and why
- When merging a branch to master that will **not** be tagged as a release,
  summarise all changes in the code since the branch started (or the last
  merge to master) and use that summary as the merge description
- When merging a branch to master that **will** be tagged as a release, the
  merge commit description contains only user-affecting changes (new
  features, bug fixes, changed behavior) — no low-level implementation
  detail. Add a new entry to `RELEASE_NOTES.md` (which holds the full
  history of releases) with that same user-facing summary plus all the
  low-level detail that would otherwise have gone in the merge commit
  description
- When starting work on each new branch, ask the user whether to bump the version number (and if so, to what value) before writing any code
- Version numbers follow `X.Y.Z`: `X` (major) is bumped only when the user
  explicitly asks for it; `Y` (minor) is bumped when a change adds a new
  feature; `Z` (patch) is bumped for bugfixes and other changes that don't
  add, remove, or change functionality
- Never commit changes unless the user explicitly asks to commit
