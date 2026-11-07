// SPDX-License-Identifier: GPL-3.0-or-later  (PROVISIONAL)
//! M3: HID report descriptors and report encoding for the USB HID mode (configfs gadget, /dev/hidgN) and the
//! Bluetooth HID mode (L2CAP interrupt channel). Standard descriptors only:
//! - keyboard: the boot keyboard of the HID 1.11 spec (appendix E.6), byte-identical to the example in the kernel's
//!   Documentation/usb/gadget_hid.rst (8-byte report, 5 LED bits out);
//! - mouse: the boot mouse of HID 1.11 appendix E.10, extended with buttons 4 / 5, a wheel and AC Pan (the boot
//!   protocol only reads the first three bytes, so it stays boot compatible);
//! - consumer control (16-bit usage array) and system control (power / sleep / wake), with report IDs;
//! - optional absolute pointer (0..32767, report ID) for the laser pointer / pen on the far device.
//!
//! USB: three functions (keyboard and mouse without report IDs: boot protocol; "extra" with IDs). Bluetooth: one
//! descriptor with IDs 1 keyboard and 2 mouse (the IDs the Bluetooth HID spec fixes for the boot reports), 3 consumer,
//! 4 system, 5 absolute pointer; reports go out as `0xA1 id data..`.
//! The evdev -> HID usage table follows the HID Usage Tables (keyboard page 0x07, consumer page 0x0C); keycodes come
//! from linux/input-event-codes.h. Pure logic: no device access.

use crate::keycodes;

// ---------------------------------------------------------------------------------------------- descriptor items

/// a short item: `prefix` = tag + type bits (size bits 0), data as the smallest size that holds it
fn item(out: &mut Vec<u8>, prefix: u8, data: i64, signed: bool) {
    let fits1 = if signed { (-128..=127).contains(&data) } else { (0..=255).contains(&data) };
    let fits2 = if signed { (-32768..=32767).contains(&data) } else { (0..=65535).contains(&data) };
    if fits1 {
        out.extend_from_slice(&[prefix | 1, data as u8]);
    } else if fits2 {
        let v = data as u16;
        out.extend_from_slice(&[prefix | 2, v as u8, (v >> 8) as u8]);
    } else {
        let v = data as u32;
        out.extend_from_slice(&[prefix | 3, v as u8, (v >> 8) as u8, (v >> 16) as u8, (v >> 24) as u8]);
    }
}

struct D(Vec<u8>);
impl D {
    fn page(&mut self, v: i64) -> &mut D { item(&mut self.0, 0x04, v, false); self }
    fn usage(&mut self, v: i64) -> &mut D { item(&mut self.0, 0x08, v, false); self }
    fn umin(&mut self, v: i64) -> &mut D { item(&mut self.0, 0x18, v, false); self }
    fn umax(&mut self, v: i64) -> &mut D { item(&mut self.0, 0x28, v, false); self }
    fn lmin(&mut self, v: i64) -> &mut D { item(&mut self.0, 0x14, v, true); self }
    fn lmax(&mut self, v: i64) -> &mut D { item(&mut self.0, 0x24, v, true); self }
    fn size(&mut self, v: i64) -> &mut D { item(&mut self.0, 0x74, v, false); self }
    fn count(&mut self, v: i64) -> &mut D { item(&mut self.0, 0x94, v, false); self }
    fn id(&mut self, v: Option<u8>) -> &mut D { if let Some(i) = v { item(&mut self.0, 0x84, i as i64, false); } self }
    fn input(&mut self, v: i64) -> &mut D { item(&mut self.0, 0x80, v, false); self }
    fn output(&mut self, v: i64) -> &mut D { item(&mut self.0, 0x90, v, false); self }
    fn coll(&mut self, v: i64) -> &mut D { item(&mut self.0, 0xa0, v, false); self }
    fn end(&mut self) -> &mut D { self.0.push(0xc0); self }
}

const DATA_VAR_ABS: i64 = 0x02;
const CNST: i64 = 0x01;
const CNST_VAR_ABS: i64 = 0x03;
const DATA_ARY_ABS: i64 = 0x00;
const DATA_VAR_REL: i64 = 0x06;

/// the boot keyboard (HID 1.11 E.6 / gadget_hid.rst): modifiers, reserved byte, 5 LEDs out, 6 key slots (0..101)
pub fn keyboard(report_id: Option<u8>) -> Vec<u8> {
    let mut d = D(vec![]);
    d.page(0x01).usage(0x06).coll(0x01).id(report_id);
    d.page(0x07).umin(0xe0).umax(0xe7).lmin(0).lmax(1).size(1).count(8).input(DATA_VAR_ABS);
    d.count(1).size(8).input(CNST_VAR_ABS);
    d.count(5).size(1).page(0x08).umin(1).umax(5).output(DATA_VAR_ABS);
    d.count(1).size(3).output(CNST_VAR_ABS);
    d.count(6).size(8).lmin(0).lmax(101).page(0x07).umin(0).umax(101).input(DATA_ARY_ABS);
    d.end();
    d.0
}

