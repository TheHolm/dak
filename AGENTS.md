# AGENTS.md

## Project overview

DAK (**D**ynamic **A**jazz **K**eyboard) is a Rust tool for controlling an **Ajazz AKP03E / AKP03R** USB macro keypad (HID device, vendor `0x0300`, product `0x3002`). It connects to the device, paints button images, controls brightness, and reacts to key/encoder input. The package, library and binary are all named `dak`. Version: v0.5.0 (declared as `0.5.0` in `Cargo.toml`, also printed on startup).

## Stack

- Rust 2021 edition, tokio (async runtime)
- `mirajazz` — device communication library
- `image` — image loading/JPEG encoding for button screens
- `serde_json` — config file parsing
- `chrono` — date/time (for timers)
- Docker (Debian `trixie` base) for building/testing

## Target platforms

Generic Linux and FreeBSD. No effort is made (to be made) to make the code compile and run under other platforms.

## Layout

- `src/main.rs` — device discovery, connection setup, image upload, input loop
- `src/actions.rs` — config loading and scene/action handling
- `config.json` — runtime config: scenes, per-key actions (pressed/released/short/long press/double click), timers
- `docker/` — Dockerfile and docker-compose for a local build environment
- `README.markdown` — udev rules and docker build/run commands

## Device notes

- `QUERY` in `main.rs` (vendor 0x0300, product 0x3002) filters the device list
- Images are 60x60 JPEG; the `image` crate computes them on the fly
- The device supports distinct press/release key and encoder states

## Status / known gaps

Work in progress. Current known issues:

- `main.rs` config errors are printed, but the program still exits with `MirajazzError::BadData` regardless of the specific failure
- `main.rs` device/input code has no test coverage (requires physical hardware)

## Commands

- Build/check: `cargo build`
- Run: `cargo run` (requires the USB device and udev rules from README)
- Tests: `cargo test` (integration tests in `tests/` split by topic — validation, scene_operations, action_types — extracting shared helpers into `tests/common/`, plus unit tests for private helpers)

## Conventions

- `cargo fmt` style, no external formatters
- Every function and every test is documented with a `///` doc comment describing its purpose and any non-obvious behavior
- All new code must be covered by tests — unit tests for private helpers and integration tests in `tests/` for public behaviour; never add production code without accompanying tests
- Commits include a detailed description of what changed and why
- When merging a branch to master, summarise all changes in the code since the branch started (or the last merge to master) and use that summary as the merge description
- When starting work on each new branch, ask the user whether to bump the version number (and if so, to what value) before writing any code
- Never commit changes unless the user explicitly asks to commit
