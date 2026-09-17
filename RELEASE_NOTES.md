# Release Notes

History of tagged `dak` releases. Each entry has a **User-facing changes**
summary (what also appears in the tagged merge commit's own description)
and a **Details** section with the full low-level technical narrative.
See `AGENTS.md`'s conventions section for how this file is maintained.

## v0.8.2 — Woodpecker CI: tag-triggered release packaging to GitHub

### User-facing changes
- No changes to the `dak` binary itself.
- Tagged releases (`vX.Y.Z`) are now built and published automatically:
  each release gets a source tarball, a Debian trixie `.deb`, an Ubuntu
  26.04 LTS `.deb`, and a FreeBSD `.pkg`, all attached to a GitHub Release
  on the public mirror (github.com/theholm/dak) with that tag's
  `RELEASE_NOTES.md` section as the release body.

### Details
The repo is developed on an on-prem Gitea instance (not internet-facing)
using Woodpecker CI, but releases needed to land on a public GitHub mirror.
Since Woodpecker can't run inside GitHub Actions and the on-prem host isn't
reachable from the internet, the release pipeline instead runs entirely on
the on-prem Woodpecker agent and pushes finished artifacts *out* to GitHub's
REST API (outbound-only, works even though nothing can reach in) - code
itself continues reaching GitHub via Gitea's own push-mirror feature,
independently of CI.

Added:
- `.woodpecker/release.yaml` - the only tag-triggered workflow
  (`event: tag`, `ref: refs/tags/v*`, never runs on branch pushes or PRs).
  Builds all four artifacts above then runs `gh release create`.
- `.woodpecker/build-ci-images.yaml` - a separate, path-filtered
  (`docker/ci/**`) maintenance workflow that (re)builds the prebuilt images
  `release.yaml` depends on, building directly against the Woodpecker
  agent's host Docker socket (bind-mounted, not a nested `docker:dind`) so
  the images land in the same local image store `release.yaml` reads from -
  avoids needing any container registry, at the cost of only working
  correctly with a single Woodpecker agent host.
- `docker/ci/deb-trixie.Dockerfile`, `deb-ubuntu2604.Dockerfile`,
  `freebsd-cross.Dockerfile`, `publish.Dockerfile` - the prebuilt images
  themselves. Baking in `cargo-deb`/`rustup`/`clang`+`lld`/the GitHub CLI/the
  FreeBSD cross-compile sysroot ahead of time means an actual tagged release
  build does none of those downloads itself - only `dak`'s own crates.io
  dependencies remain, and those are cached across releases via a host-path
  cargo-registry volume mounted into the `deb-*`/`freebsd-pkg` steps
  (requires the Woodpecker project be marked "Trusted", since host-path
  volume mounts are a privileged-adjacent feature).
  `freebsd-cross.Dockerfile` extracts just the ~28 link-time sysroot files
  identified in `NOTES.md` section 1.4 from a real FreeBSD 15.x `base.txz`,
  rather than needing a live FreeBSD host to rsync from.
- `scripts/build-freebsd-pkg.py` - promotes the throwaway `.pkg`-building
  recipe from `NOTES.md` section 2.3 into a real, parametrized script (it
  discovers files by walking a staged install root instead of a hardcoded
  file list, so it isn't tied to `dak`'s current file set).
- `scripts/make-src-tarball.sh` - wraps `git archive` for the source tarball
  asset.
- `scripts/extract-release-notes.sh` - pulls just one tag's section out of
  `RELEASE_NOTES.md` (which holds the full history) for use as the GitHub
  Release body.
- `Cargo.toml` gained `description`/`license`/`repository` fields, which
  `cargo-deb` requires to build a package at all (it previously had only
  `name`/`version`/`edition`).

## v0.8.1 — Resilient scene setup: one bad operation no longer blocks the rest

### User-facing changes
- A scene with several buttons no longer stops applying entirely just
  because one `image`/`text` setup entry's file is missing or bad -
  every other button in the scene still draws and reaches the display.
  The failing button now shows the red "Error" label instead, matching
  how `text_exec`/`image_exec` failures already behaved.
- A `text` setup entry reading from an unusually large or infinite
  source (e.g. `/dev/zero`) no longer hangs the program or grows memory
  without bound - the read is capped.
- `text_exec`/`image_exec` programs producing more output than fits in
  one OS pipe buffer now actually complete instead of always being
  killed after the timeout - this was a real, previously silent bug
  affecting any moderately large `image_exec` output.

### Details
Closes three issues found while investigating why a missing image file
blocked an entire scene from applying:
- One failing setup operation no longer blocks its siblings.
  `apply_one_operation` now returns `()` instead of `Result`, catching
  every content-acquisition/processing failure internally
  (missing/unreadable file, undecodable image, unrenderable text)
  instead of propagating it via `?`. Previously the for loop in
  `apply_scene_operations`/`refresh_button` bailed on the first error,
  skipping every operation after it in that scene's setup (processed
  in ascending key order, since `serde_json`'s default `Map` has no
  `preserve_order` feature enabled) and even skipping the batch's own
  trailing flush, so operations that succeeded *before* the failure
  never reached the LCD either.
