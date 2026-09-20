# DAK — Dynamic Ajazz Keyboard

**DAK** (**D**ynamic **A**jazz **K**eyboard) is a Rust tool for controlling Ajazz- and Mirabox-branded "stream controller" USB macro keypads (the two brands sell the same OEM hardware under different names/USB IDs) via [`mirajazz`](https://crates.io/crates/mirajazz). Running the binary connects to the device, paints the configured images and text labels onto the button LCDs, controls brightness, and reacts to key and encoder input. Built and actually tested against a real **Ajazz AKP03E** (HID device, vendor `0x0300`, product `0x3002`). Several other Ajazz/Mirabox-branded devices are wired up the same way and have a good chance of working out of the box even though nobody has confirmed them yet - see [Help me support more devices](#help-me-support-more-devices) for the full list and how verified each one is. Project is loosely based on [OpenDesk pkugin](https://github.com/4ndv/opendeck-akp03/)

Work in progress. Config structure will probably change in the future, but I will try to make it easy to update to new version.


# THIS IS VIBE CODED(mostly) GARBAGE(100%, as non vibe-coded parts are garbage too), USE ON YOUR OWN RISK

I did not check what is in the code at all, so who knows what it is really doing.

The current version is **v0.10.0**.

## Usage

```
dak [OPTIONS]

Options:
  -c, --config <CONFIG>   Path to the config file; when omitted, `config.json` is
                          searched for in ~/.config/dak/, then the current
                          directory, then the directory containing the binary
  -d, --debug <DEBUG>...  Debug subsystems to enable, comma-separated: device, scene, action
      --map               Run the interactive device-mapping wizard instead of
                          normal operation (see Devices below) and exit
  -h, --help              Print usage help and exit
```

Examples:

```
dak                         # use the default ./config.json
dak -c /path/to/config.json # load configuration from a custom path
dak -d device,scene         # show device and scene debug output
```

### Output

By default the program prints only the program banner, the config location, and then
warnings and errors about parsing the config and executing commands. Debug output is
controlled with `-d` / `--debug` (repeatable, or comma-separated values added up):

| Subsystem | Prints |
|-----------|--------|
| `device`  | the device used, its capabilities (key/encoder counts, supported states), every key press/release event, and every event that sets or clears a button image |
| `scene`   | entering and leaving scenes, armed scene timers, and every setup operation applied to a button |
| `action`  | every action that runs and what triggered it (key press or scene timer), including launched commands and their completion |

`-d` values are additive, e.g. `-d device -d scene` or `-d device,scene`.

### Config file search order

When `-c`/`--config` is not given, the program loads a file named `config.json` from
the **first** existing location in this order:

1. `~/.config/dak/config.json`
2. `<current-directory>/config.json`
3. `<the-directory-of-the-binary>/config.json`

An explicit `-c`/`--config` path is always used as-is and disables the search. If none
of the searched locations contains a `config.json`, the program reports the missing
file and exits. Run `dak --help` for the exact, generated usage text.

All runtime behavior is driven by `config.json`:

- **Scenes** — buttons can display images, static text files, or the output of async commands (`image_exec` / `text_exec` with a 5-second timeout); scenes switch on key/encoder input, from the scene timer, or on demand.
- **Per-key actions** — `short_press`, `long_press`, `double_click`, `pressed` and `released` bindings, with actions inherited from the previously active scene.
- **Encoders** — each encoder binds `turn_cw` / `turn_ccw` per rotation notch, and the knob itself is pressable like a button: pushing it fires the same `pressed` / `released` / `short_press` / `long_press` / `double_click` events.
- **Timers** — each scene can run a `timer` action after a number of seconds; button presses and scene switches re-arm it only when appropriate.

## Platform support

The target platforms are generic Linux and FreeBSD. No effort is made (or planned) to make the code compile and run under any other platform.

DAK is, and is planned to remain, a **CLI-only** tool: a config file plus a command-line binary, no GUI (no windowed config editor, no tray icon, no bundled companion app). There is no plan to add one.

Prebuilt packages for tagged releases are published to [GitHub Releases](https://github.com/theholm/dak/releases): a Debian trixie `.deb`, an Ubuntu 26.04 LTS `.deb`, and a FreeBSD 15.1-RELEASE `.pkg` (all amd64/x86_64), plus GitHub's automatically generated source archive.

## Config structure

`config.json` drives all runtime behavior. The top level of the config is a dictionary with up to five keys:

- `"scenes"` — the scenes dictionary (see [Scenes](#scenes))
- `"devices"` — the individual device definitions (see [Devices](#devices))
- `"variables"` — optional declared variables that actions read and assign (see [Variables](#variables))
- `"defaults"` — optional press-detection timing knobs and connect-time brightness levels (see [Defaults](#defaults))
- `"version"` — optional config schema version string, defaulting to `"1.0"` when absent. Not currently interpreted (there is only one schema so far) - printed on startup (`Loaded config version X from ...`) so future schema changes have somewhere to record which shape a file was written for.

### Comments

JSON itself has no comment syntax, so `dak` strips comments before parsing: both `//` line comments and `/* ... */` block comments may appear anywhere the JSON grammar allows whitespace, including at the very top or bottom of the file. Comment markers inside a string are part of the string value, not comments. Example:

```json
{
  // which scene starts the program
  "scenes": { "on_start": { "actions": {} } },
  /* one keypad, wired by serial */
  "devices": {
    "1": {
      "device_id": "0300:3002",
      "device_name": "Ajazz HOTSPOTEKUSB HID DEMO",
      "serial": "ABC123", // fall back to "unknown" if your pad has no serial
      "key_count": 9,
      "encoder_count": 3,
      "screens": 6,
      "buttons": [],
      "encoders": []
    }
  }
}
```

### Defaults

The optional top-level `defaults` section tunes how the complex button presses are detected, plus the brightness levels applied when a device connects. All keys are optional and fall back to their built-in values when missing:

```json
"defaults": {
  "short_press_duration": 300,
  "double_click_gap": 300,
  "button_brightness": 50,
  "encoder_brightness": 50
}
```

- `short_press_duration` (default `300`, milliseconds) — a press released at most this long after it started is a `short_press`; a press held any longer is a `long_press`.
- `double_click_gap` (default `300`, milliseconds) — two presses form a `double_click` when the second one lands within this long of the previous release.
- `button_brightness` (default `50`, percent `0`-`100`) — brightness applied to every device's button/screen LCDs when it connects.
- `encoder_brightness` (default `50`, percent `0`-`100`) — brightness applied to every device's encoder LED ring when it connects. Not verified against real hardware: no Ajazz AKP03E/AKP03R unit with functioning encoder LEDs was available during development, and a brute-force sweep across the underlying LED-color command's index space produced no visible response on the unit that was available - see the [TODO](#todo) section.

Complex events are decided on the release edge, per pressable control: buttons and
pushed encoders share the same detection, so a pushed encoder's knob behaves exactly
like an extra button:

- a press held past `short_press_duration` fires `long_press` on its release;
- a second press landing inside `double_click_gap` of the previous release is a `double_click` firing on that second release, no matter how long the second press is held, and its first click never fires `short_press`;
- anything else is a `short_press`, which only fires once `double_click_gap` has passed without a second press — so the first click of a double click is never reported as a short press too.

### Variables

The optional top-level `variables` section declares named values that actions and scene
`params` read and assign. Variables are global: one set shared by every device and every
scene, initialized once at startup. Every variable must be declared explicitly;
referencing an undeclared one is a config-load error.

```json
"variables": {
  "count": { "type": "int", "min": 0, "max": 100, "value": 10 },
  "label": { "type": "str", "max_length": 8, "value": "hello" },
  "plain": { "type": "int" }
}
```

- the key is the variable name: it must start with a letter and contain only letters,
  digits and underscores. `var`, `defaults`, `version`, `scenes` and `devices` are
  reserved and cannot be declared.
- `"type"` (required) is `"int"` (a 32-bit signed integer) or `"str"` (a
  variable-length unicode string).
- `"min"`/`"max"` (int only, optional, inclusive) default to the full 32-bit range and
  bound an assigned value.
- `"max_length"` (str only, optional) defaults to `255` and may be at most `65535`; an
  assigned string is truncated to it.
- `"value"` (optional) is the value the variable starts with, `0`/`""` when omitted. A
  value outside its declared range/type is a hard config error.
- unknown keys and type-inappropriate parameters (`"min"` on a `str`, ...) are hard
  config errors.

#### Reading a variable

A `$` starts a reference. `$name` reads a declared variable; `$var.name` is the same
thing written with its explicit scope (the `var` scope is the default, so both work).
`$defaults.<name>` reads a `defaults` parameter: `button_brightness`,
`encoder_brightness`, `short_press_duration` (milliseconds) and `double_click_gap`
(milliseconds) - the values [Defaults](#defaults) describes, reflecting any runtime
changes.

References are substituted in every action string, in `setup` `params` and in the
`timer` seconds key, resolved when the value is actually used, so they always see the
current value. An int renders as its decimal digits; a str is inserted verbatim (no
quotes added).

A name is greedy (`$name_kun` reads `name_kun`). Escape the next character with a
backslash to end it: `$name\-kun` is the value of `name` followed by `-kun`, and
`$name\ kun` keeps the space. `\$` is a literal dollar sign and `\\` a literal backslash;
any other backslash is left alone for the command tokenizer. A `$` not followed by a
variable name is a config error.

Because `$` now always starts a reference, a literal `$` in a command must be written
`\$`. This is a breaking change for commands that used a shell `$VAR`: write `\$VAR` so
`dak` passes it through, or run the command through a shell explicitly.

#### Assigning to a variable

An action value of the form `$<target> <op> <rhs>` assigns:

- `<target>` is a declared variable (or `var.name`) or one of the writable `defaults`
  parameters (`defaults.button_brightness`, `defaults.encoder_brightness`). The
  read-only `defaults` timing constants cannot be assigned.
- `<op>` is `:=` (clamp/truncate and warn), `~=` (same, silently) or `=` (reject an
  out-of-range/over-long value).
- `<rhs>` is an integer literal, a `"quoted string"`, another variable (`$b`) or a
  `$(command)` substitution.

An int is clamped into `min..=max`, a str is truncated to `max_length`, and a wrong-type
right-hand side is always a config error (`"100"` is a string, not the number `100`).

`$(command)` runs when the action fires and converts the output to the target's type: an
int target parses the first non-empty line (trimmed) and clamps it; a str target takes
the whole output with trailing newlines stripped and truncates it. For `=`, an
unconvertible output, a non-zero exit or a timeout is an error and the variable is left
unchanged; for `:=`/`~=` the target's default (a variable's declared `value`, or the
configured brightness default) is used, with a warning for `:=`.

Assignments write runtime state only: `config.json` is never modified.

#### Commands and the shell

A command - an action command, an `image_exec`/`text_exec`/`launch` `params`, or a
`$(command)` - runs directly when it contains no unquoted shell operator, and through
`sh -c` when it contains one of `` | & ; < > ` ( ) `` or a newline. Pipelines and
redirection therefore work when you write them, while a plain command never grows a
shell.

#### Actions run in parallel

Actions in an array are not ordered against each other: each command and each `$(command)`
assignment runs on its own task. So

```json
"pressed": ["$b = $(echo 1)", "$a = $(echo $b)"]
```

assigns the **old** value of `$b` to `$a`, because both commands read `$b` before either
finishes. Do not rely on the order of independent assignments in one action list.

See `examples/EXAMPLES.md` for complete worked examples, including an encoder that
changes screen brightness using `bc`.

### Scenes

A scene name can be any text. The special `on_start` scene is reserved and is executed when the program starts.

Buttons and encoders in `setup` and `actions` are addressed by **control references** of the form

```
<device><b|e><number>
```

- `<device>` is a single digit `1`–`9` naming a logical device. Each digit names one device definition from the `devices` section; at startup every definition matched to discovered hardware is driven, so a reference names a specific physical device by its id.
- `b` addresses a button, `e` an encoder.
- `<number>` is the control number, always written with two digits, `01`–`99` (the AKP03E has 9 buttons and 3 encoders).

For example `1b01` is button 1 on device 1, and `2e01` is encoder 1 on device 2.

Rules for the program:

- References to devices that are **not present** — whose definition was not matched to any discovered device — are skipped at runtime with a warning (`device N is referenced but not present`).
- Encoder references (`1e01`) work in `actions`: `turn_cw` / `turn_ccw` bind one rotation notch each, and the knob push binds the same `pressed` / `released` / `short_press` / `long_press` / `double_click` events as a button. Encoders are not allowed in `setup`: `setup` configures button screens only, and a `setup` entry on an `e` key — any `type` of `image`, `text`, `image_exec` or `text_exec` — is a config error and the program refuses to start.
- Assigning an image to a button that has no display (`"screen": false` in its device definition) is skipped at runtime with a warning; the file is not even read and nothing is transferred to the device.
- Out-of-range button or encoder references (e.g. `1b99` on a 9-button device, or `1e04` on a 3-encoder one) are skipped with a warning.
- The old plain numeric keys (`"1"`, `"3"`, ...) are no longer accepted; update them to `"1b01"`, `"1b03"`, ...

Each scene is a dictionary with two reserved keys: `setup` (button content) and `actions` (per-key bindings). A missing `setup` or `actions` simply means "empty". Button content from the previous scene is kept for any button not listed in `setup`:

- `setup` — a dictionary of control references. Each key (`1b01`, `1b02`, ...) maps a physical button to a dictionary with `type`, `params` and an optional `refresh`:
  - `{"type":"image","params":"path"}` — load an image from `path` onto the button
  - `{"type":"image_exec","params":"program args..."}` — run `program args...` asynchronously and use its stdout as the button image; the program must print a valid image file to stdout. If it does not finish within 5 seconds, or the button is changed in the meantime, the process is killed, an error is logged, and the button shows the text "Error" in red.
  - `{"type":"text","params":"path"}` — display the first 6 characters of the first 3 lines of the file `path`
  - `{"type":"text_exec","params":"program args..."}` — run `program args...` asynchronously and show its stdout the same way (first 6 characters of its first 3 lines); the program must exit on its own, and a timeout or reassignment kills it and draws "Error" in red, just like `image_exec`
  - `{"type":"launch","params":"program args..."}` — run `program args...` fully detached from this program: its own process group, no stdio, and it keeps running (re-parented to init) after this program exits, so it is never killed or waited on. The button is only a config slot; nothing is drawn on it and nothing is restored on termination
  - `{"type":"clear"}` — clear the button image
  - `refresh` (optional, seconds, default `0`) — on `image`, `text`, `image_exec` and `text_exec` only, re-applies this entry on its own every `refresh` seconds, without touching any other button or re-applying the rest of the scene. `0` (or omitting it) means "apply once on scene entry, never again" — today's behavior. A button's refresh, like its content, is tied to whichever scene last explicitly defined it: switching to a scene that does not mention the button leaves both its display and its refresh schedule running; a later scene that does redefine the button replaces both, whether or not the new definition itself refreshes. Not allowed (a config error) on `clear` or `launch`, which have nothing left to redraw. A refresh restarts `image_exec`/`text_exec` the same way reassigning the button does — it kills any still-running process for that key — so pick an interval comfortably longer than the command's typical runtime, or it will be killed before it ever finishes.
- `actions` — a dictionary of per-control behavior. Keys are control references (e.g. `1b01`) and map to the actions for `short_press`, `long_press`, `double_click`, `pressed` and `released`. The complex events fire on release as described in [Defaults](#defaults), while `pressed` fires on the press edge and `released` on the release edge. An encoder reference (e.g. `1e01`) additionally maps the `turn_cw` and `turn_ccw` keys, which bind one rotation notch in each direction; pushing an encoder knob addresses the same five press events on the encoder reference. The special key `timer` maps to a single-element dictionary `{ "<seconds>": "<action>" }` — the action runs once that many seconds have passed after entering the scene.

Use `short_press`, `long_press` or `double_click` for ordinary button actions: they fire on release and cover a full click, so a single action is all you usually need. `pressed` and `released` are low-level edge events — they fire instantly on the down/up edge and, unlike complex presses, are not held back so a double click can be recognized. Reach for them only when you truly need to react to the exact press or release instant (for example to start something on `pressed` and stop it on `released`).

Action values have these forms:
  - a bare `@` — stay on the same scene (re-applies its `setup`)
  - `@<scene>` (e.g. `@Main`) — jump to the named scene
  - `$<target> <op> <rhs>` — assign to a variable or a writable `defaults` parameter; see [Variables](#variables)
  - anything else — the path of a command to execute, followed by its parameters
  - an array of any of the above, run without waiting on each other - see below

A `$` reference anywhere in a value is substituted before the value is used; see
[Variables](#variables) for the reference syntax, the escaping rules, and the breaking
change to commands that used a literal `$`.

Commands are executed asynchronously, so a running command does not block button input or the timer. `dak` runs a command directly when it needs no shell, and through `sh -c` when it contains an unquoted shell operator - see [Commands and the shell](#commands-and-the-shell).

A leading `~` in a command line - the `params` of an `image_exec`/`text_exec`, a
`launch` value, or a plain command action (the fourth form above) - is expanded to
your home directory, same as a shell: `~` alone becomes `$HOME`, and `~/rest` becomes
`$HOME/rest`. This applies to every word of the line, the program and each argument
alike, not just the first. `~` anywhere but the very start of a word is left
untouched, and there is no `~user` support.

Assignment to a variable or a writable default happens immediately, without touching
`config.json` and without persisting across a reconnect - e.g.
`"$defaults.button_brightness := 100"` sets the device's brightness, and
`"$name := $other"` copies one variable into another. See [Variables](#variables) for
the full syntax, targets, operators and right-hand sides.

An event's value (and the `timer` value) may be a plain string, as above, or an array
of them, e.g. `"short_press": ["/usr/bin/notify-send hi", "@Main"]`, to trigger more
than one action from a single event. Every entry runs without waiting on the others to
finish - a command never blocks anything else in the list, and there can be at most one
scene-changing entry (`@` or `@scene`) per list, enforced when the config loads. `[]` is
the array form's way to spell "bound but no action" (matching `""` for the plain-string
form); an empty string *inside* a non-empty array is rejected instead of silently
skipped. A control listed in `actions` with no events at all (e.g. `"1b01": {}`) is
almost certainly a mistake and is warned about, though it is not an error. Because the
entries run in parallel, do not rely on the order of assignments within one array - see
[Actions run in parallel](#actions-run-in-parallel).

Example scenes (paths and commands below are illustrative placeholders - adjust
them to your own system and OS; `text2gif`/`aplay` are just example commands, not
tools `dak` ships or requires). `Main`'s clock button (`1b03`) uses `refresh` to
keep itself updated every second on its own, instead of the whole-scene
`"timer": {"1": "@"}` trick older configs needed (still shown on `on_start` and
`Test`, and still the only option for anything a per-button `refresh` cannot
express, like switching scenes on a schedule). `Test`'s `1b01` button demonstrates
the array form: one `short_press` plays a sound and returns to `on_start`, instead
of needing two separate buttons:

```json
{
  "on_start": {
    "setup": {
      "1b01": { "type": "image", "params": "/usr/lib/python3/dist-packages/smartcard/wx/resources/reader.ico" },
      "1b02": { "type": "image_exec", "params": "/usr/bin/text2gif -t Start" },
      "1b03": { "type": "text_exec", "params": "/usr/bin/date +%H:%M" },
      "1b04": { "type": "text", "params": "/tmp/aaa.txt" }
    },
    "actions": {
      "1b01": { "short_press": "@", "long_press": "", "double_click": "", "pressed": "", "released": "" },
      "1b02": { "short_press": "@Test", "long_press": "", "double_click": "", "pressed": "", "released": "" },
      "timer": { "1": "@Main" }
    }
  },
  "Main": {
    "setup": {
      "1b03": { "type": "text_exec", "params": "/usr/bin/date +%H:%M", "refresh": 1 },
      "1b04": { "type": "text", "params": "/tmp/aaa.txt" }
    },
    "actions": {}
  },
  "Test": {
    "setup": {
      "1b02": { "type": "image_exec", "params": "/usr/bin/text2gif -t Test" },
      "1b03": { "type": "clear" }
    },
    "actions": {
      "1b01": { "short_press": ["/usr/bin/aplay /usr/share/sounds/sound-icons/prompt.wav", "@on_start"], "long_press": "", "double_click": "", "pressed": "", "released": "" },
      "timer": { "1": "@" }
    }
  }
}
```

### Devices

The `devices` section declares the individual devices the config drives. Its keys are **logical device ids** (single digits `1`–`9`, the same digits the control references use), and each value is a device definition — the exact JSON that `dak --map` prints when it has walked you through picking a device and capturing its buttons and encoders:

```
dak --map
```

Each `buttons` entry maps a button `number` to the raw codes it sends when pressed and released, and whether the button has a screen (`screen` `true`/`false` with its `draw_id`). Each `encoders` entry maps an encoder `number` to its `cw`/`ccw` codes — one `turn_cw`/`turn_ccw` action per rotation notch — and, after the wizard replays a knob push, its `press`/`release` codes; an encoder without push codes still turns, but its knob push is ignored at runtime.

`protocol_version` is optional (omit it, or set it to JSON `null`, to fall back to the
default): it picks which of `mirajazz`'s connection protocol variants `dak` speaks to
this device with (see [Help me support more devices](#help-me-support-more-devices)
for the wider device family this matters for). `dak --map` fills it in with the
recognized device kind's own default and prints the same value to the console while
running; set it by hand to override that default for a specific unit, e.g. if a kind
this project hasn't verified itself needs a different value than the one currently
wired up for it.

A device definition looks like this:

```json
{
  "device_id": "0300:3002",
  "device_name": "Ajazz HOTSPOTEKUSB HID DEMO",
  "serial": "unknown",
  "key_count": 9,
  "encoder_count": 3,
  "screens": 6,
  "protocol_version": 2,
  "buttons": [
    { "number": 1, "press": 1, "release": 1, "screen": true, "draw_id": 1 },
    { "number": 2, "press": 2, "release": 2, "screen": true, "draw_id": 2 }
  ],
  "encoders": [
    { "number": 1, "cw": 144, "ccw": 145, "press": 146, "release": 146 }
  ]
}
```

At startup each definition is matched against the discovered hardware:

- A definition whose serial is anything but `"unknown"` matches only the device reporting that exact serial, which tells identical devices apart.
- A definition whose serial is `"unknown"` falls back to comparing the VID:PID string (`device_id` vs. the device's vendor/product ids), so devices without serials still work as long as only one of their kind is connected.

Every matched device is connected using the key and encoder counts from its own definition and driven with the shared scenes: the `on_start` scene is applied on it, and its buttons/timers run the `setup` and `actions` entries, addressed by the device's own id. A device defined in config but not found is reported with a warning, a discovered device with no config definition is ignored with a warning, and when no configured device is found the program exits with an error.

Driving multiple devices at once (multiple entries under `devices`, each on its own connection and input loop) is implemented - see `main.rs`'s `run_device`, spawned once per matched device - but has only been exercised with mocked devices in tests, never against two or more real keypads attached at the same time. Treat it as unverified in practice until someone confirms it against actual hardware.

A complete config combining both sections looks like:

```json
{
  "scenes": {
    "on_start": {
      "setup": {
        "1b01": { "type": "image", "params": "/path/reader.ico" }
      },
      "actions": {
        "timer": { "1": "@Main" }
      }
    }
  },
  "devices": {
    "1": {
      "device_id": "0300:3002",
      "device_name": "Ajazz HOTSPOTEKUSB HID DEMO",
      "serial": "unknown",
      "key_count": 9,
      "encoder_count": 3,
      "screens": 6,
      "protocol_version": 2,
      "buttons": [
        { "number": 1, "press": 1, "release": 1, "screen": true, "draw_id": 1 }
      ],
      "encoders": []
    }
  }
}
```

## Device install

See [INSTALL.md](INSTALL.md) for one-time device/permissions setup (Linux udev
rules, FreeBSD hidraw setup).

## Help me support more devices

DAK is built on [`mirajazz`](https://crates.io/crates/mirajazz), which targets the whole
family of Ajazz- and Mirabox-branded "stream controller" keypads: the two brands sell the
same OEM hardware under different USB vendor/product IDs, and `mirajazz` groups devices by
an internal "protocol version" rather than by brand. DAK recognizes (via `dak --map` and
normal startup) every device ID listed in the table below, taken from
[`opendeck-akp03`](https://github.com/4ndv/opendeck-akp03)'s "Supported devices" list - the
reference OpenDeck plugin the `mirajazz` author built for exactly this device family.

**Only the Ajazz AKP03E (rev. 2) row is actually verified against real hardware by this
project.** Every other row is wired up exactly as `opendeck-akp03` documents it (protocol
version and image format included) but has not been tested by anyone working on DAK - best
case it works out of the box, worst case it connects but renders images incorrectly, or
fails to connect cleanly (an error, not a crash).

| Device | USB ID | Verified? |
| --- | --- | --- |
| Ajazz AKP03 | `0300:1001` | No |
| Ajazz AKP03E | `0300:1002` | No |
| Ajazz AKP03R | `0300:1003` | No |
| **Ajazz AKP03E (rev. 2)** | **`0300:3002`** | **Yes** |
| Ajazz AKP03R (rev. 2) | `0300:3003` | No |
| Mirabox N3 | `6602:1000` | No |
| Mirabox N3 | `6602:1002` | No |
| Mirabox N3 | `6603:1002` | No |
| Mirabox N3 | `6603:1003` | No |
| Soomfon Stream Controller SE | `1500:3001` | No |
| Mars Gaming MSD-TWO | `0B00:1001` | No |
| TreasLin N3 | `5548:1001` | No |
| Redragon Skyrider SS-551 | `0200:2000` | No |

`dak --map` exists specifically to walk through and capture whatever button/screen/encoder
layout is actually connected, for any of the devices above, rather than assuming one.

If you're running DAK against a device from the table above (verified or not), or one with
a USB ID not listed at all, please [open a GitHub
issue](https://github.com/theholm/dak/issues) with:

1. Your device's raw USB vendor/product ID (from `lsusb`, Windows Device Manager, or
   similar) if `dak --map` doesn't recognize it at all, or the full JSON output of
   `dak --map` if it does, and
2. The output of a normal run with `-d device` enabled (`dak -d device`), pressing
   every button and turning/pushing every encoder at least once so its codes show up
   in the log, and
3. Whether the button screens actually rendered correctly (for a device not yet marked
   verified above) - the single most useful thing you can confirm that this project has
   no way to check itself without owning the hardware.

Both together let us build up a database of which models report which raw codes and which
ones actually work end-to-end, without needing physical access to every device model
ourselves.

**Side/secondary screens are not supported.** Some Mirabox devices (e.g. the Mirabox
293S/293SV3 family) have an extra small LCD screen that isn't tied to any button or
encoder. `mirajazz` (the library DAK is built on) has no working way to address such a
screen - image writes are only ever keyed by button index, and while its source has
leftover scaffolding suggesting a screen concept was once planned (an unused `ImageRect`
type and `MirajazzError::NoScreen` variant), nothing in the library actually sends to one.
Until `mirajazz` gains real support for this, DAK cannot drive these secondary screens
either.

## TODO

1. The FreeBSD build vendors its own copies of `mirajazz` and `async-hid` under
   `vendor/` (see `vendor/README.md` for why), pinned to specific upstream versions
   (`mirajazz` 0.16.0, `async-hid` 0.5.3) rather than tracking crates.io like every
   other platform's dependencies do. Nothing currently checks whether newer upstream
   releases of either crate exist. Add a CI job that periodically checks
   crates.io for newer `mirajazz`/`async-hid` versions than the ones vendored, so a
   security fix or bugfix upstream doesn't silently sit unnoticed for the FreeBSD
   build - see `vendor/README.md`'s "Updating" section for the manual re-vendoring
   steps such a check would need to prompt for.
2. Improve `-d scene` logging: add a single old->new scene-transition line
   (with cause: key press, timer, or command), timing info for
   `enter_scene`/`apply_scene_operations`, and gate scene-application
   skip/no-op warnings behind the `scene` debug subsystem instead of always
   printing them via `log.warn`.
3. Variables landed in v0.10.0 (declarations, `$name` substitution, the three
   assignment operators, and `$(command)` output capture - see [Variables](#variables)).
   Still to come: per-scene variables (set only for the duration of one scene),
   arithmetic/expression right-hand sides (calculations are delegated to external tools
   like `bc` for now), explicit `str(...)`/`int(...)` type conversions, an `env.` scope
   for environment variables, and a per-scene "compute" section that runs commands to
   populate variables.
4. A built-in Lua (or similar) scripting language for advanced control - may be a bad
   idea: it's a much bigger surface (a whole embedded interpreter, its own error
   handling, a new config/script relationship to design) than anything else on this
   list, and might be better served by composing existing primitives (commands,
   variables once they exist, multiple actions per event) instead of adding a second
   configuration language alongside JSON.
5. Animated button images: a button's screen can already be redrawn continuously with
   a different image each frame at a solid 30fps, across all of a device's screens at
   once, with no config support needed for that at all - confirmed against real
   hardware, including a 3-hour zero-error run (see `NOTES.md` section 6 for the full
   findings, including where the real throughput ceiling is). What's missing is a way
   to *configure* one from `config.json`: today `setup` only ever pushes a single
   static frame (`"image"`/`"image_exec"`), with no way to say "cycle through these
   frames" or "call this command every N ms and redraw". Needs a new setup type (or an
   extension of the existing ones) plus a way to control the frame rate.
6. (Maybe - lower priority, and only relevant once #5 above exists) A sleep/low-power
   mode for animations specifically: redrawing a button continuously costs real,
   measurable CPU and HID bandwidth (see `NOTES.md` section 6) even though today's
   measured headroom is ample - it might be worth pausing an animation's redraws once
   its scene isn't the active one, rather than only when a config author remembers to
   stop it explicitly. There's no existing signal for "is anyone actually looking at
   this button" beyond scene switches and button presses, so this may not be worth the
   added complexity - noted as a "maybe", not a commitment.
7. Encoder LED *color* control from config (`mirajazz`'s `Device::set_led_colors`,
   distinct from the `encoder_brightness` default added in v0.9.0, which only
   controls overall LED brightness). Not implemented yet for two reasons: it can't be
   verified against real hardware (no Ajazz AKP03E/AKP03R unit with functioning
   encoder LEDs was available during development - `set_led_colors` and
   `set_led_brightness` both return success even on a unit with no LEDs wired up at
   all, so a successful call proves nothing), and the mapping between the LED-color
   array's index order and the physical/logical encoder numbering used elsewhere
   (`turn_cw`/`turn_ccw`/push references) is unconfirmed - a brute-force sweep setting
   one color-array index at a time across a wide range found no visible correlation
   before the sweep was cut short by the device repeatedly dropping off USB when
   hammered with this command. Needs a real unit with working encoder LEDs to
   properly map and add this safely.
## License

This project is licensed under the **GNU Affero General Public License v3.0** (AGPL-3.0). See [LICENSE](LICENSE) for the full license text. This program is free software: you can redistribute it and/or modify it under the terms of the AGPL as published by the Free Software Foundation, either version 3 of the License, or (at your option) any later version.