/// a relative mouse. `buttons` 3..=5; `wheel` adds the vertical wheel, `pan` the horizontal one (consumer AC Pan).
/// mouse(3, false, false, None) is HID 1.11 appendix E.10 byte for byte.
pub fn mouse(buttons: u8, wheel: bool, pan: bool, report_id: Option<u8>) -> Vec<u8> {
    let b = buttons.clamp(3, 8) as i64;
    let mut d = D(vec![]);
    d.page(0x01).usage(0x02).coll(0x01).id(report_id).usage(0x01).coll(0x00);
    d.page(0x09).umin(1).umax(b).lmin(0).lmax(1).count(b).size(1).input(DATA_VAR_ABS);
    if b < 8 {
        d.count(1).size(8 - b).input(CNST);
    }
    d.page(0x01).usage(0x30).usage(0x31);
    if wheel {
        d.usage(0x38);
    }
    d.lmin(-127).lmax(127).size(8).count(if wheel { 3 } else { 2 }).input(DATA_VAR_REL);
    if pan {
        d.page(0x0c).usage(0x0238).lmin(-127).lmax(127).size(8).count(1).input(DATA_VAR_REL);
    }
    d.end().end();
    d.0
}

/// consumer control: one 16-bit usage (0 = none), array
pub fn consumer(report_id: u8) -> Vec<u8> {
    let mut d = D(vec![]);
    d.page(0x0c).usage(0x01).coll(0x01).id(Some(report_id));
    d.lmin(0).lmax(0x03ff).umin(0).umax(0x03ff).size(16).count(1).input(DATA_ARY_ABS);
    d.end();
    d.0
}

/// system control: power down (0x81), sleep (0x82), wake up (0x83) as bits
pub fn system(report_id: u8) -> Vec<u8> {
    let mut d = D(vec![]);
    d.page(0x01).usage(0x80).coll(0x01).id(Some(report_id));
    d.umin(0x81).umax(0x83).lmin(0).lmax(1).size(1).count(3).input(DATA_VAR_ABS);
    d.count(1).size(5).input(CNST);
    d.end();
    d.0
}

/// absolute pointer: 3 buttons + X / Y 0..32767
pub fn abs_pointer(report_id: u8) -> Vec<u8> {
    let mut d = D(vec![]);
    d.page(0x01).usage(0x02).coll(0x01).id(Some(report_id)).usage(0x01).coll(0x00);
    d.page(0x09).umin(1).umax(3).lmin(0).lmax(1).count(3).size(1).input(DATA_VAR_ABS);
    d.count(1).size(5).input(CNST);
    d.page(0x01).usage(0x30).usage(0x31).lmin(0).lmax(32767).size(16).count(2).input(DATA_VAR_ABS);
    d.end().end();
    d.0
}

pub const RID_KBD: u8 = 1;
pub const RID_MOUSE: u8 = 2;
pub const RID_CONSUMER: u8 = 3;
pub const RID_SYSTEM: u8 = 4;
pub const RID_ABS: u8 = 5;
/// USB "extra" function report IDs (its own descriptor)
pub const USB_RID_CONSUMER: u8 = 1;
pub const USB_RID_SYSTEM: u8 = 2;
pub const USB_RID_ABS: u8 = 3;

pub const MOUSE_REPORT_LEN: usize = 5;

/// one USB gadget HID function
#[derive(Clone, Debug, PartialEq)]
pub struct UsbFunction {
    /// configfs instance name (functions/hid.<name>)
    pub name: &'static str,
    pub subclass: u8,
    pub protocol: u8,
    /// the longest report incl. its report ID
    pub report_length: u16,
    pub desc: Vec<u8>,
}

/// the USB functions: keyboard (boot, protocol 1), mouse (boot, protocol 2), extra (consumer + system [+ absolute])
pub fn usb_functions(abs: bool) -> Vec<UsbFunction> {
    let mut extra = consumer(USB_RID_CONSUMER);
    extra.extend(system(USB_RID_SYSTEM));
    if abs {
        extra.extend(abs_pointer(USB_RID_ABS));
    }
    vec![
        UsbFunction { name: "kbd", subclass: 1, protocol: 1, report_length: 8, desc: keyboard(None) },
        UsbFunction { name: "mouse", subclass: 1, protocol: 2, report_length: MOUSE_REPORT_LEN as u16, desc: mouse(5, true, true, None) },
        UsbFunction { name: "extra", subclass: 0, protocol: 0, report_length: if abs { 6 } else { 3 }, desc: extra },
    ]
}

/// the Bluetooth HID descriptor (one, with report IDs)
pub fn bt_descriptor(abs: bool) -> Vec<u8> {
    let mut v = keyboard(Some(RID_KBD));
    v.extend(mouse(5, true, true, Some(RID_MOUSE)));
    v.extend(consumer(RID_CONSUMER));
    v.extend(system(RID_SYSTEM));
    if abs {
        v.extend(abs_pointer(RID_ABS));
    }
    v
}

// ---------------------------------------------------------------------------------------------- a small parser

/// input / output report sizes in bits per report ID (0 = no ID), from a descriptor (a check of the encoders)
#[derive(Debug, Default, PartialEq)]
pub struct Parsed {
    pub input_bits: std::collections::BTreeMap<u8, u32>,
    pub output_bits: std::collections::BTreeMap<u8, u32>,
    pub collections_balanced: bool,
}

