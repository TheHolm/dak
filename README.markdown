# DAK — Dynamic Ajazz Keyboard

**DAK** (**D**ynamic **A**jazz **K**eyboard) is a Rust tool for controlling an **Ajazz AKP03E / AKP03R** USB macro keypad (HID device, vendor `0x0300`, product `0x3002`). Running the binary connects to the device, paints the configured images and text labels onto the button LCDs, controls brightness, and reacts to key and encoder input. Loosely based on [OpenDesk pkugin](https://github.com/4ndv/opendeck-akp03/)    
        
Work in progress. Config structure will probably change in the future, but I will try to make it easy to update to new version.


# THIS IS VIBE CODED(mostly) GARBAGE(100%, as non vibe-coded parts are garbage too), USE ON YOUR OWN RISK

I did not check what is in the code at all, so who knows what it is really doing.

The current version is **v0.5.1**.

## Usage

```
dak [OPTIONS]

Options:
  -c, --config <CONFIG>   Path to the config file; when omitted, `config.json` is
                          searched for in ~/.config/dak/, then the current
                          directory, then the directory containing the binary
  -d, --debug <DEBUG>...  Debug subsystems to enable, comma-separated: device, scene, action
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

All runtime behavior is driven by `config.json` instead of the hard-coded demo once shipped in `main.rs`:

- **Scenes** — buttons can display images, static text files, or the output of async commands (`image_exec` / `text_exec` with a 5-second timeout); scenes switch on key/encoder input, from the scene timer, or on demand.
- **Per-key actions** — `short_press`, `long_press`, `double_click`, `pressed` and `released` bindings, with actions inherited from the previously active scene.
- **Encoders** — each encoder binds `turn_cw` / `turn_ccw` per rotation notch, and the knob itself is pressable like a button: pushing it fires the same `pressed` / `released` / `short_press` / `long_press` / `double_click` events.
- **Timers** — each scene can run a `timer` action after a number of seconds; button presses and scene switches re-arm it only when appropriate.

## Platform support

The target platforms are generic Linux and FreeBSD. No effort is made (or planned) to make the code compile and run under any other platform.

## Config structure

`config.json` drives all runtime behavior. The top level of the config is a dictionary with up to three keys:

- `"scenes"` — the scenes dictionary (see [Scenes](#scenes))
- `"devices"` — the individual device definitions (see [Devices](#devices))
- `"defaults"` — optional press-detection timing knobs (see [Defaults](#defaults))

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

The optional top-level `defaults` section tunes how the complex button presses are detected. All keys are optional and fall back to their built-in values when missing:

```json
"defaults": {
  "short_press_duration": 300,
  "double_click_gap": 300
}
```

- `short_press_duration` (default `300`, milliseconds) — a press released at most this long after it started is a `short_press`; a press held any longer is a `long_press`.
- `double_click_gap` (default `300`, milliseconds) — two presses form a `double_click` when the second one lands within this long of the previous release.

Complex events are decided on the release edge, per pressable control: buttons and
pushed encoders share the same detection, so a pushed encoder's knob behaves exactly
like an extra button:

- a press held past `short_press_duration` fires `long_press` on its release;
- a second press landing inside `double_click_gap` of the previous release is a `double_click` firing on that second release, no matter how long the second press is held, and its first click never fires `short_press`;
- anything else is a `short_press`, which only fires once `double_click_gap` has passed without a second press — so the first click of a double click is never reported as a short press too.

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

- `setup` — a dictionary of control references. Each key (`1b01`, `1b02`, ...) maps a physical button to a dictionary with `type` and `params`:
  - `{"type":"image","params":"path"}` — load an image from `path` onto the button
  - `{"type":"image_exec","params":"program args..."}` — run `program args...` asynchronously and use its stdout as the button image; the program must print a valid image file to stdout. If it does not finish within 5 seconds, or the button is changed in the meantime, the process is killed, an error is logged, and the button shows the text "Error" in red.
  - `{"type":"text","params":"path"}` — display the first 6 characters of the first 3 lines of the file `path`
  - `{"type":"text_exec","params":"program args..."}` — run `program args...` asynchronously and show its stdout the same way (first 6 characters of its first 3 lines); the program must exit on its own, and a timeout or reassignment kills it and draws "Error" in red, just like `image_exec`
  - `{"type":"launch","params":"program args..."}` — run `program args...` fully detached from this program: its own process group, no stdio, and it keeps running (re-parented to init) after this program exits, so it is never killed or waited on. The button is only a config slot; nothing is drawn on it and nothing is restored on termination
  - `{"type":"clear"}` — clear the button image
- `actions` — a dictionary of per-control behavior. Keys are control references (e.g. `1b01`) and map to the actions for `short_press`, `long_press`, `double_click`, `pressed` and `released`. The complex events fire on release as described in [Defaults](#defaults), while `pressed` fires on the press edge and `released` on the release edge. An encoder reference (e.g. `1e01`) additionally maps the `turn_cw` and `turn_ccw` keys, which bind one rotation notch in each direction; pushing an encoder knob addresses the same five press events on the encoder reference. The special key `timer` maps to a single-element dictionary `{ "<seconds>": "<action>" }` — the action runs once that many seconds have passed after entering the scene.

Use `short_press`, `long_press` or `double_click` for ordinary button actions: they fire on release and cover a full click, so a single action is all you usually need. `pressed` and `released` are low-level edge events — they fire instantly on the down/up edge and, unlike complex presses, are not held back so a double click can be recognized. Reach for them only when you truly need to react to the exact press or release instant (for example to start something on `pressed` and stop it on `released`).

Action values have three forms:
  - `~` — stay on the same scene
  - `@<scene>` (e.g. `@Main`) — jump to the named scene
  - anything else — the path of a command to execute, followed by its parameters

Commands are executed asynchronously, so a running command does not block button input or the timer.

Example scenes:

```json
{
  "on_start": {
    "setup": {
      "1b01": { "type": "image", "params": "/usr/lib/python3/dist-packages/smartcard/wx/resources/reader.ico" },
      "1b02": { "type": "image_exec", "params": "/usr/bin/text2gif -t Start" },
      "1b03": { "type": "text_exec", "params": "/usr/bin/date +%H:%M" },
      "1b04": { "type": "text", "params": "/proc/uptime" }
    },
    "actions": {
      "1b01": { "short_press": "~", "long_press": "", "double_click": "", "pressed": "", "released": "" },
      "1b02": { "short_press": "@Test", "long_press": "", "double_click": "", "pressed": "", "released": "" },
      "timer": { "1": "@Main" }
    }
  },
  "Main": {
    "setup": {
      "1b03": { "type": "text_exec", "params": "/usr/bin/date +%H:%M" },
      "1b04": { "type": "text", "params": "/proc/uptime" }
    },
    "actions": {
      "timer": { "1": "~" }
    }
  },
  "Test": {
    "setup": {
      "1b02": { "type": "image_exec", "params": "/usr/bin/text2gif -t Test" },
      "1b03": { "type": "clear" }
    },
    "actions": {
      "1b01": { "short_press": "/usr/bin/aplay /usr/share/sounds/sound-icons/prompt.wav", "long_press": "", "double_click": "", "pressed": "", "released": "" },
      "1b03": { "short_press": "@on_start", "long_press": "", "double_click": "", "pressed": "", "released": "" },
      "timer": { "1": "~" }
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

A device definition looks like this:

```json
{
  "device_id": "0300:3002",
  "device_name": "Ajazz HOTSPOTEKUSB HID DEMO",
  "serial": "unknown",
  "key_count": 9,
  "encoder_count": 3,
  "screens": 6,
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
      "buttons": [
        { "number": 1, "press": 1, "release": 1, "screen": true, "draw_id": 1 }
      ],
      "encoders": []
    }
  }
}
```

## Device install

1. Find your device and record ID.
```
# lsusb
Bus 003 Device 020: ID 0300:3002 Ajazz HOTSPOTEKUSB HID DEMO
```
2. Create UDEV rule to give regular user access to the device
change GROUP= to some group appropriate to your system which your user is member of.

```
#cat /etc/udev/rules.d/ajazz_akp03e.conf
SUBSYSTEM=="usb", ATTR{idVendor}=="0300", ATTR{idProduct}=="3002", MODE="0660", TAG+="uaccess", GROUP="plugdev"
SUBSYSTEM=="usb", ATTRS{idVendor}=="0300", ATTRS{idProduct}=="3002", MODE="0660", TAG+="uaccess", GROUP="plugdev"
KERNEL=="hidraw*", SUBSYSTEM=="hidraw", ATTR{idVendor}=="0300", ATTR{idProduct}=="3002", MODE="0660", TAG+="uaccess", GROUP="plugdev"
KERNEL=="hidraw*", SUBSYSTEM=="hidraw", ATTRS{idVendor}=="0300", ATTRS{idProduct}=="3002", MODE="0660", TAG+="uaccess", GROUP="plugdev"
```     
```
sudo chown root:root /etc/udev/rules.d/ajazz_akp03e.conf
```

Device list is imported from https://github.com/4ndv/opendeck-akp03/

### FreeBSD

FreeBSD needs its `hidraw(4)` driver (Linux-`hidraw`-compatible, FreeBSD 13+), which
is not enabled by default - `uhid(4)` claims the device instead otherwise, and `dak`
does not work against `uhid(4)` (see `vendor/README.md` for why). One-time setup:

```
# Load the hidraw module now, and on every boot:
kldload hidraw
echo 'hidraw_load="YES"' | sudo tee -a /boot/loader.conf

# Prefer hidraw over uhid for HID interfaces, now and on every boot:
sudo sysctl hw.usb.usbhid.enable=1
echo 'hw.usb.usbhid.enable=1' | sudo tee -a /etc/sysctl.conf
```

`/dev/hidrawN` nodes default to `0600 root:operator` (root-only, even for group
members) - add a `devfs.rules(5)` entry granting your user's group read/write
access, analogous to the udev rule above, e.g.:

```
# /etc/devfs.rules
[dakrules=10]
add path 'hidraw*' mode 0660 group operator
```

Activate it and add your user to that group:

```
sudo sysrc devfs_system_ruleset=dakrules
sudo pw groupmod operator -m yourusername
```

Then either reboot, or apply immediately without one:

```
sudo service devfs restart
sudo usbconfig -d ugenX.Y reset   # replug the device, or reset it like this,
                                   # so it re-attaches under /dev/hidrawN
                                   # instead of /dev/uhidN
```
(log out and back in too, so your shell picks up the new group membership).

## TODO

- The FreeBSD build vendors its own copies of `mirajazz` and `async-hid` under
  `vendor/` (see `vendor/README.md` for why), pinned to specific upstream versions
  (`mirajazz` 0.16.0, `async-hid` 0.5.3) rather than tracking crates.io like every
  other platform's dependencies do. Nothing currently checks whether newer upstream
  releases of either crate exist. Add a CI job that periodically checks
  crates.io for newer `mirajazz`/`async-hid` versions than the ones vendored, so a
  security fix or bugfix upstream doesn't silently sit unnoticed for the FreeBSD
  build - see `vendor/README.md`'s "Updating" section for the manual re-vendoring
  steps such a check would need to prompt for.

## License

This project is licensed under the **GNU Affero General Public License v3.0** (AGPL-3.0). See [LICENSE](LICENSE) for the full license text. This program is free software: you can redistribute it and/or modify it under the terms of the AGPL as published by the Free Software Foundation, either version 3 of the License, or (at your option) any later version.
