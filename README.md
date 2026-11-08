# DexHid

DexHid turns a tablet's touchscreen into a touchpad and a PC keyboard. It has three modes:

- **Keyboard + touchpad for an external display.** When a monitor is connected over USB-C, DexHid can grab the
  internal touchscreen and show an on-screen keyboard and a touchpad. The desktop on the monitor receives a virtual
  multitouch touchpad and a virtual keyboard through `uinput`, so the desktop's own libinput settings, xkb layout and
  key repeat apply.
- **USB keyboard + mouse for another computer.** The tablet acts as a USB HID device (a keyboard, a mouse and media /
  system keys) for the computer at the other end of the USB-C cable. It works like a KVM without video.
- **Second screen.** The tablet is a normal display again and DexHid stays out of the way.

DexHid does not depend on a distribution or a compositor. It runs as a systemd user service with a session D-Bus API
(`org.dexhid.DexHid1`) and a CLI. It draws its UI as a layer-shell overlay on the internal panel. It is tested with
KDE Plasma 6 (KWin) on Fedora 45 on the OnePlus Pad 4 (mainline Linux).

## Features

**Monitor keyboard + touchpad mode**

- Hotplug: when a monitor connects, an "ask" card offers *Use as pad*, *Keep as display*, *Always* or *Never*. The
  choice is remembered per monitor. Auto-start defaults to "ask".
- Touchpad: libinput recognises the virtual device as a real touchpad. Tap, two-finger tap, tap-and-drag, two-finger
  scrolling (with optional inertia), 3- and 4-finger swipes, independent click zones and a middle-click chord all
  work. The desktop's touchpad settings are available from the DexHid settings sheet.
- Keyboards: full PC QWERTY (with a numpad), simplified tablet layout (with a symbols page), split, laptop preset,
  numpad / calculator, text editing and presentation layouts, and a touchpad-only layout. You can move and resize
  the keyboard and the touchpad in edit mode, with handles, snap guides and minimum sizes. Arrangements are saved per
  layout and orientation.
- Keys send evdev codes, so the desktop's xkb layout applies. Labels come from libxkbcommon. Live layout tracking
  works on KDE. Also included: Shift / AltGr / Caps legends, sticky modifiers, an Fn layer, long-press alternates,
  Caps / Num LEDs, tap-on-release with a 150 ms hold and slide-off cancel, and a lone modifier that you cancel does
  not trigger the launcher.
- Whole-screen 3- and 4-finger swipes work even over the keys, and gesture shortcuts can be configured.
- Quick actions: copy, paste, cut, undo, redo, select all, find and save, window / system / media actions, and your
  own chords or commands.
- Extras, each one a setting: a clipboard strip, a text-editing select key and joystick, a presentation laser and
  talk timer, the pen as a pointer / drawing tablet on the monitor (pressure, tilt), word suggestions and an emoji
  panel, profiles per monitor or app, a mirror preview of the monitor (xdg-desktop-portal ScreenCast), themes
  (light, dark, AMOLED, follow the desktop), battery / display info, rest zones, palm and pen-proximity rejection,
  and a mouse fallback.
- Panel dimming while in pad mode (default 20 %), restored on exit. Plasma panels follow to the monitor and are
  restored afterwards.
- Safe release: the grab is dropped on unplug, Exit, toggle, a 5-finger hold, the emergency key chord (configurable,
  default Volume Up + Volume Down for 2 s), lid close, lock, sleep, loss of the UI, SIGTERM or a crash. A separate
  guardian process owns the virtual keyboard and releases any held key if the daemon dies or freezes, so no modifier
  stays stuck.

**USB keyboard + mouse mode**

- A root helper (`dexhid-hid.socket`) creates and binds a configfs USB gadget with keyboard, boot mouse and
  media / system report descriptors. It hands `/dev/hidgN` to the daemon, then releases the keys and removes the
  gadget when the daemon stops or dies.
