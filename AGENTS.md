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

DAK (**D**ynamic **A**jazz **K**eyboard) is a Rust tool for controlling an **Ajazz AKP03E / AKP03R** USB macro keypad (HID device, vendor `0x0300`, product `0x3002`). It connects to the device, paints button images, controls brightness, and reacts to key/encoder input. The package, library and binary are all named `dak`. Version: v0.14.1 (declared as `0.14.1` in `Cargo.toml`, also printed on startup).

## Stack

- Rust 2021 edition, tokio (async runtime)
- `mirajazz` — device communication library
- `async-hid` — underlying HID transport (pulled in via `mirajazz`)
- `image` — image loading/JPEG encoding for button screens
- `ab_glyph` — font rendering for button text labels
- `unicode-segmentation` / `unicode-width` — grapheme clusters and display widths for
  cutting button text to 6 columns (emoji count as 2)
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
- `src/cli.rs` — the clap `Cli` command-line definition, kept in the library so
  `man/dak.1` can be generated from exactly the flags the binary parses (the
  `clap_mangen` dev-dependency renders it; see `man/` below)
- `src/actions.rs` — config loading and scene/action handling; also the public
  vocabulary constants (`TOP_LEVEL_KEYS`, `DEFAULTS_KEYS`, `SETUP_KINDS`,
  `SETUP_ENTRY_FIELDS`, `CONTROL_EVENTS`, `ENCODER_EVENTS`) that both validation and
  `tests/man_pages.rs` use. Config loading also loads `defaults.fonts` into
  `LoadedConfig::fonts`, which `main.rs` hands every `SceneRunner` via
  `set_text_settings` together with `defaults.markup`
- `src/variables.rs` — declared variables and their validation, `$` reference
  expansion/substitution, and the runtime variable/default state
  (`VariableStore`/`Variables`) shared by every device
- `src/baseplane.rs` — device addressing: the `Reference`/`Kind` model (device N,
  button B, encoder E) that scene configs and control references use
- `src/reconnect.rs` — surviving the keypad disappearing (host suspend, unplug,
  USB reset): `SwappableDevice`, the `ButtonDevice` handle the scene runner draws
  through, whose connection `run_device` swaps for a fresh one once the device is
  rediscovered; `is_disconnect_error` (ENODEV/ENXIO/EIO and async-hid's own
  disconnect variants, plus timeouts); `ReconnectPolicy`, resolved per device from
  `device_reconnect_interval`/`device_reconnect_max_attempts` (device definition
  first, then `defaults`, then 15 s/unlimited); `wait_until`, the rediscovery poll
  that Ctrl-C cancels and that gives up after the attempt limit; and the
  always-shown connect/disconnect/give-up messages. FreeBSD reconnect is known to
  be flaky under VM USB passthrough - see `NOTES.md` section 7
- `src/press.rs` — complex press-event detection: turns a button's raw
  press/release timeline into `short_press`/`long_press`/`double_click` events
- `src/color.rs` — button colours: parsing the `background`/`text_color` config
  values (hex or CSS colour names) and alpha-compositing transparent images onto
  an opaque background
- `src/markup.rs` — button text markup: parses a `text`/`text_value`/`text_exec`
  entry's text into lines of styled spans (`Line`/`Span`/`Style`/`Align`) per its
  `markup` (`none`, or the default `tmux`: `#[bold,fg=red,align=left]` tags, `##`
  escape, and the non-tmux `#[u=1F600]` / `#[u=1F44D,1F3FD]` code-point extension);
  malformed tags stay literal and come back as warnings. `MARKUP_VALUES` is a
  vocabulary constant `tests/man_pages.rs` checks
- `src/text.rs` — text rendering for button LCDs. `FontSet` holds a lookup chain per
  style (configured `defaults.fonts` file first, then the embedded DejaVu Sans Mono
  regular/bold/oblique/bold-oblique, then for bold/italic the configured regular font)
  plus the emoji chain (configured, then embedded monochrome Noto Emoji) and last the
  configured `extra` font (CJK); `FontSet::embedded()` is parsed once and shared,
  `FontSet::load(&FontPaths)` reads configured files (`path#N` = `.ttc` face, 256 MiB
  cap), scanning each with `scan_font` (fails on nothing drawable, reports gaps and
  ignored COLR/SVG colour in a `FontReport`). Glyphs are outlines or colour bitmaps
  (CBDT/sbix PNG/BGRA, drawn as pictures); COLR/SVG are not drawn - see `NOTES.md`
  section 9. `render_lines` draws `markup::Line`s; `render_text`/`render_text_colored`/
  `button_text` keep the plain-text API. `FONT_KEYS` is a vocabulary constant.
  `tests/colour_fonts.rs` uses in-memory fonts from `tests/common/font_builder.rs`
