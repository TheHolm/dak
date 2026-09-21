# Examples

Small, complete configs you can copy next to `dak` (or point at with `dak -c`) and
adjust. Paths, device serials and commands are placeholders - change them to match your
system. Every example is a full config, runnable as-is once you have the hardware and any
helper tools it mentions.

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
  down, both via a `$(/usr/bin/expr ...)` substitution since the language has no inline
  arithmetic) and reset by a button; a `str` "mode" is set by buttons with `:=` and from
  a command's output. Both are shown live on buttons through `text_value` + `refresh`.
  Needs `expr` (coreutils) and `hostname`.

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

See the README's [Variables](../README.markdown#variables) section for the syntax these
examples use.
