# Installing dak

Building from source, and one-time device/permissions setup so `dak` can talk
to the Ajazz AKP03E / AKP03R over USB HID as a regular (non-root) user.
Device list is imported from https://github.com/4ndv/opendeck-akp03/

## Building from source

Most users don't need this - see [GitHub Releases](https://github.com/theholm/dak/releases)
for prebuilt packages instead. Building from source needs `git` and a
reasonably recent stable Rust toolchain, on both Linux and FreeBSD:

```
git clone https://github.com/theholm/dak.git
cd dak
cargo build --release
```

The binary is at `target/release/dak`. `cargo test` runs the full suite,
including the hardware-gated tests in `tests/hardware.rs`/
`tests/hardware_read_loop.rs` - they need the device permission setup below
to actually exercise real hardware, and skip themselves cleanly otherwise.

Your OS's packaged Rust toolchain may be too old: e.g. Debian 13 (trixie)
ships `rustc` 1.85, but `image` and `built` (two of `dak`'s dependencies)
need 1.88 and 1.87 respectively, so `cargo build` fails with `rustc x.y.z is
not supported by the following packages`. Install a current toolchain via
[rustup.rs](https://rustup.rs) instead of (or alongside) your OS package if
you hit this.

### FreeBSD-specific note

If you've set up local cross-compilation from Linux to FreeBSD per
`NOTES.md`, that leaves a repo-root `.cargo/config.toml` (gitignored, not
part of the repo) pinning the `x86_64-unknown-freebsd` target to a
Linux-side sysroot path. Remove or rename that file before building
*natively* on a real FreeBSD machine - the native target triple is the same
one that config overrides, so Cargo applies it there too, and the build
fails at the link step looking for libraries (`-lexecinfo`, `-lpthread`,
...) under a sysroot path that only exists on the Linux machine you cross-
compiled from.

## Man pages

The `.deb` and `.pkg` packages install **dak(1)** and **dak-config(5)** into
the system man tree, so `man dak` and `man dak-config` work after a package
install. A source checkout has the same pages under `man/`; read them with
`man ./man/dak.1` and `man ./man/dak-config.5`, or copy
`man/dak.1`/`man/dak-config.5` into your system's `man1`/`man5` directory.

## Device permissions

### Linux

1. Find your device and record ID.
```
# lsusb
Bus 003 Device 020: ID 0300:3002 Ajazz HOTSPOTEKUSB HID DEMO
```
2. Create a udev rule giving the user at the seat access to the keypad's `hidraw`
nodes - and nothing else. The file must be named with a `.rules` suffix - `udevd`
silently ignores any other extension (e.g. `.conf`) in `/etc/udev/rules.d/`.

```
#cat /etc/udev/rules.d/70-ajazz-akp03e.rules
KERNEL=="hidraw*", SUBSYSTEM=="hidraw", ATTRS{idVendor}=="0300", ATTRS{idProduct}=="3002", MODE="0600", TAG+="uaccess"
```
```
sudo chown root:root /etc/udev/rules.d/70-ajazz-akp03e.rules
sudo udevadm control --reload && sudo udevadm trigger --subsystem-match=hidraw
```

`TAG+="uaccess"` lets systemd-logind give the logged-in user at the seat (and only
them) access, and takes it away again at logout; the file name must sort before
`73-seat-late.rules` for that to work. dak only needs the `hidraw` nodes, so the rule
deliberately does not open up the raw USB device node (`SUBSYSTEM=="usb"`), which
would let a user talk to the keypad's firmware directly.

Without systemd-logind, use a dedicated group instead of `uaccess`, containing only
the users who should drive the keypad (not a catch-all group such as `plugdev`):

```
sudo groupadd --system dak
sudo usermod -aG dak yourusername
```
```
#cat /etc/udev/rules.d/70-ajazz-akp03e.rules
KERNEL=="hidraw*", SUBSYSTEM=="hidraw", ATTRS{idVendor}=="0300", ATTRS{idProduct}=="3002", MODE="0660", GROUP="dak"
```

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

`/dev/hidrawN` nodes default to `0600 root:operator`. Give a dedicated group access
to the keypad's nodes only, with a `devd(8)` rule that runs when a `hidraw` node of
vendor 0x0300, product 0x3002 attaches:

```
sudo pw groupadd dak
sudo pw groupmod dak -m yourusername
```
```
# /usr/local/etc/devd/dak-permissions.conf
attach 100 {
	device-name "hidraw[0-9]+";
	match "vendor" "0x0300";
	match "product" "0x3002";
	action "chgrp dak /dev/$device-name && chmod 0660 /dev/$device-name";
};
```
```
sudo service devd restart
```

Do **not** use a `devfs.rules(5)` entry such as `add path 'hidraw*' ... group operator`
for this: with `hw.usb.usbhid.enable=1` *every* HID device (keyboards included) gets a
`hidraw` node, so that rule would let the group read every keystroke typed on the
machine - and `operator` can also read raw disks, so it must never be handed out just
to use a keypad.

Then replug the device, or reset it, so it re-attaches under `/dev/hidrawN` instead of
`/dev/uhidN` (and the rule runs):

```
sudo usbconfig -d ugenX.Y reset
```
(log out and back in too, so your shell picks up the new group membership).

## Running as a service

dak can run in the background, started with your session: through a systemd user
unit, or (FreeBSD, Linux without systemd) through your desktop's autostart or X
session script. Either way it locks each keypad it drives (see "One dak
per keypad" in the README), so a second user's dak waits (`--wait`) instead of
fighting over it.

### Linux (systemd user unit)

The `.deb` installs a user unit, `/usr/lib/systemd/user/dak.service`. It runs dak
inside your graphical session (so actions see `DISPLAY`, `WAYLAND_DISPLAY` and the
session D-Bus) and stops it when you log out:

```
systemctl --user enable --now dak      # start now and with every graphical login
systemctl --user status dak            # shows e.g. "1 device connected"
systemctl --user reload dak            # re-read ~/.config/dak/config.json (SIGHUP)
systemctl --user kill -s USR1 dak      # look for keypads again (SIGUSR1)
journalctl --user -u dak -p warning    # warnings and errors only
```

To pass other flags (e.g. `-c` or `-d device`), run `systemctl --user edit dak` and
add:

```
[Service]
ExecStart=
ExecStart=/usr/bin/dak --wait -c /path/to/config.json
```

A broken config makes dak exit with status 3, which the unit does not restart; fix
it and `systemctl --user restart dak`. From a source build, copy
`debian/dak.service` to `~/.config/systemd/user/` and adjust the `ExecStart` path.

### Starting at login without systemd (FreeBSD, other Linux)

FreeBSD has no per-user service manager: rc.d starts system services at boot,
before anyone logs in, and dak must run inside your session (its actions need your
`DISPLAY`/Wayland/D-Bus). Start it with the session instead, detached:

- **Desktop autostart** (XFCE, KDE, MATE, LXQt, ... - anything following the XDG
  autostart spec): copy the shipped example into your autostart folder.

  ```
  mkdir -p ~/.config/autostart
  cp /usr/local/share/examples/dak/dak.desktop ~/.config/autostart/   # FreeBSD .pkg
  cp /usr/share/doc/dak/examples/dak.desktop ~/.config/autostart/     # Debian/Ubuntu .deb
  ```

  In a checkout it is `examples/service/dak.desktop`. It runs `dak --detach --wait`
  and is ignored by systemd-based sessions (`X-systemd-skip=true`), which should use
  the user unit above instead. Do not copy it to `/etc/xdg/autostart`: it would start
  dak for every user.

- **`startx` / X session script**: add to `~/.xinitrc` (or `~/.xsession`) before the
  window manager:

  ```
  dak --detach --wait
  ```

`--detach` returns once the keypad is connected (or a warning if none is attached
yet), and `dak` logs to syslog (`/var/log/messages`) unless its config's `logging`
section says otherwise.

**Stopping it at logout.** A detached dak runs in its own session, so logging out
does not stop it - it keeps the keypad, and the next user's `dak --wait` waits
forever. Stop it from your logout path:

```
pkill -u "$USER" -x dak
```

for example in `~/.xinitrc` after the window manager exits (start the window
manager without `exec`, then put the `pkill` line after it), in your display
manager's session cleanup hook (LightDM `session-cleanup-script`, SDDM `Xstop`), or
in `~/.logout` (csh/tcsh) / `~/.bash_logout`. dak cleans up (clears the buttons,
releases the keypad) on the `SIGTERM` `pkill` sends.

### Picking a keypad up again when it is plugged in

A dak that gave up on an unplugged keypad (`device_reconnect_max_attempts`) or
started without one keeps running as a service and waits for `SIGUSR1`. Example
hooks send that signal automatically whenever a keypad appears; they are shipped but
not active:

- Linux: `/usr/share/doc/dak/examples/99-dak-rescan.rules` (in a checkout:
  `examples/service/99-dak-rescan.rules`)

  ```
  sudo cp /usr/share/doc/dak/examples/99-dak-rescan.rules /etc/udev/rules.d/
  sudo udevadm control --reload
  ```

- FreeBSD: `/usr/local/share/examples/dak/dak-rescan.conf` (in a checkout:
  `examples/service/dak-rescan.conf`)

  ```
  sudo cp /usr/local/share/examples/dak/dak-rescan.conf /usr/local/etc/devd/
  sudo service devd restart
  ```

Both signal every running dak (`pkill -USR1 -x dak`); one with nothing to look for
ignores it.
