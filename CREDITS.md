# Credits

## Denial, by Doctor Logix

DexHid exists because of [Denial](https://github.com/denialwm/denial) ([DenialWM](https://github.com/denialwm)),
the Wayland desktop by **Doctor Logix**. Its *input deck*, which turns a tablet's screen into a keyboard and touchpad
for an external display, gave us the idea for DexHid.

DexHid is written from scratch in Rust and contains **no Denial code**. What it does take from Denial is behaviour:
the timings and rules described in Denial's `INPUT_DECK.md`, reimplemented in DexHid's own code. Each place is marked
in a source header and in the commit trailers (`Based-on:`).

| DexHid part | Taken from Denial's input deck | Where in DexHid |
|---|---|---|
| On-screen key timing | A tap commits on release; a stationary hold (150 ms) presses the key so the desktop's own key repeat runs; modifiers press at once; sliding off a key cancels it | `crates/dexhid-core/src/keys.rs` (`hold_ms = 150`) |
| Touchpad click zones | Independent left and right click zones at the bottom of the touchpad | `crates/dexhid-core/src/config.rs` (`click_zones`) |
| Whole-screen 3- and 4-finger swipes | Swipes work anywhere on the screen, even over the keys, and are recognised only when every finger moves the same way | `crates/dexhid-core/src/swipe.rs` |
| Mouse fallback mode | Pointer gain 1.5x; a tap is at most 250 ms with little movement; two-finger tap = right click; double-tap-and-hold drags; two-finger scrolling | `crates/dexhid-core/src/mouse.rs` |
| The overall idea | A tablet that is the keyboard and touchpad of the monitor it drives | the whole project |

Denial is licensed GPL-3.0-or-later, the same licence as DexHid.

Commits that carry the `Based-on:` credit: `58e4f9c` (M1, the first DexHid daemon) and `301c661` (M2.1: key slop and
stationary hold, whole-screen swipes, click zones). `e49004f` (M2 core) also builds on the key timing and names Denial
in its message.

## Third-party code

- **`third_party/blocking`** is [smol-rs/blocking](https://github.com/smol-rs/blocking) 1.7.0 (Apache-2.0 OR MIT;
  its `LICENSE-APACHE` and `LICENSE-MIT` are kept). It carries one change, described in
  `third_party/blocking/DEXHID-PATCH.md`.
- The other Rust crates come from crates.io under their own licences (see the README's
  [Dependencies](README.md#dependencies)); the RPM build lists them in `LICENSE.dependencies`.

## Authors

DexHid is written by mzak, with Claude (Anthropic) as co-author (see the `Co-Authored-By:` trailers).
