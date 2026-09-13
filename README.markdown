# DAK — Dynamic Ajazz Keyboard

**DAK** (**D**ynamic **A**jazz **K**eyboard) is a Rust tool for controlling an **Ajazz AKP03E / AKP03R** USB macro keypad (HID device, vendor `0x0300`, product `0x3002`). Running the binary connects to the device, paints the configured images and text labels onto the button LCDs, controls brightness, and reacts to key and encoder input. Loosely based on [OpenDesk pkugin](https://github.com/4ndv/opendeck-akp03/)    
        
Work in progress. Config structure will probably change in the future, but I will try to make it easy to update to new version.


# THIS IS VIBE CODED(mostly) GARBAGE(100%, as non vibe-coded parts are garbage too), USE ON YOUR OWN RISK

I did not check what is in the code at all, so who knows what it is really doing.

The current version is **v0.3.0-dev**.

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
- **Per-key actions** — `pressed`, `released`, `short_press`, `long_press` and `double_click` bindings, with actions inherited from the previously active scene.
- **Timers** — each scene can run a `timer` action after a number of seconds; button presses and scene switches re-arm it only when appropriate.

## Platform support

The target platforms are generic Linux and FreeBSD. No effort is made (or planned) to make the code compile and run under any other platform.

## Config structure

`config.json` drives all runtime behavior. Keys in the top-level object denote **scenes**, and a scene name can be any text. The special `on_start` scene is reserved and is executed when the program starts.

Each scene is a dictionary with two reserved keys: `setup` (button content) and `actions` (per-key bindings). A missing `setup` or `actions` simply means "empty". Button content from the previous scene is kept for any button not listed in `setup`:

- `setup` — a dictionary of numbered buttons. Numbered keys map a physical button to a dictionary with `type` and `params`:
  - `{"type":"image","params":"path"}` — load an image from `path` onto the button
  - `{"type":"image_exec","params":"program args..."}` — run `program args...` asynchronously and use its stdout as the button image; the program must print a valid image file to stdout. If it does not finish within 5 seconds, or the button is changed in the meantime, the process is killed, an error is logged, and the button shows the text "Error" in red.
  - `{"type":"text","params":"path"}` — display the first 6 characters of the first 3 lines of the file `path`
  - `{"type":"text_exec","params":"program args..."}` — run `program args...` asynchronously and show its stdout the same way (first 6 characters of its first 3 lines); the program must exit on its own, and a timeout or reassignment kills it and draws "Error" in red, just like `image_exec`
  - `{"type":"launch","params":"program args..."}` — run `program args...` fully detached from this program: its own process group, no stdio, and it keeps running (re-parented to init) after this program exits, so it is never killed or waited on. The button is only a config slot; nothing is drawn on it and nothing is restored on termination
  - `{"type":"clear"}` — clear the button image
- `actions` — a dictionary of per-key behavior. Numerical keys are button numbers and map to the actions for `pressed`, `released`, `short_press`, `long_press` and `double_click`. The special key `timer` maps to a single-element dictionary `{ "<seconds>": "<action>" }` — the action runs once that many seconds have passed after entering the scene.

Action values have three forms:
  - `~` — stay on the same scene
  - `@<scene>` (e.g. `@Main`) — jump to the named scene
  - anything else — the path of a command to execute, followed by its parameters

Commands are executed asynchronously, so a running command does not block button input or the timer.

Example:

```json
{
  "on_start": {
    "setup": {
      "1": { "type": "image", "params": "/usr/lib/python3/dist-packages/smartcard/wx/resources/reader.ico" },
      "2": { "type": "image_exec", "params": "/usr/bin/text2gif -t Start" },
      "3": { "type": "text_exec", "params": "/usr/bin/date +%H:%M" },
      "4": { "type": "text", "params": "/proc/uptime" }
    },
    "actions": {
      "1": { "pressed": "~", "released": "", "short_press": "", "long_press": "", "double_click": "" },
      "2": { "pressed": "@Test", "released": "", "short_press": "", "long_press": "", "double_click": "" },
      "timer": { "1": "@Main" }
    }
  },
  "Main": {
    "setup": {
      "3": { "type": "text_exec", "params": "/usr/bin/date +%H:%M" },
      "4": { "type": "text", "params": "/proc/uptime" }
    },
    "actions": {
      "timer": { "1": "~" }
    }
  },
  "Test": {
    "setup": {
      "2": { "type": "image_exec", "params": "/usr/bin/text2gif -t Test" },
      "3": { "type": "clear" }
    },
    "actions": {
      "1": { "pressed": "/usr/bin/aplay /usr/share/sounds/sound-icons/prompt.wav", "released": "", "short_press": "", "long_press": "", "double_click": "" },
      "3": { "pressed": "@on_start", "released": "", "short_press": "", "long_press": "", "double_click": "" },
      "timer": { "1": "~" }
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

## License

This project is licensed under the **GNU Affero General Public License v3.0** (AGPL-3.0). See [LICENSE](LICENSE) for the full license text. This program is free software: you can redistribute it and/or modify it under the terms of the AGPL as published by the Free Software Foundation, either version 3 of the License, or (at your option) any later version.
