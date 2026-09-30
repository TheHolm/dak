# Examples

Small, complete configs you can copy next to `dak` (or point at with `dak -c`) and
adjust. Paths, device serials and commands are placeholders - change them to match your
system. Every example is a full config, runnable as-is once you have the hardware and any
helper tools it mentions.

- [`hello-dak.json`](hello-dak.json) - a quick tour in one file, buttons only (no
  encoder needed). The home screen has a colourful greeting with an emoji, a live clock
  (`text_exec` + `refresh`, seconds on a second line to fit the 6-column button), the 1/5/15-minute load averages from `/proc/loadavg`
  (one per line, each in its own colour), a `notify-send` button and a "PARTY" button.
  `Party` is a differently-themed scene (its own `background`/`text_color`) with a
  "chase light" animation across `1b04`-`1b06` - three emoji frames picked from the
  current second, each button one frame ahead of the previous - and it returns home
  after 5 seconds via the scene `timer`. Both scenes set, clear or unbind every button
  the other one uses differently, since a scene change keeps whatever it does not
  redefine. Needs `date`, `cut` and Linux's `/proc/loadavg`; `notify-send` (present on
  most Debian/Ubuntu desktops) only warns if missing. Start here if you just want to
  see what a config can do.

- [`scene-carry-over.json`](scene-carry-over.json) - shows how a scene change only
  replaces what the new scene explicitly defines. A home scene fills buttons `1b01`-`1b06`
  and binds actions on `1b01`-`1b05`; the `Alt` scene redefines only `1b01` (a new label
  and the "go back" action), `1b03` (`clear`) and `1b02` (a different action). The other
  buttons keep both their content and their action bindings, inherited from the previous
  scene. Needs `notify-send` (libnotify) to see which binding fired.

- [`media-control.json`](media-control.json) - a media deck. `playerctl` drives the
  active player (play/pause, previous, next), the first encoder changes the system volume
  with `pactl` (`turn_cw` / `turn_ccw`), and `1b05` shows the current track with a
  `text_exec` that refreshes every 3 seconds. `1b04`'s mute is an array action, so it
  toggles the mute and posts a `notify-send` notification in parallel. Needs `playerctl`,
  `pactl` (and a running sound server) and `notify-send`.

- [`scene-navigation.json`](scene-navigation.json) - a home screen that jumps to a live
  clock and a system-info scene. Shows the three complex press events on one button
  (`short_press` / `long_press` / `double_click`, each a different notification), the
  scene `timer` (the clock auto-returns home after 30 seconds), a per-button `refresh`
  (the clock updates every second) and a `clear` button. Needs `notify-send`, and reads
  `date`, `uptime` and `uname`.

- [`push-to-talk.json`](push-to-talk.json) - holding `1b01` unmutes the microphone and
  releasing mutes it again, using the low-level `pressed` / `released` edge events;
  `1b02` toggles mute. `1b03` is a `launch` slot that starts a command fully detached
  when the scene is entered (here a `nerd-dictation` daemon). Needs `pactl` (and a
  running sound server), `notify-send`, and `nerd-dictation` for the launch slot.

- [`launcher-with-icons.json`](launcher-with-icons.json) - an app launcher: each home
  button shows an `image` icon and jumps to a one-button scene that `launch`es the app
  detached and returns home after two seconds. `1b04` demonstrates `image_exec` with
  ImageMagick, whose stdout must be a whole image file. Needs ImageMagick (`convert`, or
  `magick` on v7) and whichever applications you point it at.

- [`counter-and-modes.json`](counter-and-modes.json) - variables. An `int` counter is
  bumped by an encoder (strict `=` on the way up, silently clamping `~=` on the way
  down, both via a `$(expr ...)` substitution since the language has no inline
  arithmetic) and reset by a button; a `str` "mode" is set by buttons with `:=` and from
  a command's output. Both are shown live on buttons through `text_value` + `refresh`.
  Needs `expr` and `hostname` (both in the base system on Linux and FreeBSD; bare
  program names are looked up in `PATH`, so the example works on both).