- Failed image/text draws now consistently show the red "Error" label,
  matching `text_exec`/`image_exec` failures (which already did, via
  `handle_exec_event`). A new `fail_button` helper logs and calls the
  existing `draw_error_label`. The rule is consistent throughout:
  content-acquisition/processing failures (missing file, bad decode,
  bad render) draw "Error"; device-I/O failures (the write/clear call
  itself) are logged only, since attempting to also draw an "Error"
  label would hit the same failing device call and likely just fail
  too - this applies to `Clear` (its only operation is a device write)
  and to the device-write step of `SetImage`/`Text`.
- A `text` setup entry reading from an infinite or huge source (e.g.
  `/dev/zero`) no longer hangs and grows memory without bound. A new
  `read_text_file_bounded` uses `tokio::fs` with a hard
  `MAX_TEXT_FILE_BYTES` (64 KiB) cap instead of unbounded
  `std::fs::read_to_string`; `image::open` also moved into
  `spawn_blocking` (`load_image_file`) for the same "don't block a
  tokio worker thread with sync I/O/decoding" reasoning (`Cargo.toml`
  gains tokio's `"fs"` feature for this).
- A related bug in `text_exec`/`image_exec`'s output capture was fixed
  alongside: `run_command_with_timeout` previously waited for the
  child to exit *before* draining stdout, so any program producing
  more than one OS pipe buffer's worth of output would block on its
  own `write()` (nothing reads it until later) and never exit until
  the timeout killed it - even output that would otherwise finish
  instantly always timed out. Now stdout is drained first, capped at a
  new `MAX_EXEC_OUTPUT_BYTES` (10 MiB) with a distinct "output
  exceeded" error and immediate kill on overflow instead of silently
  deadlocking into the generic timeout.
- A `Send`-related fix was needed along the way: `apply_one_operation`'s
  future is spawned by `run_device` (`main.rs`), which requires it to
  be `Send`. `crate::text::render_text`'s error type is not `Send`, and
  merely calling `drop()` on it before a sibling `.await` in a nested
  `match` was not sufficient to satisfy the compiler's liveness
  analysis. Fixed by converting every fallible result to an owned
  `String` immediately via `.map_err(|e| e.to_string())`, before any
  match/await, sidestepping the question entirely.
- Tests (+12/updated): inverted the two "fails the whole scene" tests
  to assert the error label is drawn and the batch still succeeds;
  added a zero-size-format test asserting graceful degradation
  (neither the real nor fallback render can succeed, nothing is
  staged, no crash); added a three-button test proving a sibling with
  a valid file still draws when another button's file is missing;
  added a `/dev/zero` text test asserting completion well under a
  bounded timeout; added two `run_command_with_timeout` tests (output
  between the pipe buffer size and the cap now succeeds, output past
  the cap is killed immediately with the new distinct error); added a
  test proving two exec entries in one scene do not block each other.
  Two `src/main.rs` tests updated for the new
  draws-error-label-and-continues behavior instead of "stages nothing".