pub fn parse(desc: &[u8]) -> Result<Parsed, String> {
    let mut p = Parsed::default();
    let (mut size, mut count, mut id) = (0u32, 0u32, 0u8);
    let mut depth = 0i32;
    let mut i = 0;
    while i < desc.len() {
        let b = desc[i];
        if b == 0xfe {
            return Err("long items are not used".into());
        }
        let n = match b & 3 {
            3 => 4,
            x => x as usize,
        };
        if i + 1 + n > desc.len() {
            return Err(format!("item at {} runs past the end", i));
        }
        let mut v: u32 = 0;
        for k in 0..n {
            v |= (desc[i + 1 + k] as u32) << (8 * k);
        }
        match b & 0xfc {
            0x74 => size = v,
            0x94 => count = v,
            0x84 => id = v as u8,
            0x80 => *p.input_bits.entry(id).or_insert(0) += size * count,
            0x90 => *p.output_bits.entry(id).or_insert(0) += size * count,
            0xa0 => depth += 1,
            0xc0 => {
                depth -= 1;
                if depth < 0 {
                    return Err("end collection without a collection".into());
                }
            }
            _ => {}
        }
        i += 1 + n;
    }
    p.collections_balanced = depth == 0;
    Ok(p)
}

// ---------------------------------------------------------------------------------------------- usages

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Usage {
    /// keyboard page usage (0x04..=0x65)
    Key(u8),
    /// modifier bit 0..7 (LCtrl LShift LAlt LGui RCtrl RShift RAlt RGui)
    Mod(u8),
    /// consumer page usage
    Consumer(u16),
    /// system control bit (0 power, 1 sleep, 2 wake)
    System(u8),
}

/// evdev keycode -> HID usage (None: not expressible with these descriptors)
pub fn usage(code: u16) -> Option<Usage> {
    use Usage::*;
    let k = |u: u8| Some(Key(u));
    Some(match code {
        // letters: evdev rows QWERTY.. / ASDF.. / ZXCV..
        16 => return k(0x14), 17 => return k(0x1a), 18 => return k(0x08), 19 => return k(0x15), 20 => return k(0x17),
        21 => return k(0x1c), 22 => return k(0x18), 23 => return k(0x0c), 24 => return k(0x12), 25 => return k(0x13),
        30 => return k(0x04), 31 => return k(0x16), 32 => return k(0x07), 33 => return k(0x09), 34 => return k(0x0a),
        35 => return k(0x0b), 36 => return k(0x0d), 37 => return k(0x0e), 38 => return k(0x0f),
        44 => return k(0x1d), 45 => return k(0x1b), 46 => return k(0x06), 47 => return k(0x19), 48 => return k(0x05),
        49 => return k(0x11), 50 => return k(0x10),
        // digits 1..9, 0
        2..=10 => return k(0x1e + (code - 2) as u8),
        11 => return k(0x27),
        28 => return k(0x28),  // ENTER
        1 => return k(0x29),   // ESC
        14 => return k(0x2a),  // BACKSPACE
        15 => return k(0x2b),  // TAB
        57 => return k(0x2c),  // SPACE
        12 => return k(0x2d),  // MINUS
        13 => return k(0x2e),  // EQUAL
        26 => return k(0x2f),  // LEFTBRACE
        27 => return k(0x30),  // RIGHTBRACE
        43 => return k(0x31),  // BACKSLASH
        39 => return k(0x33),  // SEMICOLON
        40 => return k(0x34),  // APOSTROPHE
        41 => return k(0x35),  // GRAVE
        51 => return k(0x36),  // COMMA
        52 => return k(0x37),  // DOT
        53 => return k(0x38),  // SLASH
        58 => return k(0x39),  // CAPSLOCK
        59..=68 => return k(0x3a + (code - 59) as u8), // F1..F10
        87 => return k(0x44),  // F11
        88 => return k(0x45),  // F12
        99 => return k(0x46),  // SYSRQ / Print
        70 => return k(0x47),  // SCROLLLOCK
        119 => return k(0x48), // PAUSE
        110 => return k(0x49), // INSERT
        102 => return k(0x4a), // HOME
        104 => return k(0x4b), // PAGEUP
        111 => return k(0x4c), // DELETE
        107 => return k(0x4d), // END
        109 => return k(0x4e), // PAGEDOWN
        106 => return k(0x4f), // RIGHT
        105 => return k(0x50), // LEFT
        108 => return k(0x51), // DOWN
        103 => return k(0x52), // UP
        69 => return k(0x53),  // NUMLOCK
        98 => return k(0x54),  // KPSLASH
        55 => return k(0x55),  // KPASTERISK
        74 => return k(0x56),  // KPMINUS
        78 => return k(0x57),  // KPPLUS
        96 => return k(0x58),  // KPENTER
        79 => return k(0x59), 80 => return k(0x5a), 81 => return k(0x5b), // KP1..3
        75 => return k(0x5c), 76 => return k(0x5d), 77 => return k(0x5e), // KP4..6
        71 => return k(0x5f), 72 => return k(0x60), 73 => return k(0x61), // KP7..9
        82 => return k(0x62),  // KP0
        83 => return k(0x63),  // KPDOT
        86 => return k(0x64),  // 102ND (ISO key)
        127 => return k(0x65), // COMPOSE / Menu (Application)
        // modifiers
        29 => Mod(0), 42 => Mod(1), 56 => Mod(2), 125 => Mod(3), 97 => Mod(4), 54 => Mod(5), 100 => Mod(6), 126 => Mod(7),
        // consumer page
        113 => Consumer(0xe2), // MUTE
        114 => Consumer(0xea), // VOLUMEDOWN
        115 => Consumer(0xe9), // VOLUMEUP
        164 => Consumer(0xcd), // PLAYPAUSE
        163 => Consumer(0xb5), // NEXTSONG
        165 => Consumer(0xb6), // PREVIOUSSONG
        166 => Consumer(0xb7), // STOPCD
        225 => Consumer(0x6f), // BRIGHTNESSUP
        224 => Consumer(0x70), // BRIGHTNESSDOWN
        158 => Consumer(0x224), // BACK (AC Back)
        159 => Consumer(0x225), // FORWARD
        172 => Consumer(0x223), // HOMEPAGE
        217 => Consumer(0x221), // SEARCH
        140 => Consumer(0x192), // CALC
        // system control
        116 => System(0), // POWER
        142 => System(1), // SLEEP
        143 => System(2), // WAKEUP
        _ => return None,
    })
}