- [`variable-scene-select.json`](variable-scene-select.json) - `$name` vs `$!name`
  (added in v0.15.0). Menu buttons assign a literal, fully config-authored scene
  reference (`$target := "@Video"`) to a variable; a single "GO" button, bound once
  to `$!target`, pastes that value in as a whole new action - here `@Video` or
  `@Music` - and, never redefined by `Video`/`Music`, carries the same binding into
  both (see `scene-carry-over.json`). By default a `$` value is always data, never an
  action or shell syntax; `$!` is the (documented, narrow) exception, safe only
  because `$target` is never set from a command's output. Needs nothing external.

- [`press-scene-preview.json`](press-scene-preview.json) - a "flash" trick: `pressed` on
  `1b01` switches to a scene that repaints the keypad, and the inherited `released`
  binding switches back when the button is let go. Relies on the scene/action
  inheritance shown in `scene-carry-over.json`.

- [`encoder-screen-brightness.json`](encoder-screen-brightness.json) - turn the third
  encoder to raise/lower the button-screen brightness in steps of 10. Declares a `step`
  variable and uses `bc` (which must be installed) to do the arithmetic:
  `$defaults.button_brightness ~= $(echo $defaults.button_brightness + $step | bc)`.
  `~=` clamps silently at 0 and 100, so winding past either end is a no-op rather than
  an error or a warning. The same config shows a live clock on button 1 and the current
  brightness on button 2, both using a per-button `refresh`; button 2 is a `text_value`
  showing `"$defaults.button_brightness%"` directly (no `echo` needed), re-expanded on
  every tick so it tracks the encoder's changes (the `%` is literal text, ending the
  reference name).

- [`color-themes.json`](color-themes.json) - button/text colours (added in v0.11.0).
  `defaults.background`/`text_color` set the whole keypad's theme, toggled at runtime
  by `1b01` with `$defaults.background := ...` / `$defaults.text_color := ...`
  (readable too, so `1b01`'s own label tracks the current pair live via `refresh`).
  `1b02` is a literal per-button `background`/`text_color` override that ignores the
  theme entirely - an override belongs to the button, not the scene, the same rule as
  content and actions (see `scene-carry-over.json`). `1b03`'s `text_color` is a
  `$variable` instead of a literal, changed on its own with a plain assignment. `1b04`
  shows a `background` override on an `image` entry, composited under any transparent
  icon pixels. Needs nothing external.

- [`styled-text.json`](styled-text.json) - button text markup: a bold left-aligned
  heading over a right-aligned value whose colour comes from a variable (`1b01` switches
  it on press), a single emoji inserted by code point (`#[u=1F600]`), mixed
  bold/italic with a highlighted line, a `text_exec` whose program prints the tags
  itself, the same kind of text shown literally with `"markup": "none"`, and a line of
  wide emoji. A commented-out `defaults.fonts` block shows how to use your own fonts,
  including a colour emoji font and a CJK font (`extra`).
  Needs only `date`.

- [`multi-device.json`](multi-device.json) - one config driving two keypads. A single
  `on_start` scene addresses both through the leading digit of each control reference
  (`1b01` is device 1's first button, `2b01` is device 2's), sharing content and
  actions the same way one device's buttons do. Implemented and covered by tests with
  mocked devices, but never exercised against two real keypads at once - treat it as
  unverified in practice. Also shows the serial-matching rule that makes this safe with
  identical hardware: device "1" matches by VID:PID alone (`serial: "unknown"`, fine
  with only one such device attached), while device "2" carries a placeholder real
  serial to tell a second, identical unit apart - replace it with what `dak --map`
  reports for that unit. Needs nothing external (`notify-send` on `1b02`/`2b02` is
  optional; missing it only warns).

- [`service.json`](service.json) - a config for running dak as a service (systemd user
  unit, XDG autostart or `--detach`): a `logging` section writing to the journal
  plus a log file that SIGHUP reopens for rotation, and reconnect limits after which a
  lost keypad is released until a rescan (`SIGUSR1`). The scene is just a clock. The
  [`service/`](service/) folder has the plug-in hooks that send that rescan
  automatically - a udev rule (`99-dak-rescan.rules`) and a devd rule
  (`dak-rescan.conf`) - and an XDG autostart entry (`dak.desktop`) for starting dak
  with a desktop that has no systemd, such as on FreeBSD. Needs only `date`.

See the README's [Variables](../README.markdown#variables),
[Text markup](../README.markdown#text-markup) and
[Devices](../README.markdown#devices) sections for the syntax these examples use.
