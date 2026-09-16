# Installing dak

One-time device/permissions setup so `dak` can talk to the Ajazz AKP03E /
AKP03R over USB HID as a regular (non-root) user. Device list is imported
from https://github.com/4ndv/opendeck-akp03/

### Linux

1. Find your device and record ID.
```
# lsusb
Bus 003 Device 020: ID 0300:3002 Ajazz HOTSPOTEKUSB HID DEMO
```
2. Create UDEV rule to give regular user access to the device
change GROUP= to some group appropriate to your system which your user is member of.
The file must be named with a `.rules` suffix - `udevd` silently ignores any
other extension (e.g. `.conf`) in `/etc/udev/rules.d/`.

```
#cat /etc/udev/rules.d/ajazz_akp03e.rules
SUBSYSTEM=="usb", ATTR{idVendor}=="0300", ATTR{idProduct}=="3002", MODE="0660", TAG+="uaccess", GROUP="plugdev"
SUBSYSTEM=="usb", ATTRS{idVendor}=="0300", ATTRS{idProduct}=="3002", MODE="0660", TAG+="uaccess", GROUP="plugdev"
KERNEL=="hidraw*", SUBSYSTEM=="hidraw", ATTR{idVendor}=="0300", ATTR{idProduct}=="3002", MODE="0660", TAG+="uaccess", GROUP="plugdev"
KERNEL=="hidraw*", SUBSYSTEM=="hidraw", ATTRS{idVendor}=="0300", ATTRS{idProduct}=="3002", MODE="0660", TAG+="uaccess", GROUP="plugdev"
```
```
sudo chown root:root /etc/udev/rules.d/ajazz_akp03e.rules
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