- Verified: `cargo test` passes identically in debug and release (292
  tests), `cargo fmt --check` and `cargo clippy --all-targets` are
  clean.

## v0.8.0 — Multi-action lists and a top-level config version key

### User-facing changes
- Any action value (`short_press`, `long_press`, `double_click`,
  `pressed`, `released`, `turn_cw`, `turn_ccw`, and the scene `timer`)
  can now be either a single string, as before, or an array of them, to
  trigger more than one action from a single event - e.g.
  `"short_press": ["/usr/bin/notify-send hi", "@Main"]`. Every entry
  runs without waiting on the others to finish; an array may contain at
  most one scene-changing entry (`~` or `@scene`), enforced when the
  config loads. `[]` is the array form's way to spell "bound but no
  action" (matching `""` for the plain-string form). A control listed
  in `actions` with no events at all (e.g. `"1b01": {}`) is now warned
  about, though it is not an error. Every existing config keeps working
  unchanged - the plain-string form was not removed.
- The config gains an optional top-level `"version"` string, defaulting
  to `"1.0"` when absent and printed on startup
  (`Loaded config version X from /path/to/config.json`). Not currently
  interpreted; it exists so future schema changes have somewhere to
  record which shape a file was written for.

### Details
Multi-action lists (both string and array forms accepted, per the
smaller-blast-radius design: a `oneOf [string, array-of-string]` union
is exactly as rigid/validatable as a single type, and this way every
existing config, README example, and the large majority of test
fixtures keep working untouched)
- `src/actions.rs`: new `check_action_values` validates either a single
  string (unchanged, via the existing `check_action_value`) or an array
  of non-empty strings - rejecting non-string/empty elements with their
  index, and rejecting more than one scene-changing entry (`~` or
  `@scene`) per array, enforced at config-load time so the runtime
  never has to guard against two competing scene transitions in one
  list. New warning when a per-reference `actions` object has zero
  event keys at all (e.g. `"1b01": {}`) - almost certainly a mistake,
  but not an error.
- `action_for_event`'s return type simplifies from `Option<&str>` to a
  plain `Vec<&str>`: an absent binding and an explicit empty value
  (`[]` or `""`) already behaved identically to every caller (do
  nothing), so collapsing them removes a distinction nothing used
  rather than adding one. `timer_for_scene` mirrors this with
  `Option<(u64, Vec<&str>)>`.
- `main.rs`: new `run_actions` helper loops a resolved list through the
  existing single-action `run_action`, shared by `run_bound_action` and
  the timer branch of `run_device`'s select loop (the only two places
  an action value ever resolves to more than one action). The timer
  channel's item type changed from `String` to `Vec<String>` to carry
  a fired timer's whole action list through.
- Verified actions run without waiting on each other: a new test
  proves a list with a slow command (`sleep 5`) followed by a scene
  switch completes in well under 500ms via `tokio::time::timeout` - the
  slow command's own `Action::Command` handling already spawns and
  never awaits completion, and validation caps a list to at most one
  scene-changing entry, so there is never a competing synchronous
  `enter_scene` call to serialize against.

Top-level config version key
- New optional `"version"` string (default `"1.0"` when absent),
  validated in `validate()` and carried through in `LoadedConfig`. Not
  currently interpreted - just printed on startup so future schema
  changes have somewhere to record which shape a file was written for.
- `config.json.example` and the real local `config.json` both gained
  `"version": "1.0"` and were reordered to version, defaults (where
  present), scenes, devices.

Tests (+30 net): validation (array acceptance, non-string/empty
element rejection with index, multi-scene-change rejection, empty-
object warning, version accept/default/reject), updates to every
existing `tests/action_types.rs` test for the `Vec<&str>` signature
change, and the new non-blocking multi-action dispatch test in
`main.rs`.