- The gadget's serial number is a fixed text (`dexhid-0001`), never the device's own serial.
- It refuses to start while another gadget is active or while the port is in host role. The strip and the settings
  show the live power state and the USB role.
- The keyboard language for the target is selectable (28 layouts; the default is the desktop's layout).
- Power: use a USB-C PD pass-through splitter, or the target's own PD source. Hubs and docks that make the tablet a
  USB host cannot work in this mode.
- Verified with a Windows PC as host: Windows shows an HID keyboard, an HID-compliant mouse, a consumer control
  device and a system controller.

**App window and mode switch**

- `dexhid window` (also in the app menu) opens the app window. Its Home page has the 3-way mode switch (USB HID /
  monitor keyboard + touchpad / second screen) and the layout picker. All settings are in the same window
  (`dexhid settings`).
- App-menu actions: *USB keyboard + mouse*, *Keyboard + touchpad*, *Second screen*.
- CLI / D-Bus: `dexhid toggle | release | resume | status | layout | set | mode usb|pad|screen | window | settings`,
  `OpenWindow`, `OpenSettings` and `SetMode`.

## Overhead (measured on the OnePlus Pad 4, Fedora 45, KDE Plasma 6.8)

| Measurement | Value |
|---|---|
| Idle wake-ups, daemon + guardian | 0.02 per second |
| Wake-ups with the app window open | 0.00 per second |
| Wake-ups in pad mode (touchscreen grabbed) | 0.50 per second |
| Key latency in DexHid (median) | 0 ms |
| Memory (PSS) | 20.8 MB |
| Touch event to presented frame on the overlay, 120 Hz panel | p50 14.7 ms, p95 23.7 ms |

## Build and install (Fedora, RPM)

The spec is `packaging/fedora/dexhid.spec`. It builds offline from a source tarball and a `cargo vendor` tarball:

```bash
git clone https://github.com/mzakamin-hub/DexHid && cd DexHid
mkdir -p ~/rpmbuild/SOURCES
git archive --prefix=DexHid-0.1.0/ -o ~/rpmbuild/SOURCES/dexhid-0.1.0.tar.gz HEAD
cargo vendor --locked vendor >/dev/null && tar -czf ~/rpmbuild/SOURCES/dexhid-0.1.0-vendor.tar.gz vendor && rm -rf vendor
rpmbuild -ba packaging/fedora/dexhid.spec          # or: mock -r fedora-45-aarch64 --buildsrpm / --rebuild
sudo dnf install ~/rpmbuild/RPMS/*/dexhid-0.1.0-*.rpm
```

The package installs the user service `dexhid.service`, the root helper `dexhid-hid.socket` / `.service`, the udev
rule `70-dexhid.rules` (uinput and touchscreen access for the seat user), the D-Bus interface and activation files,
the app-menu entries, the layouts, the device presets and `config.example.toml`.

Devices are described by small presets in `data/devices/`, which give the touchscreen and pen names, the internal
output and the size. A preset is chosen by the device-tree compatible. The OnePlus Pad 4 preset is included.

Without RPM: `cargo build --release`, then install `target/release/dexhid` and the files from `data/` to the paths
the spec uses.

## Credits

- The behaviour numbers of the touchpad and keyboard (tap / hold timing, click zones, swipe thresholds) come from the
  "input deck" of the [Denial](https://github.com/denialwm/denial) compositor by Doctor Logix. DexHid is written
  from scratch and uses no Denial code.
- `third_party/blocking` is smol-rs's [blocking](https://github.com/smol-rs/blocking) 1.7.0 (Apache-2.0 OR MIT,
  licence files kept) with one change described in `third_party/blocking/DEXHID-PATCH.md`. The change lets idle
  pool threads exit, so the idle daemon does not wake up.

## Licence

**LICENCE NOT DECIDED YET (owner decision pending).** The source headers and the spec say `GPL-3.0-or-later`, but
that is provisional. See `LICENSE-PENDING.md`. The third-party code in `third_party/` keeps its own licence.