// ---------------------------------------------------------------------------------------------- reports

#[derive(Clone, Debug, PartialEq)]
pub enum Report {
    Keyboard([u8; 8]),
    Consumer(u16),
    System(u8),
    Mouse([u8; MOUSE_REPORT_LEN]),
    /// buttons, x, y (0..32767)
    Abs(u8, u16, u16),
}

/// the keyboard side: held keys -> reports (6-key rollover; 7+ keys = ErrorRollOver in every slot)
#[derive(Clone, Debug, Default)]
pub struct KeyState {
    pub mods: u8,
    pub keys: Vec<u8>,
    pub consumer: Vec<u16>,
    pub system: u8,
}

impl KeyState {
    pub fn kbd_report(&self) -> [u8; 8] {
        let mut r = [0u8; 8];
        r[0] = self.mods;
        if self.keys.len() > 6 {
            for x in r.iter_mut().skip(2) {
                *x = 0x01;
            }
        } else {
            for (i, k) in self.keys.iter().enumerate() {
                r[2 + i] = *k;
            }
        }
        r
    }

    /// an evdev key change -> the report(s) to send (empty: unknown key or no change)
    pub fn key(&mut self, code: u16, pressed: bool) -> Vec<Report> {
        let Some(u) = usage(code) else { return vec![] };
        match u {
            Usage::Mod(b) => {
                let m = if pressed { self.mods | 1 << b } else { self.mods & !(1 << b) };
                if m == self.mods {
                    return vec![];
                }
                self.mods = m;
                vec![Report::Keyboard(self.kbd_report())]
            }
            Usage::Key(k) => {
                let had = self.keys.contains(&k);
                if pressed == had {
                    return vec![];
                }
                if pressed {
                    self.keys.push(k);
                } else {
                    self.keys.retain(|x| *x != k);
                }
                vec![Report::Keyboard(self.kbd_report())]
            }
            Usage::Consumer(c) => {
                let had = self.consumer.contains(&c);
                if pressed == had {
                    return vec![];
                }
                if pressed {
                    self.consumer.push(c);
                } else {
                    self.consumer.retain(|x| *x != c);
                }
                vec![Report::Consumer(self.consumer.last().copied().unwrap_or(0))]
            }
            Usage::System(b) => {
                let s = if pressed { self.system | 1 << b } else { self.system & !(1 << b) };
                if s == self.system {
                    return vec![];
                }
                self.system = s;
                vec![Report::System(s)]
            }
        }
    }

    /// everything up (the reports that differ from "all released")
    pub fn release_all(&mut self) -> Vec<Report> {
        let mut v = vec![];
        if self.mods != 0 || !self.keys.is_empty() {
            self.mods = 0;
            self.keys.clear();
            v.push(Report::Keyboard([0; 8]));
        }
        if !self.consumer.is_empty() {
            self.consumer.clear();
            v.push(Report::Consumer(0));
        }
        if self.system != 0 {
            self.system = 0;
            v.push(Report::System(0));
        }
        v
    }

    pub fn any_held(&self) -> bool {
        self.mods != 0 || !self.keys.is_empty() || !self.consumer.is_empty() || self.system != 0
    }
}

/// the "everything released" reports a helper sends when the daemon is gone (USB: per function; BT: framed)
pub fn all_released(abs: bool) -> Vec<Report> {
    let mut v = vec![Report::Keyboard([0; 8]), Report::Consumer(0), Report::System(0), Report::Mouse([0; MOUSE_REPORT_LEN])];
    if abs {
        v.push(Report::Abs(0, 0, 0));
    }
    v
}

/// mouse button bits: BTN_LEFT 0x110 .. BTN_EXTRA 0x114 -> bit 0..4
pub fn button_bit(btn: u16) -> Option<u8> {
    match btn {
        0x110 => Some(0),
        0x111 => Some(1),
        0x112 => Some(2),
        0x113 => Some(3),
        0x114 => Some(4),
        _ => None,
    }
}

/// the relative mouse: motion / wheel accumulate, buttons change at once
#[derive(Clone, Debug, Default)]
pub struct MouseState {
    pub buttons: u8,
    /// hi-res wheel remainders (120 per notch)
    wheel_acc: (i32, i32),
}