- `src/exit.rs` — the exit statuses (0 ok, 1 failure, 2 usage, 3 config, 4 no
  device, 5 all devices busy); `EXIT_CODES` is checked against `dak(1)`'s EXIT STATUS
  by `tests/man_pages.rs`, and `tests/exit_status.rs` runs the binary for them
- `src/daemon.rs` — background service support: `detach` (pipe, fork, setsid,
  fork, chdir `/`, stdio to `/dev/null`, pid file; `unsafe`, must run before the
  tokio runtime exists) with a `Readiness` pipe over which the daemon sends one
  `StartupReport` so `dak --detach` returns the daemon's real startup status and last
  error (`log::last_error`); `write_pid_file`/`remove_pid_file`; `notify`/`notify_to`,
  a hand-written `sd_notify` client (path or Linux `@abstract` `NOTIFY_SOCKET`). `serve`
  in `main.rs` counts each device task's `StartSignal` (connected / waiting for a lock)
  and reports READY once all have reported or ended. `tests/daemon.rs` runs the binary
- `src/control.rs` — signal handling: one task (`spawn_signal_handler`) owns the
  SIGINT/SIGTERM/SIGHUP/SIGUSR1 listeners for the program's whole life and records
  requests in a `Controller` (quit flag; reload and rescan counters, so bursts
  collapse; a second quit exits at once). `main.rs`'s `Supervisor` owns the device
  tasks (a `JoinSet`), a `LockTable` of held device locks, and a per-generation
  `StopSource`: SIGHUP validates the config, reinstalls logging, stops and restarts
  every task (keeping locks still needed); SIGUSR1 starts tasks for missing devices,
  while a task that gave up reconnecting parks (`park_until_rescan`: releases its lock,
  waits for a rescan, relocks). Foreground exits 4 when idle; `--detach`/systemd
  (`service_mode`) keep waiting. Device tasks get a level-triggered
  `StopSignal` (from a `StopSource`) instead of calling `tokio::signal::ctrl_c()`
  themselves, so a signal arriving mid-event is never lost. `main` is a plain
  function that loads the config before building the tokio runtime by hand
- `src/lock.rs` — one dak per keypad: an `flock(2)` lock file per device
  (`DeviceKey::file_name`, `dak-<vid>-<pid>-<serial>.lock`) in `lock_dir()`
  (`$DAK_LOCK_DIR`, else `/run/lock`, else `/tmp`), holding a `Holder` record (pid,
  uid, user, since). `try_lock`/`acquire` with `Conflict::{Refuse, Wait, Replace}`
  (`--wait`, `--replace`: SIGTERM to own-uid holder or as root, 10 s). Opened
  `O_NOFOLLOW|O_NONBLOCK`, without `O_CREAT` first (protected_regular), created 0666
  - see `NOTES.md` section 10. `main.rs` locks each device before connecting and keeps
  the lock through reconnects; `--map` locks too. `tests/device_lock.rs` re-runs its
  own test binary as a second lock-holding process for the `--replace` tests
- `src/log.rs` — centralized, filterable output. `Log` (still `Copy`) filters by
  `Level` (error/warning/info/debug; errors always pass) and, for debug lines, per
  `Subsystem` (`device`/`scene`/`action`/`fonts`) from `-d`/`logging.debug`; `fonts`
  prints the font lookup order and undrawable-character ranges built by
  `FontSet::load`. Lines go to a process-wide `Sinks` (`install`ed once the config is
  read; plain console before that and in tests): `console`, `journal` (stderr with
  `<N>` priority prefixes), `syslog` (libc `syslog(3)`), `file` (appended, reopenable).
  `check_logging` validates the top-level `logging` section into `LoggingConfig`;
  `LogSettings::resolve` merges it with `CliLogging` (`--log-level`, `--log-file`,
  `--syslog`, `-d`) and resolves `auto` from `Environment` (`JOURNAL_STREAM` matching
  fd 2 -> journal, detached -> syslog, else console). `LOGGING_KEYS`, `LOG_OUTPUTS`,
  `LOG_LEVELS`, `SYSLOG_FACILITIES`, `TIMESTAMP_VALUES` are vocabulary constants
  `tests/man_pages.rs` checks against `dak-config.5`; `tests/logging.rs` runs the binary