Docs: README documents the array form (now "four forms") and the
version key; the `Test` scene's example now demonstrates one
`short_press` both playing a sound and switching scenes in a single
list (replacing what previously needed two separate buttons) - applied
consistently to `README.markdown`, `config.json.example`, and the real
local `config.json`.

Verified: `cargo build --release` is clean, and the full test suite
(301 tests) passes identically in both debug and release profiles.

## v0.7.0 — Per-button setup refresh interval

### User-facing changes
- A `setup` entry (`image`, `text`, `image_exec` or `text_exec`) can now
  set an optional `"refresh"` (seconds) to redraw itself on its own
  schedule, without re-applying the rest of the scene. A clock button no
  longer needs the whole-scene `"timer": {"1": "~"}` trick to update
  every second - see the updated example in README.markdown and
  `config.json.example`. The refresh keeps running across a scene switch
  that doesn't redefine that button, just like its content already does;
  a later scene that does redefine the button replaces both. `refresh`
  is not allowed on `clear` or `launch` (nothing to redraw there) and
  defaults to `0` (never), so every existing config keeps working
  unchanged.

### Details
Version bump
- Cargo.toml, README version badge and AGENTS.md now declare 0.7.0.

Add per-button setup refresh interval (closes the second TODO item:
"Support refreshing individual setup entries without re-applying the
whole scene")
- Schema (`src/actions.rs`, `check_button_op`): `"refresh"` is optional
  on `image`/`text`/`image_exec`/`text_exec` entries; `0` or absent means
  "apply once on scene entry, never again" (today's behavior,
  unchanged). A nonzero `refresh` on `"clear"` or `"launch"` is a config
  error.
- Data model (`SceneOp`): `SetImage`/`Text`/`TextExec`/`ImageExec` each
  gained a `refresh_seconds` field (`SceneOp` now derives `Clone`);
  `scene_operations()` reads the field with the same config-load-time-
  only validation split every other setup field already has.
- Runtime (`SceneRunner`): refresh is tied to a button's currently
  active setup entry, not to whichever scene happens to be current - so
  it survives a scene switch that doesn't redefine the button, exactly
  like action inheritance already works. `apply_scene_operations`'s per-
  operation body was refactored into a shared `apply_one_operation`,
  used by both the existing batch path and a new `refresh_button(key)`,
  so a refresh tick reuses the exact same validity checks, drawing
  logic, and generation-based exec cancellation as a normal scene
  application - just for one button instead of a whole scene, followed
  by its own flush. Two new per-runner maps track this: `active_setup`
  (the operation currently in effect per button, so a tick knows what to
  redraw) and `refresh_handles` (the pending next-tick task per button,
  aborted and replaced every time that button is explicitly
  re-applied). Scheduling mirrors the existing scene-level timer's
  one-shot-respawn style (`arm_scene_timer`/`rearm_scene_timer` in
  `main.rs`) rather than a repeating `tokio::interval`: each tick, once
  handled, reschedules the next one itself as a side effect of re-
  running `apply_one_operation`.
- `main.rs`: a new `refresh_tx`/`refresh_rx` channel is wired into
  `run_device`'s `tokio::select!` loop alongside the existing
  exec/click/timer channels.
- Tests (+22): validation (accepted on the 4 redrawable types, rejected
  on clear/launch, rejected non-numbers, absent defaults to 0);
  `scene_operations` (field threading into the built operations); four
  new `SceneRunner`-level tests using real short sleeps (matching the
  existing 1s-sleep precedent in `main.rs`'s own timer tests): no tick
  when absent, redraw-after-interval, survival across a scene switch
  that doesn't redefine the button, and cancellation on redefinition.
- Docs: README.markdown documents the new field, including the exec-
  restart caveat (a tick restarts `image_exec`/`text_exec` the same way
  reassignment does); both README.markdown's and `config.json.example`'s
  `Main` scene now use `refresh` on the clock button instead of the old
  whole-scene `"timer": {"1": "~"}` trick, demonstrating the feature's
  motivating use case directly.

## v0.6.0 — FreeBSD support via vendored async-hid/mirajazz forks

### User-facing changes
- dak now builds and runs on FreeBSD (previously Linux-only); behavior,
  config format, and Linux builds are unchanged.

### Details
Version bump
- Cargo.toml, README version badge and AGENTS.md now declare 0.6.0.

Add FreeBSD support via vendored async-hid/mirajazz forks
- Makes dak build and run on FreeBSD, where the real async-hid crate
  (pulled in via mirajazz) has no HID backend at all - with zero impact
  on every other platform. dak's Cargo.toml only references the
  vendored path dependencies from a
  `[target.'cfg(target_os = "freebsd")'.dependencies]` table; every
  other target (including Linux) keeps resolving the real, unmodified
  mirajazz/async-hid releases straight from crates.io. Building for
  Linux never resolves, downloads, or compiles anything under vendor/.
- Architecture (see vendor/README.md for full detail): vendor/async-hid-
  freebsd/ is a fork of async-hid 0.5.3 adding a hidraw(4)-based
  backend (src/backend/hidraw_bsd/), with every other platform's
  existing file untouched except one shared-code promotion (the HID
  report descriptor parser moved from Linux-private to crate-shared).
  vendor/mirajazz-freebsd/ is mirajazz 0.16.0, byte-for-byte identical
  to the crates.io release - only its Cargo.toml differs, pointing
  async-hid at the local fork. dak's own source needs only two tiny,
  cfg-gated `extern crate ... as ...;` bridges (src/lib.rs, src/main.rs,
  tests/scene_runner.rs) - Cargo requires a single dependency name to
  resolve to one source kind across all targets, so the FreeBSD path
  dependencies use different manifest keys, and a dependency key (not
  `package =`) is what Rust code imports it as. Every other line of
  dak's own logic (main.rs/actions.rs/map.rs) is unchanged.
- Real hardware investigation and fixes, all confirmed against a real
  Ajazz AKP03E (VID:PID 0300:3002) on a FreeBSD 14.5 VM:
  - uhid(4) vs hidraw(4): a first attempt targeted uhid(4) (attaches by
    default, no extra kernel config) but hit two hardware-confirmed
    bugs - its write(2) doesn't use Linux's leading-report-id-byte
    convention (silently corrupted every command/image write, with no
    error - the device just never drew anything), and its fixed-struct
    USB_GET_REPORT returned ENXIO for feature reports. Switching to
    hidraw(4) (needs hw.usb.usbhid.enable=1 and the hidraw module, both
    off by default) fixed both: write(2) matches Linux exactly, and its
    Linux-compatible HIDIOCGFEATURE ioctl successfully reads the real
    firmware version string.
  - Read-path redesign: hidraw's fd fails tokio::io::unix::AsyncFd
    registration with EINVAL on this FreeBSD (no kqueue support for
    this driver), so writes/feature reports run as blocking syscalls on
    spawn_blocking threads (fine, they always complete on their own).
    Reads needed a different design: dak's tokio::select! input loop
    routinely abandons a pending read whenever any other branch (a
    timer, a click confirmation) resolves first - free on Linux
    (AsyncFd), but confirmed via procstat -t to leak a permanently-
    stuck thread per abandoned read here (60+ within seconds with a
    1-second scene timer) and hang the whole process on Ctrl-C (tokio's
    runtime teardown waits for outstanding blocking tasks). Fixed with
    one dedicated, plain std::thread per opened-for-reading handle (not
    tracked by tokio's blocking pool), started lazily on first actual
    read - eager start also caused a separate EBUSY-on-reopen bug via a
    captured fd clone that never let an unused handle's fd close.
  - Verified via procstat -t (60+ stuck threads before the fix, exactly
    one after) and 5 repeated run/Ctrl-C cycles, all clean; verified
    end-to-end against real hardware: enumeration, connect handshake
    with the firmware feature report, image writes (all 6 screened
    buttons showed correct colors, visually confirmed), and
    button/encoder input reads (correct framing and codes).
- Test suite portability fixes: several tests hardcoded Linux-only
  assumptions harmless there (merged /bin+/usr/bin, /proc always
  mounted) but broken on FreeBSD - /bin/true, /bin/false,
  /usr/bin/sleep switched to bare PATH-resolved names; /proc/<pid>
  existence checks replaced with a portable `kill -0` helper; a
  /proc/<pid>/stat parse replaced with portable `ps -o pgid=`.
- Documentation: README.markdown gained a FreeBSD device-install
  subsection (hidraw/sysctl/devfs.rules one-time setup) and a TODO
  section (CI should check for newer vendored mirajazz/async-hid
  releases); AGENTS.md's Layout section now mentions vendor/;
  vendor/README.md documents the full architecture and investigation
  for future maintainers.

271 tests pass on both Linux and FreeBSD, stable across repeated runs;
clippy and rustfmt clean on both.

## v0.5.1 — dead non-unix fallback removal

### User-facing changes
- None; internal cleanup only (removed dead code paths for platforms
  dak doesn't support).

### Details
Version bump
- Cargo.toml, README version badge and AGENTS.md now declare 0.5.1.

Remove dead non-unix fallbacks from is_executable and spawn_detached
- The project's only supported platform family is unix (Linux and
  FreeBSD, per AGENTS.md); the #[cfg(not(unix))] stubs in
  is_executable (treated any existing file as executable) and
  spawn_detached (logged an error and never ran the command) were only
  ever reachable on an unsupported OS. Made the real unix
  implementations unconditional and dropped the now-unneeded
  #[cfg(unix)] on the CommandExt import.
- Also removed a stray doc comment above spawn_detached that actually
  described a different (command-line parsing) function, left over
  from an earlier edit.
- Investigated but left alone: four unreachable!() calls in actions.rs
  are provably unreachable given exhaustive prior matches/filters, not
  removable dead branches; map.rs's device_path wildcard arm is
  required by the compiler because the upstream async_hid::DeviceId
  enum is #[non_exhaustive], even though it can't currently be reached
  at runtime. No unused pub items, fields or variants were found.
- Verified this doesn't affect FreeBSD support: dak already fails to
  build for x86_64-unknown-freebsd before and after this change, due
  to async-hid 0.5.3 (pulled in via mirajazz) having no FreeBSD HID
  backend at all - a pre-existing dependency gap unrelated to this
  cleanup, flagged for follow-up but not fixed here.

271 tests pass; clippy and rustfmt clean.

## v0.5.0 — encoder turn/push events, testable dispatch layer, coverage fixes

### User-facing changes
- Encoders (knobs) now support `turn_cw`/`turn_ccw` actions, and
  pushing the knob itself acts like a button press, supporting the
  same pressed/released/short/long/double-click events.
- No other user-visible change; the rest of this release is internal
  testability and coverage work.

### Details
Version bump
- Cargo.toml, README version badge and AGENTS.md now declare 0.5.0.

Encoder turn and push events
- Encoders now bind turn_cw / turn_ccw actions per rotation notch, and the
  knob itself acts as a button: pushing it addresses the same pressed /
  released / short_press / long_press / double_click events as a keypad
  button, detected by the shared ClickDetector.
- map.rs: EncoderMapping gains press/release push codes, defaulting to 0
  via serde so configs written before knob capture still load and their
  pushes stay inert; new TwistDirection and ControlEvent enums;
  Mapping::control_event resolves a raw code to a button edge, encoder
  push or encoder turn (buttons win collisions; pushes are only
  recognized once push codes were captured, keeping a raw code-0
  "nothing pressed" report inert); the --map wizard Step 5 now prompts
  the user to push each knob and captures its press/release codes;
  mapping_json emits them.
- main.rs: the input loop routes every report through control_event
  instead of button_number, with per-kind bounds checks; new
  run_pressable_edge helper feeds button and encoder-push edges through
  the shared logic (down-state tracking via HashSet<Reference> replaces
  the button Vec, edge actions and the complex-press detector); pending
  short-press confirmations and the click channel are now keyed by
  Reference, so encoder pushes participate in double-click detection
  exactly like buttons; encoder turns dispatch run_bound_action with
  turn_cw / turn_ccw.
- Tests: unit tests for control_event (button edges, encoder
  turns/pushes, unmapped codes, no-push back-compat); tests/devices.rs
  covers push codes loading and older-format defaulting to 0;
  tests/action_types.rs covers turn_cw / turn_ccw and knob-push actions
  resolving on encoder references.
- Docs: README covers encoder actions and the setup restriction for
  encoders; config.json.example gains encoder push codes and a 1e01
  action block.

Testable dispatch layer, a racy test fix, and coverage gap closure
- run_action, run_bound_action and run_pressable_edge are now generic
  over actions::ButtonDevice instead of hardcoded to mirajazz::Device, so
  the scene-dispatch logic (scene switching, press/release/click
  routing, timer re-arming) can run against a mock device instead of
  requiring physical hardware.
- New test infrastructure in main.rs: a MockButtonDevice recording
  set/clear/flush calls, an EdgeState + press_edge helper mirroring
  run_device's loop state, and scene/binding fixtures for the five press
  events.
- New/expanded unit tests: scene timers (arm/rearm), run_action's stay
  and switch-scene paths including their previously untested failure
  branches, a spawned command's completion now actually runs to exercise
  both its success and failure log branches, run_bound_action
  (bound/unbound refs, encoder turn events), and run_pressable_edge
  (short press, encoder push, duplicate/unmatched edges, long press,
  double-click cancellation).
- map.rs: mapping_json_uses_compact_one_line_entries now covers two
  encoders, exercising the comma-joining branch between multiple encoder
  entries.
- tests/scene_runner.rs: fixed a race where two tests wrote their
  subprocess's pid to the same hardcoded path
  (/tmp/dak_runner_pid_<process id>); since cargo runs a test binary's
  tests concurrently on threads sharing one pid, they intermittently
  read each other's pid and failed their liveness assertions (~1 in 5-10
  runs under cargo llvm-cov) - both now use a per-test-unique path. Also
  added text_op_unrenderable_text_fails_scene and
  text_exec_unrenderable_output_is_logged_and_skipped, covering render
  failures other than a missing file.
- Coverage (via cargo llvm-cov): main.rs region coverage 70.72% ->
  75.15%, map.rs 64.12% -> 64.24%; other modules were already 96-100%.
  Remaining gaps are main()/run_device() and the --map wizard's I/O
  helpers, which need physical hardware and/or an interactive TTY (the
  known, documented gap in AGENTS.md).

271 tests pass, verified stable across repeated runs after the race
fix; clippy and rustfmt clean.

## v0.4.0 — released actions, complex press events, raw-code resolution, --map compact output

### User-facing changes
- Buttons can now bind a `released` action (previously only press was
  actionable).
- New `short_press` / `long_press` / `double_click` bindings, with
  configurable timing via a new `defaults` section - recommended over
  raw `pressed`/`released` for most configs.
- Fixed: buttons without a display could be missed entirely due to
  incorrect raw-code-to-button-number mapping; all buttons now work
  correctly.
- `dak --map` output is now compact (one line per button/encoder),
  ready to paste into a config.

### Details
Released button actions
- action_for_key generalized to action_for_event: the release edge now runs
  the button's "released" binding through a new run_bound_action helper,
  with pressed/released/lookup tests converted and three new tests added.

Complex press events and config defaults
- New src/press.rs detects short_press, long_press and double_click per
  button: a press held past short_press_duration is a long press; a second
  press within double_click_gap of the previous release is a double click
  (firing on that release no matter how long it is held); everything else
  is a short press, confirmed only once the gap passes without a second
  press. Built-in knobs: short_press_duration 300 ms, double_click_gap 300
  ms. Optional top-level "defaults" section tunes both; absent keys fall
  back to the built-ins and unknown/zero/non-numeric values are errors.
- main.rs feeds the detector, fires long/double inline on release, and
  schedules a cancellable short-press confirmation via a click channel
  (double-click cancellation via flag + task abort); -d device logs each
  detection. 16 new unit/validation tests.

Raw-code resolution and config polish
- Buttons without a display report raw codes far above their number
  (37/48/49), so reading data[9] as the button number dropped all their
  events; Mapping::button_number now resolves every report through the
  captured press/release codes and buttons_down indexes by number - 1.
- dak --map prints the compact example-config form (one line per button
  and encoder) so capture output drops straight into a config file.
- short_press_duration default restored to 300 ms; README/config examples
  move ordinary actions to short_press, reorder action keys, and document
  short/long/double press as the recommended bindings over the low-level
  pressed/released edge events; example config takes the real captured
  device codes.

242 tests pass; clippy and rustfmt clean.

## v0.3 — multi-device config, --map wizard, config comments, first release

### User-facing changes
- First tagged release.
- New `dak --map` interactive wizard generates device definitions
  instead of hand-writing them.
- Config can now drive multiple physical devices at once from one
  shared set of scenes (previously a single hard-wired device).
- Config files can contain `//` and `/* */` comments.
- Verbose `-d` device logging now shows full device identity (vendor/
  product ids, usage page/usage, interface).
- Assigning an image to an encoder, or an image to a screenless
  button, is now rejected/skipped instead of silently misbehaving.

### Details
This merged the whole v0.3 development line into a single release
point on top of the v0.2 baseline. Summary of everything that changed:

Device identity and logging
- Verbose -d mode now prints the full identity of every discovered device
  (vendor/product ids, usage page/usage, interface), not just names.
- AGENTS.md documents the project conventions: version-bump workflow,
  per-function/per-test doc comments, and merge-description summaries.

Interactive mapping wizard (dak --map)
- New binary mode that walks the user through picking a USB device and
  capturing each button (and optionally encoder) with its press/release
  state values, then prints a ready-to-paste device definition as JSON.

Multi-device support
- New baseplane module: Reference/Baseplane address model (device N,
  button B, encoder E) that scene configs use; validation now reports
  precise per-entry errors referencing it.
- Config restructured to top-level "scenes" and "devices" sections: the
  scenes dictionary is shared across every physical device, and each
  logical device id (1-9) maps to a definition (keys/encoders counts,
  button press/release values, screens, draw ids) obtained via --map.
- The runtime matches config definitions against discovered HID devices
  and drives every present device concurrently: on_start is applied on
  each, its own buttons/timers run the shared scenes, and each has its
  own spawn executor, event stream and SceneRunner. Absent devices and
  unmatched discovered devices are reported with warnings. This replaces
  the previous single hard-wired device.
- Integration tests migrated/extended around the new model with shared
  helpers in tests/common; devices tests added.

Config comments
- Config loader strips // and /* */ comments (outside strings, positions
  preserved) so config.json can carry notes.

Test coverage
- Coverage audited with cargo llvm-cov and closed: validation for
  non-object setup, backslash parsing in command-line splitting, failed
  spawn_detached start, log info/debug output, and device write-failure
  branches in the scene runner.

Screen/encoder edge cases (drawing safety)
- Assigning an image to an encoder is now a config error (text/image/
  image_exec/text_exec on an "e" reference) and the program refuses to
  start; other encoder entries stay valid and are skipped at runtime.
- Image ops targeting screenless buttons (screen: false) now warn and are
  skipped by the SceneRunner before any file read or USB transfer.

Version
- Bumped to v0.3.0 (was 0.3.0-dev) in Cargo.toml, README and AGENTS.md,
  tagging this merge as the v0.3 release.