impl MouseState {
    /// relative motion (px), hi-res wheel (120 = one notch, vertical / horizontal; positive = up / right) and
    /// button changes -> mouse reports, large moves split into several reports of at most +-127
    pub fn feed(&mut self, dx: i32, dy: i32, wheel_hi: (i32, i32), buttons: &[(u16, bool)]) -> Vec<Report> {
        let mut out = vec![];
        for &(b, p) in buttons {
            if let Some(bit) = button_bit(b) {
                let n = if p { self.buttons | 1 << bit } else { self.buttons & !(1 << bit) };
                if n != self.buttons {
                    self.buttons = n;
                    out.push(Report::Mouse([n, 0, 0, 0, 0]));
                }
            }
        }
        self.wheel_acc.0 += wheel_hi.0;
        self.wheel_acc.1 += wheel_hi.1;
        let wv = self.wheel_acc.0 / 120;
        let wh = self.wheel_acc.1 / 120;
        self.wheel_acc.0 -= wv * 120;
        self.wheel_acc.1 -= wh * 120;
        let (mut rx, mut ry, mut rv, mut rh) = (dx, dy, wv, wh);
        while rx != 0 || ry != 0 || rv != 0 || rh != 0 {
            let c = |v: &mut i32| {
                let s = (*v).clamp(-127, 127);
                *v -= s;
                s as i8 as u8
            };
            let r = [self.buttons, c(&mut rx), c(&mut ry), c(&mut rv), c(&mut rh)];
            out.push(Report::Mouse(r));
        }
        out
    }

    pub fn release_all(&mut self) -> Vec<Report> {
        self.wheel_acc = (0, 0);
        if self.buttons != 0 {
            self.buttons = 0;
            return vec![Report::Mouse([0; MOUSE_REPORT_LEN])];
        }
        vec![]
    }
}

/// absolute position 0..1 -> 0..32767
pub fn abs_report(buttons: u8, x: f32, y: f32) -> Report {
    let s = |v: f32| (v.clamp(0.0, 1.0) * 32767.0).round() as u16;
    Report::Abs(buttons, s(x), s(y))
}

/// USB: (function index in usb_functions(), bytes to write to its /dev/hidgN); None = not in this gadget
pub fn encode_usb(r: &Report, abs: bool) -> Option<(usize, Vec<u8>)> {
    Some(match r {
        Report::Keyboard(b) => (0, b.to_vec()),
        Report::Mouse(b) => (1, b.to_vec()),
        Report::Consumer(c) => (2, vec![USB_RID_CONSUMER, *c as u8, (*c >> 8) as u8]),
        Report::System(s) => (2, vec![USB_RID_SYSTEM, *s]),
        Report::Abs(b, x, y) if abs => (2, vec![USB_RID_ABS, *b, *x as u8, (*x >> 8) as u8, *y as u8, (*y >> 8) as u8]),
        Report::Abs(..) => return None,
    })
}

/// Bluetooth: a DATA | Input frame for the interrupt channel: 0xA1, report ID, data
pub fn encode_bt(r: &Report, abs: bool) -> Option<Vec<u8>> {
    let mut v = vec![0xa1];
    match r {
        Report::Keyboard(b) => {
            v.push(RID_KBD);
            v.extend_from_slice(b);
        }
        Report::Mouse(b) => {
            v.push(RID_MOUSE);
            v.extend_from_slice(b);
        }
        Report::Consumer(c) => v.extend_from_slice(&[RID_CONSUMER, *c as u8, (*c >> 8) as u8]),
        Report::System(s) => v.extend_from_slice(&[RID_SYSTEM, *s]),
        Report::Abs(b, x, y) if abs => v.extend_from_slice(&[RID_ABS, *b, *x as u8, (*x >> 8) as u8, *y as u8, (*y >> 8) as u8]),
        Report::Abs(..) => return None,
    }
    Some(v)
}

/// keyboard LEDs (bit 0 Num, 1 Caps, 2 Scroll) from an output report: USB = the 1-byte report of the keyboard
/// function; Bluetooth = `0xA2 0x01 leds` (DATA | Output) or `0x52 0x01 leds` (SET_REPORT | Output)
pub fn leds_usb(b: &[u8]) -> Option<(bool, bool)> {
    let v = *b.first()?;
    Some((v & 2 != 0, v & 1 != 0))
}