- `src/map.rs` — interactive device-mapping wizard (`dak --map`). All prompts go
  through a `Console<R: BufRead, O: Write, E: Write>` (`Console::stdio()` in a real
  run, byte buffers in tests); every question re-asks after an invalid answer but ends
  the wizard with `MapError::InputClosed`/`MapError::Input` at end of input or on a
  read error (only `ask_number_with_default` keeps its default at EOF), so a closed
  stdin can never spin. `run_map_wizard` is only device glue (discovery, lock,
  connect, shutdown); the steps are testable functions: `choose_device`,
  `choose_protocol_version`, `map_connected` (steps 2-6, generic over
  `ButtonDevice<Error = MirajazzError>` and `input::InputSource`) and
  `recheck_display`
- `src/input.rs` — the input side of a connection: the `InputSource` trait (async
  `read_report`, implemented for mirajazz's `DeviceStateReader` and `Arc<T>`),
  `decode_report`/`encode_report` (`ACK` prefix, code at byte 9, state at byte 10)
  shared by `main.rs` and `map.rs`, and `ScriptedInput`, a channel-fed fake the tests
  of both use
- `src/hardware.rs` — device family identifiers (`QUERY`/protocol version/default
  key+encoder counts/image format) and `discover`/`is_present` enumeration helpers,
  used by `tests/hardware.rs` and `tests/hardware_read_loop.rs` to detect and drive
  real hardware; `main.rs` and `map.rs` keep their own private copies of the same
  constants for their own connection setup rather than depending on this module
- `main.rs`'s device loop is split for testing: `run_device` keeps the connection
  side (connect, `'connection` loop, reconnect, cleanup) and hands every event to a
  `Session<D: ButtonDevice>` (`on_report`/`on_timer`/`on_click`/`on_exec`/
  `on_refresh`/`reset_after_disconnect`; `run_connection` is the `select!` loop over
  an `InputSource` and the `SessionChannels`); `route` is the pure raw-code to
  `Dispatch` translation. `await_reconnect` is generic over a `Reopen` trait
  (`ReopenDevice` rediscovers and connects the real keypad; tests script outcomes).
  `Supervisor` samples the reload/rescan counters when it is created (not when `run`
  starts), so a signal arriving during startup is not lost
- `src/lib.rs` — library crate exposing config loading/validation and the scene
  runner so both the binary and the integration tests can drive it
- `fonts/` — the fonts embedded with `include_bytes!` (all unmodified upstream files:
  DejaVu Sans Mono 2.37 in four styles, Noto Emoji 3.000 monochrome variable font)
  and their licences (`LICENSE.txt` = DejaVu's, `OFL.txt` = Noto Emoji's)
- `debian/copyright` — DEP-5 licence file with the full AGPL, Bitstream Vera, Arev
  and OFL-1.1 texts; the only licence file installed by both the `.deb`s (as a
  cargo-deb asset) and the FreeBSD `.pkg`. `tests/packaging.rs` keeps it in step with
  `LICENSE`/`fonts/*`. See `NOTES.md` section 8 for why (Debian/Ubuntu policy)
- `debian/dak.service` — the systemd user unit (`Type=notify`, `--wait`, reload via
  SIGHUP, `RestartPreventExitStatus=3`, `KillMode=process`, bound to
  `graphical-session.target`), a cargo-deb asset;
  `examples/service/` — `dak.desktop` (XDG autostart `dak --detach --wait` for
  non-systemd desktops such as FreeBSD; there is deliberately no rc.d script, see
  `NOTES.md` section 11) and udev/devd rescan-on-plug hooks, all shipped as inactive
  examples in both packages; `examples/service.json` — the service example config. `tests/packaging.rs`
  checks their key settings and that both packaging paths ship them
- `config.json` — the user's own runtime config (gitignored, not checked in):
  scenes, per-key actions (pressed/released/short/long press/double click), timers
- `config.json.example` — checked-in template new users copy to `config.json`
- `examples/` — complete, copyable example configs, indexed by `examples/EXAMPLES.md`
- `man/` — roff man pages: `dak.1` (CLI reference) and `dak-config.5` (config
  file format). `dak.1` is **generated** from `src/cli.rs` plus the roff
  appendix in `tests/man_pages.rs`; regenerate it after touching either with
  `cargo test --test man_pages -- --ignored regenerate_dak_1`. `dak-config.5`
  is hand-written. Installed by the `.deb` (via `[package.metadata.deb]`
  assets in `Cargo.toml`) and `.pkg` (staged in `.woodpecker/release.yaml`)
  packages, both of which now also assert the pages made it into the artifact.
  `tests/man_pages.rs` guards filenames/section/version, compares the committed
  `dak.1` to a fresh render semantically (normalized tokens, so `clap_mangen`
  needs no pinning), and checks `dak-config.5` documents every vocabulary
  constant from `src/actions.rs`/`src/variables.rs`/`src/markup.rs`/`src/text.rs`. Both pages' `.TH` version
  fields must match `Cargo.toml`'s `version`.
- `docker/` — Dockerfile and docker-compose for a local build environment
- `README.markdown` — user-facing usage/config docs
- `INSTALL.md` — building from source (both platforms, plus a FreeBSD-specific
  note about a stray cross-compile `.cargo/config.toml`), one-time device/
  permissions setup (Linux udev rules, FreeBSD hidraw setup), and running as a
  service (systemd user unit, XDG autostart/xinitrc with a logout `pkill`, rescan
  hooks)
- `RELEASE_NOTES.md` — history of tagged releases; see the merge/release
  convention below
- `vendor/` — FreeBSD-only forks of `mirajazz`/`async-hid` (the real `async-hid` has no FreeBSD HID backend); only referenced from `Cargo.toml`'s `[target.'cfg(target_os = "freebsd")'.dependencies]`, so Linux and every other platform still resolve the real crates.io releases untouched. See `vendor/README.md`.
- `NOTES.md` — agent-to-agent knowledge base for cross-compiling/packaging/testing `dak` for FreeBSD from Linux (sysroot setup, building a `.pkg`, jail-based dependency testing). Read it before touching CI or cross-compilation; keep it updated as you learn more, don't let it go stale.
- `.woodpecker/release.yaml` — tag-triggered CI pipeline (`event: tag`, `ref: refs/tags/v*`) that builds a Debian trixie `.deb`, an Ubuntu 26.04 LTS `.deb`, and a FreeBSD `.pkg`, then publishes them to a GitHub Release; `.woodpecker/check-target-freshness.yaml` — monthly cron job flagging when the OS versions pinned in `release.yaml` go stale (see `scripts/check-target-freshness.sh`)
- `scripts/` — helpers used only by `.woodpecker/*.yaml`: `build-freebsd-pkg.py` (builds the FreeBSD `.pkg`, see `NOTES.md` section 2), `extract-release-notes.sh` (pulls one tag's user-facing section out of `RELEASE_NOTES.md` for the GitHub release body), `check-target-freshness.sh` (the actual staleness checks referenced above)

## Device notes

- `QUERY` in `main.rs` (vendor 0x0300, product 0x3002) filters the device list
- Images are 60x60 JPEG; the `image` crate computes them on the fly
- Button text: at most 3 lines x 6 display columns, scaled to fit; embedding the fonts
  and colour-bitmap decoding grew the release binary by about 3.7 MB (5.6 MB to 9.4 MB stripped)
- The device supports distinct press/release key and encoder states

## Status / known gaps

Work in progress. Current known issues:

- What still has no automated coverage is the glue that needs a real keypad: in
  `run_device`, everything between connecting and handing the connection to the
  `Session` (connect, device info logging, the `'connection` loop's reader setup and
  cleanup/shutdown), `ReopenDevice::reopen`, `connect_device`'s success path, the
  `ButtonDevice for Device` wrappers, and `run_map_wizard`'s discovery/lock/connect/
  shutdown glue. Everything they call is tested with fakes (`Session`, `route`,
  `run_connection` over `input::ScriptedInput`, `await_reconnect` over a scripted
  `Reopen`, `map_connected`/`Console`), so the remaining end-to-end check is a
  manual run against hardware: pressing buttons, unplugging and re-plugging.
  `tests/hardware.rs`/`tests/hardware_read_loop.rs` cover, against real hardware
  when attached (skipping themselves otherwise): enumeration,
  connect/identify/shutdown, `set_brightness`, the
  `set_button_image`/`flush`/`clear_button_image` image path, and opening the raw
  input reader without erroring. The raw-input-reader
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
- A keypad that is enumerable but cannot be opened (e.g. a container that sees
  the host's sysfs but has no `/dev/hidraw*` node) makes the hardware tests skip,
  but `dak --map` still lists it; `tests/exit_status.rs`'s `--map` test accepts
  both outcomes (no device: 4; device listed: stdin EOF at the first question: 1)

## Commands

- Build/check: `cargo build`
- Run: `cargo run` (requires the USB device and udev rules from README)
- Coverage: `cargo llvm-cov --summary-only` (add `--show-missing-lines` for line
  numbers); see `NOTES.md` section 12 for the gotchas (stale profiles, forked daemon)
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
- No test may depend on the user's own (gitignored) `config.json` or on a
  keypad being attached: `cargo test` must pass on a fresh checkout on any
  machine. Use temp files (`tests/common`) or the checked-in examples