pub fn leds_bt(b: &[u8]) -> Option<(bool, bool)> {
    match b {
        [0xa2, RID_KBD, v, ..] | [0x52, RID_KBD, v, ..] => Some((v & 2 != 0, v & 1 != 0)),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------------- Bluetooth

/// the HID service UUID (Human Interface Device, 0x1124)
pub const BT_HID_UUID: &str = "00001124-0000-1000-8000-00805f9b34fb";
pub const PSM_CONTROL: u16 = 0x11;
pub const PSM_INTERRUPT: u16 = 0x13;

/// the SDP record (BlueZ XML) of a keyboard + pointing combo device (HID profile 1.1 attributes)
pub fn sdp_record(desc: &[u8]) -> String {
    let a = |id: u16, body: &str| format!("  <attribute id=\"0x{:04x}\">{}</attribute>\n", id, body);
    let mut s = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<record>\n");
    s += &a(0x0001, "<sequence><uuid value=\"0x1124\"/></sequence>");
    s += &a(0x0004, "<sequence><sequence><uuid value=\"0x0100\"/><uint16 value=\"0x0011\"/></sequence><sequence><uuid value=\"0x0011\"/></sequence></sequence>");
    s += &a(0x0005, "<sequence><uuid value=\"0x1002\"/></sequence>");
    s += &a(0x0006, "<sequence><uint16 value=\"0x656e\"/><uint16 value=\"0x006a\"/><uint16 value=\"0x0100\"/></sequence>");
    s += &a(0x0009, "<sequence><sequence><uuid value=\"0x1124\"/><uint16 value=\"0x0101\"/></sequence></sequence>");
    s += &a(0x000d, "<sequence><sequence><sequence><uuid value=\"0x0100\"/><uint16 value=\"0x0013\"/></sequence><sequence><uuid value=\"0x0011\"/></sequence></sequence></sequence>");
    s += &a(0x0100, "<text value=\"DexHid keyboard + mouse\"/>");
    s += &a(0x0101, "<text value=\"Tablet touchpad and keyboard\"/>");
    s += &a(0x0102, "<text value=\"DexHid\"/>");
    s += &a(0x0201, "<uint16 value=\"0x0111\"/>");
    // subclass 0xc0: keyboard + pointing device combo
    s += &a(0x0202, "<uint8 value=\"0xc0\"/>");
    s += &a(0x0203, "<uint8 value=\"0x00\"/>");
    s += &a(0x0204, "<boolean value=\"true\"/>");
    s += &a(0x0205, "<boolean value=\"true\"/>");
    s += &a(0x0206, &format!("<sequence><sequence><uint8 value=\"0x22\"/><text encoding=\"hex\" value=\"{}\"/></sequence></sequence>", hex(desc)));
    s += &a(0x0207, "<sequence><sequence><uint16 value=\"0x0409\"/><uint16 value=\"0x0100\"/></sequence></sequence>");
    s += &a(0x020b, "<uint16 value=\"0x0100\"/>");
    s += &a(0x020c, "<uint16 value=\"0x0c80\"/>");
    s += &a(0x020d, "<boolean value=\"false\"/>");
    s += &a(0x020e, "<boolean value=\"true\"/>");
    s += "</record>\n";
    s
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct CtrlReply {
    /// bytes to send back on the control channel
    pub reply: Option<Vec<u8>>,
    /// keyboard LEDs (caps, num) from a SET_REPORT
    pub leds: Option<(bool, bool)>,
    /// the host asked for a virtual cable unplug: close both channels
    pub unplug: bool,
    /// boot (0) / report (1) protocol from SET_PROTOCOL
    pub protocol: Option<u8>,
}

/// a message from the host on the HID control channel (Bluetooth HID 1.1, transaction header = type << 4 | param)
pub fn bt_control(msg: &[u8], keys: &KeyState) -> CtrlReply {
    let mut r = CtrlReply::default();
    let Some(&h) = msg.first() else { return r };
    const OK: u8 = 0x00;
    const ERR_UNSUPPORTED: u8 = 0x03;
    match h >> 4 {
        // HID_CONTROL: 0x05 = VIRTUAL_CABLE_UNPLUG
        0x1 => r.unplug = h & 0x0f == 0x05,
        // GET_REPORT: answer the keyboard input report (others: unsupported)
        0x4 => {
            let id = msg.get(1).copied().unwrap_or(RID_KBD);
            r.reply = Some(if h & 0x03 == 0x01 && id == RID_KBD {
                let mut v = vec![0xa1, RID_KBD];
                v.extend_from_slice(&keys.kbd_report());
                v
            } else {
                vec![ERR_UNSUPPORTED]
            });
        }
        // SET_REPORT (output: LEDs)
        0x5 => {
            if h & 0x03 == 0x02 {
                if let (Some(&id), Some(&v)) = (msg.get(1), msg.get(2)) {
                    if id == RID_KBD {
                        r.leds = Some((v & 2 != 0, v & 1 != 0));
                    }
                }
            }
            r.reply = Some(vec![OK]);
        }
        // GET_PROTOCOL
        0x6 => r.reply = Some(vec![0xa0, 0x01]),
        // SET_PROTOCOL
        0x7 => {
            r.protocol = Some(h & 0x01);
            r.reply = Some(vec![OK]);
        }
        // GET_IDLE / SET_IDLE (deprecated, accepted)
        0x8 => r.reply = Some(vec![0xa0, 0x00]),
        0x9 => r.reply = Some(vec![OK]),
        // DATA on the control channel (output report)
        0xa => {
            r.leds = leds_bt(msg);
        }
        _ => r.reply = Some(vec![ERR_UNSUPPORTED]),
    }
    r
}

/// evdev keys DexHid layouts can send that have no HID usage here (for the notes / legends)
pub fn unsupported(codes: &[u16]) -> Vec<String> {
    codes.iter().filter(|c| usage(**c).is_none()).map(|c| keycodes::name(*c)).collect()
}

/// lowercase hex of a descriptor (logs; never with separators)
pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{:02x}", x)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Documentation/usb/gadget_hid.rst (kernel tree wf/q91i/wt), `my_hid_data.report_desc`, 63 bytes
    const REF_KEYBOARD: [u8; 63] = [
        0x05, 0x01, 0x09, 0x06, 0xa1, 0x01, 0x05, 0x07, 0x19, 0xe0, 0x29, 0xe7, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0x95, 0x01,
        0x75, 0x08, 0x81, 0x03, 0x95, 0x05, 0x75, 0x01, 0x05, 0x08, 0x19, 0x01, 0x29, 0x05, 0x91, 0x02, 0x95, 0x01, 0x75, 0x03, 0x91, 0x03, 0x95, 0x06,
        0x75, 0x08, 0x15, 0x00, 0x25, 0x65, 0x05, 0x07, 0x19, 0x00, 0x29, 0x65, 0x81, 0x00, 0xc0,
    ];
    /// HID 1.11 appendix E.10 (boot mouse), 50 bytes
    const REF_MOUSE: [u8; 50] = [
        0x05, 0x01, 0x09, 0x02, 0xa1, 0x01, 0x09, 0x01, 0xa1, 0x00, 0x05, 0x09, 0x19, 0x01, 0x29, 0x03, 0x15, 0x00, 0x25, 0x01, 0x95, 0x03, 0x75, 0x01,
        0x81, 0x02, 0x95, 0x01, 0x75, 0x05, 0x81, 0x01, 0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x15, 0x81, 0x25, 0x7f, 0x75, 0x08, 0x95, 0x02, 0x81, 0x06,
        0xc0, 0xc0,
    ];

    #[test]
    fn descriptors_byte_exact_vs_references() {
        assert_eq!(keyboard(None), REF_KEYBOARD.to_vec(), "boot keyboard = gadget_hid.rst");
        assert_eq!(mouse(3, false, false, None), REF_MOUSE.to_vec(), "boot mouse = HID 1.11 E.10");
        // a report ID goes right after the application collection
        let k = keyboard(Some(1));
        assert_eq!(&k[..6], &REF_KEYBOARD[..6]);
        assert_eq!(&k[6..8], &[0x85, 0x01]);
        assert_eq!(&k[8..], &REF_KEYBOARD[6..]);
        // 2-byte items where 1 byte would be read as negative
        let c = consumer(1);
        assert!(c.windows(3).any(|w| w == [0x26, 0xff, 0x03]), "logical max 0x3ff as 2 bytes");
        let a = abs_pointer(5);
        assert!(a.windows(3).any(|w| w == [0x26, 0xff, 0x7f]), "logical max 32767");
        assert!(mouse(5, true, true, None).windows(3).any(|w| w == [0x0a, 0x38, 0x02]), "AC Pan usage 0x0238 as 2 bytes");
    }

    #[test]
    fn parsed_sizes_match_the_encoders() {
        let k = parse(&keyboard(None)).unwrap();
        assert!(k.collections_balanced);
        assert_eq!(k.input_bits[&0], 64);
        assert_eq!(k.output_bits[&0], 8);
        let m = parse(&mouse(5, true, true, None)).unwrap();
        assert_eq!(m.input_bits[&0] as usize, MOUSE_REPORT_LEN * 8);
        for f in usb_functions(true) {
            let p = parse(&f.desc).unwrap();
            assert!(p.collections_balanced, "{}", f.name);
            let longest = p.input_bits.iter().map(|(id, b)| b / 8 + if *id != 0 { 1 } else { 0 }).max().unwrap();
            assert_eq!(longest as u16, f.report_length, "{}: report_length", f.name);
        }
        let ex = parse(&usb_functions(true)[2].desc).unwrap();
        assert_eq!(ex.input_bits[&USB_RID_CONSUMER], 16);
        assert_eq!(ex.input_bits[&USB_RID_SYSTEM], 8);
        assert_eq!(ex.input_bits[&USB_RID_ABS], 40);
        // every encoded report has the length its descriptor declares
        let bt = parse(&bt_descriptor(true)).unwrap();
        assert!(bt.collections_balanced);
        for r in all_released(true) {
            let f = encode_bt(&r, true).unwrap();
            assert_eq!(f[0], 0xa1);
            assert_eq!((f.len() - 2) as u32 * 8, bt.input_bits[&f[1]], "{:?}", r);
            let (fi, b) = encode_usb(&r, true).unwrap();
            let p = parse(&usb_functions(true)[fi].desc).unwrap();
            let bits = if fi == 2 { p.input_bits[&b[0]] + 8 } else { p.input_bits[&0] };
            assert_eq!(b.len() as u32 * 8, bits, "{:?}", r);
        }
        assert_eq!(bt.output_bits[&RID_KBD], 8);
        assert!(encode_usb(&Report::Abs(0, 0, 0), false).is_none());
    }

    #[test]
    fn keyboard_reports_keys_modifiers_rollover() {
        let mut s = KeyState::default();
        assert_eq!(s.key(42, true), vec![Report::Keyboard([0x02, 0, 0, 0, 0, 0, 0, 0])], "LShift");
        assert_eq!(s.key(30, true), vec![Report::Keyboard([0x02, 0, 0x04, 0, 0, 0, 0, 0])], "Shift+A");
        assert!(s.key(30, true).is_empty(), "repeat press: no report");
        assert_eq!(s.key(100, true), vec![Report::Keyboard([0x42, 0, 0x04, 0, 0, 0, 0, 0])], "AltGr bit 6");
        assert_eq!(s.key(30, false), vec![Report::Keyboard([0x42, 0, 0, 0, 0, 0, 0, 0])]);
        s.key(100, false);
        s.key(42, false);
        assert!(!s.any_held());
        for c in [16u16, 17, 18, 19, 20, 21] {
            s.key(c, true);
        }
        assert_eq!(s.kbd_report(), [0, 0, 0x14, 0x1a, 0x08, 0x15, 0x17, 0x1c], "Q W E R T Y");
        assert_eq!(s.key(22, true), vec![Report::Keyboard([0, 0, 1, 1, 1, 1, 1, 1])], "7th key: ErrorRollOver");
        assert_eq!(s.key(16, false), vec![Report::Keyboard([0, 0, 0x1a, 0x08, 0x15, 0x17, 0x1c, 0x18])], "back to 6");
        assert_eq!(s.release_all(), vec![Report::Keyboard([0; 8])]);
        assert_eq!(s.key(115, true), vec![Report::Consumer(0xe9)], "Vol+");
        assert_eq!(s.key(115, false), vec![Report::Consumer(0)]);
        assert_eq!(s.key(142, true), vec![Report::System(2)], "sleep bit 1");
        assert_eq!(s.release_all(), vec![Report::System(0)]);
        // spot checks of the usage table
        for (code, u) in [(2u16, 0x1e), (11, 0x27), (28, 0x28), (57, 0x2c), (59, 0x3a), (88, 0x45), (103, 0x52), (82, 0x62), (86, 0x64)] {
            assert_eq!(usage(code), Some(Usage::Key(u)), "code {}", code);
        }
        assert_eq!(usage(240), None, "KEY_UNKNOWN (the modifier mask) is not sent");
        assert_eq!(unsupported(&[183, 30]), vec![keycodes::name(183)], "F13 is outside the boot keyboard range");
        // every letter maps to a distinct usage 0x04..0x1d
        let mut seen: Vec<u8> = (b'A'..=b'Z').map(|c| match usage(keycodes::code(&(c as char).to_string()).unwrap()) { Some(Usage::Key(u)) => u, _ => 0 }).collect();
        seen.sort();
        assert_eq!(seen, (0x04u8..=0x1d).collect::<Vec<_>>());
    }

    #[test]
    fn mouse_reports_split_and_encode() {
        let mut m = MouseState::default();
        assert_eq!(m.feed(3, -2, (0, 0), &[]), vec![Report::Mouse([0, 3, 0xfe, 0, 0])]);
        assert_eq!(m.feed(300, 0, (0, 0), &[]), vec![Report::Mouse([0, 127, 0, 0, 0]), Report::Mouse([0, 127, 0, 0, 0]), Report::Mouse([0, 46, 0, 0, 0])]);
        assert_eq!(m.feed(0, 0, (60, 0), &[]), vec![], "half a notch waits");
        assert_eq!(m.feed(0, 0, (60, -120), &[]), vec![Report::Mouse([0, 0, 0, 1, 0xff])]);
        assert_eq!(m.feed(0, 0, (0, 0), &[(0x110, true)]), vec![Report::Mouse([1, 0, 0, 0, 0])]);
        assert_eq!(m.feed(1, 1, (0, 0), &[(0x111, true)]), vec![Report::Mouse([3, 0, 0, 0, 0]), Report::Mouse([3, 1, 1, 0, 0])]);
        assert_eq!(m.release_all(), vec![Report::Mouse([0; 5])]);
        assert_eq!(abs_report(1, 0.5, 1.2), Report::Abs(1, 16384, 32767));
        assert_eq!(encode_bt(&Report::Keyboard([2, 0, 4, 0, 0, 0, 0, 0]), false).unwrap(), vec![0xa1, 1, 2, 0, 4, 0, 0, 0, 0, 0]);
        assert_eq!(encode_usb(&Report::Consumer(0xe9), false).unwrap(), (2, vec![1, 0xe9, 0]));
        assert_eq!(leds_usb(&[0x03]), Some((true, true)));
        assert_eq!(leds_usb(&[0x02]), Some((true, false)));
        assert_eq!(leds_bt(&[0xa2, 1, 0x01]), Some((false, true)));
        assert_eq!(leds_bt(&[0x52, 1, 0x02]), Some((true, false)));
        assert_eq!(leds_bt(&[0xa2, 2, 0x02]), None);
        assert_eq!(hex(&[0x05, 0x01]), "0501");
    }

    #[test]
    fn bluetooth_sdp_and_control() {
        let d = bt_descriptor(false);
        let x = sdp_record(&d);
        assert!(x.contains(&format!("value=\"{}\"", hex(&d))), "the descriptor as hex");
        assert!(x.contains("<uint16 value=\"0x0011\"/>") && x.contains("<uint16 value=\"0x0013\"/>"), "PSMs 17 / 19");
        assert_eq!(x.matches("<attribute ").count(), x.matches("</attribute>").count());
        assert_eq!(x.matches("<sequence>").count(), x.matches("</sequence>").count());
        assert!(!x.contains(':') || !x.chars().any(|c| c == ':'), "no colon-separated hex anywhere (MAC guard)");
        let mut k = KeyState::default();
        k.key(30, true);
        assert_eq!(bt_control(&[0x71], &k), CtrlReply { reply: Some(vec![0]), protocol: Some(1), ..Default::default() }, "SET_PROTOCOL report");
        assert_eq!(bt_control(&[0x52, 0x01, 0x02], &k).leds, Some((true, false)), "SET_REPORT output: Caps");
        assert_eq!(bt_control(&[0x41, 0x01], &k).reply, Some(vec![0xa1, 1, 0, 0, 4, 0, 0, 0, 0, 0]), "GET_REPORT input keyboard");
        assert_eq!(bt_control(&[0x43, 0x01], &k).reply, Some(vec![3]), "feature reports unsupported");
        assert!(bt_control(&[0x15], &k).unplug);
        assert_eq!(bt_control(&[0xa2, 0x01, 0x01], &k).leds, Some((false, true)));
        assert_eq!(bt_control(&[], &k), CtrlReply::default());
    }
}
