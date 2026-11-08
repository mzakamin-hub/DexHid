// SPDX-License-Identifier: GPL-3.0-or-later  (PROVISIONAL)
//! The UI thread: one Wayland connection for the daemon's life. Shows one surface at a time on the internal output:
//! - overlay: full-screen layer-shell overlay (exclusive zone -1, no keyboard focus) drawing the pad-mode model;
//!   input comes from the grabbed evdev device, not from Wayland;
//! - prompt: the "ask" card (ungrabbed; its buttons take wl_touch / wl_pointer, the rest of the screen stays usable
//!   through the input region);
//! - pill: a small "Use as pad" button at the top edge while the monitor is used as a plain display.
//! Fallback without zwlr_layer_shell_v1 (GNOME): xdg_toplevel fullscreen (takes focus; M3 adds the extension).
//! Also reports the outputs (for the "monitor really enabled" check), the geometry and the xkb legends.

use crate::render::{self, Canvas, Fonts};
use dexhid_core::feedback::{self, Palette};
use crate::shared::{Cmd, SharedRef};
use crate::sys::*;
use dexhid_core::geom::{Geometry, Rect, Transform};
use dexhid_core::keycodes;
use dexhid_core::layout::{Action, KeyKind, PlacedKey};
use dexhid_core::router::{Btn, UiModel};
use dexhid_core::state::{Choice, UiKind};
use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::Ordering;
use wayland_client::protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_keyboard, wl_output, wl_pointer, wl_region, wl_registry, wl_seat, wl_shm,
    wl_shm_pool, wl_surface, wl_touch,
};
use wayland_client::{delegate_noop, Connection, Dispatch, QueueHandle, WEnum};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

#[derive(Default)]
struct OutInfo {
    reg: u32,
    name: String,
    mode: (i32, i32),
    scale: i32,
    transform: u32,
    proxy: Option<wl_output::WlOutput>,
}

#[derive(Debug, Clone, Copy)]
enum In {
    Down(i32, f64, f64),
    Motion(i32, f64, f64),
    Up(i32),
}

struct St {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    layer: Option<zwlr_layer_shell_v1::ZwlrLayerShellV1>,
    wm: Option<xdg_wm_base::XdgWmBase>,
    seat: Option<wl_seat::WlSeat>,
    touch: Option<wl_touch::WlTouch>,
    pointer: Option<wl_pointer::WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    outputs: Vec<OutInfo>,
    outputs_changed: bool,
    configured: Option<(u32, u32)>,
    xdg_size: (i32, i32),
    closed: bool,
    frame_pending: bool,
    frame_done: bool,
    released: Vec<(u64, usize)>,
    keymap: Option<String>,
    input: Vec<In>,
    ptr_pos: (f64, f64),
}

impl Dispatch<wl_registry::WlRegistry, ()> for St {
    fn event(st: &mut Self, reg: &wl_registry::WlRegistry, ev: wl_registry::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        match ev {
            wl_registry::Event::Global { name, interface, version } => match interface.as_str() {
                "wl_compositor" => st.compositor = Some(reg.bind(name, version.min(4), qh, ())),
                "wl_shm" => st.shm = Some(reg.bind(name, 1, qh, ())),
                "zwlr_layer_shell_v1" => st.layer = Some(reg.bind(name, version.min(4), qh, ())),
                "xdg_wm_base" => st.wm = Some(reg.bind(name, version.min(2), qh, ())),
                "wl_seat" if st.seat.is_none() => st.seat = Some(reg.bind(name, version.min(7), qh, ())),
                "wl_output" => {
                    let o: wl_output::WlOutput = reg.bind(name, version.min(4), qh, name);
                    st.outputs.push(OutInfo { reg: name, scale: 1, proxy: Some(o), ..Default::default() });
                    st.outputs_changed = true;
                }
                _ => {}
            },
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(i) = st.outputs.iter().position(|o| o.reg == name) {
                    st.outputs.remove(i);
                    st.outputs_changed = true;
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, u32> for St {
    fn event(st: &mut Self, _: &wl_output::WlOutput, ev: wl_output::Event, reg: &u32, _: &Connection, _: &QueueHandle<Self>) {
        let Some(o) = st.outputs.iter_mut().find(|o| o.reg == *reg) else { return };
        match ev {
            wl_output::Event::Name { name } => {
                o.name = name;
                st.outputs_changed = true;
            }
            wl_output::Event::Geometry { transform, .. } => {
                if let WEnum::Value(t) = transform {
                    o.transform = t as u32;
                    st.outputs_changed = true;
                }
            }
            wl_output::Event::Mode { flags: WEnum::Value(f), width, height, .. } if f.contains(wl_output::Mode::Current) => {
                o.mode = (width, height);
                st.outputs_changed = true;
            }
            wl_output::Event::Scale { factor } => o.scale = factor.max(1),
            _ => {}
        }
    }
}

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ()> for St {
    fn event(st: &mut Self, ls: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ev: zwlr_layer_surface_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match ev {
            zwlr_layer_surface_v1::Event::Configure { serial, width, height } => {
                ls.ack_configure(serial);
                st.configured = Some((width, height));
            }
            zwlr_layer_surface_v1::Event::Closed => st.closed = true,
            _ => {}
        }
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for St {
    fn event(_: &mut Self, wm: &xdg_wm_base::XdgWmBase, ev: xdg_wm_base::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_wm_base::Event::Ping { serial } = ev {
            wm.pong(serial);
        }
    }
}
impl Dispatch<xdg_surface::XdgSurface, ()> for St {
    fn event(st: &mut Self, xs: &xdg_surface::XdgSurface, ev: xdg_surface::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_surface::Event::Configure { serial } = ev {
            xs.ack_configure(serial);
            st.configured = Some((st.xdg_size.0.max(0) as u32, st.xdg_size.1.max(0) as u32));
        }
    }
}
impl Dispatch<xdg_toplevel::XdgToplevel, ()> for St {
    fn event(st: &mut Self, _: &xdg_toplevel::XdgToplevel, ev: xdg_toplevel::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match ev {
            xdg_toplevel::Event::Configure { width, height, .. } => st.xdg_size = (width, height),
            xdg_toplevel::Event::Close => st.closed = true,
            _ => {}
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, (u64, usize)> for St {
    fn event(st: &mut Self, _: &wl_buffer::WlBuffer, ev: wl_buffer::Event, d: &(u64, usize), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_buffer::Event::Release = ev {
            st.released.push(*d);
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for St {
    fn event(st: &mut Self, _: &wl_callback::WlCallback, ev: wl_callback::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_callback::Event::Done { .. } = ev {
            st.frame_pending = false;
            st.frame_done = true;
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for St {
    fn event(st: &mut Self, seat: &wl_seat::WlSeat, ev: wl_seat::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_seat::Event::Capabilities { capabilities: WEnum::Value(c) } = ev {
            if c.contains(wl_seat::Capability::Touch) && st.touch.is_none() {
                st.touch = Some(seat.get_touch(qh, ()));
            }
            if c.contains(wl_seat::Capability::Pointer) && st.pointer.is_none() {
                st.pointer = Some(seat.get_pointer(qh, ()));
            }
            if c.contains(wl_seat::Capability::Keyboard) && st.keyboard.is_none() {
                st.keyboard = Some(seat.get_keyboard(qh, ()));
            }
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for St {
    fn event(st: &mut Self, _: &wl_keyboard::WlKeyboard, ev: wl_keyboard::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_keyboard::Event::Keymap { format: WEnum::Value(wl_keyboard::KeymapFormat::XkbV1), fd, size } = ev {
            let size = size as usize;
            unsafe {
                let p = libc::mmap(std::ptr::null_mut(), size, libc::PROT_READ, libc::MAP_PRIVATE, fd.as_raw_fd(), 0);
                if p != libc::MAP_FAILED {
                    let b = std::slice::from_raw_parts(p as *const u8, size);
                    let n = b.iter().position(|&c| c == 0).unwrap_or(size);
                    st.keymap = Some(String::from_utf8_lossy(&b[..n]).into_owned());
                    libc::munmap(p, size);
                }
            }
        }
    }
}

impl Dispatch<wl_touch::WlTouch, ()> for St {
    fn event(st: &mut Self, _: &wl_touch::WlTouch, ev: wl_touch::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match ev {
            wl_touch::Event::Down { id, x, y, .. } => st.input.push(In::Down(id, x, y)),
            wl_touch::Event::Motion { id, x, y, .. } => st.input.push(In::Motion(id, x, y)),
            wl_touch::Event::Up { id, .. } => st.input.push(In::Up(id)),
            wl_touch::Event::Cancel => st.input.push(In::Up(-100)),
            _ => {}
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for St {
    fn event(st: &mut Self, _: &wl_pointer::WlPointer, ev: wl_pointer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match ev {
            wl_pointer::Event::Enter { surface_x, surface_y, .. } | wl_pointer::Event::Motion { surface_x, surface_y, .. } => {
                st.ptr_pos = (surface_x, surface_y);
                st.input.push(In::Motion(-1, surface_x, surface_y));
            }
            wl_pointer::Event::Button { state: WEnum::Value(s), .. } => {
                if s == wl_pointer::ButtonState::Pressed {
                    st.input.push(In::Down(-1, st.ptr_pos.0, st.ptr_pos.1));
                } else {
                    st.input.push(In::Up(-1));
                }
            }
            _ => {}
        }
    }
}

delegate_noop!(St: ignore wl_compositor::WlCompositor);
delegate_noop!(St: ignore wl_shm::WlShm);
delegate_noop!(St: ignore wl_shm_pool::WlShmPool);
delegate_noop!(St: ignore wl_surface::WlSurface);
delegate_noop!(St: ignore wl_region::WlRegion);
delegate_noop!(St: ignore zwlr_layer_shell_v1::ZwlrLayerShellV1);

// ------------------------------------------------------------------ buffers

struct Buf {
    wl: wl_buffer::WlBuffer,
    off: usize,
    busy: bool,
    key: (u64, u64),
    dyn_rects: Vec<(i32, i32, i32, i32)>,
}

struct Pool {
    _fd: OwnedFd,
    map: *mut u32,
    len: usize,
    pool: wl_shm_pool::WlShmPool,
    bufs: Vec<Buf>,
    bw: i32,
    bh: i32,
    gen: u64,
}

impl Pool {
    fn new(shm: &wl_shm::WlShm, bw: i32, bh: i32, argb: bool, gen: u64, qh: &QueueHandle<St>) -> Option<Pool> {
        let n = 2usize;
        let bsize = (bw * bh * 4) as usize;
        let len = bsize * n;
        let fd = unsafe { libc::memfd_create(c"dexhid".as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            return None;
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        if unsafe { libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) } != 0 {
            return None;
        }
        let map = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd.as_raw_fd(), 0) };
        if map == libc::MAP_FAILED {
            return None;
        }
        let pool = shm.create_pool(fd.as_fd(), len as i32, qh, ());
        let fmt = if argb { wl_shm::Format::Argb8888 } else { wl_shm::Format::Xrgb8888 };
        let bufs = (0..n)
            .map(|i| Buf { wl: pool.create_buffer((i * bsize) as i32, bw, bh, bw * 4, fmt, qh, (gen, i)), off: i * bsize / 4, busy: false, key: (0, 0), dyn_rects: vec![] })
            .collect();
        Some(Pool { _fd: fd, map: map as *mut u32, len, pool, bufs, bw, bh, gen })
    }
    fn px(&mut self, i: usize) -> &mut [u32] {
        let n = (self.bw * self.bh) as usize;
        unsafe { std::slice::from_raw_parts_mut(self.map.add(self.bufs[i].off), n) }
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        for b in &self.bufs {
            b.wl.destroy();
        }
        self.pool.destroy();
        unsafe { libc::munmap(self.map as *mut libc::c_void, self.len) };
    }
}

struct Surf {
    kind: UiKind,
    /// M2.1 fix: the daemon's surface epoch (bumps on every kind change, so overlay -> pill -> overlay while this
    /// thread was busy still makes a fresh surface and a fresh UI-MAPPED)
    epoch: u64,
    surface: wl_surface::WlSurface,
    layer: Option<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1>,
    xdg: Option<(xdg_surface::XdgSurface, xdg_toplevel::XdgToplevel)>,
    w: i32,
    h: i32,
    scale: i32,
    pool: Option<Pool>,
    mapped: bool,
    /// prompt / pill buttons (logical px) for hit tests
    hits: Vec<(Rect, Hit)>,
    pressed: HashMap<i32, Hit>,
    pos: HashMap<i32, (f64, f64)>,
    lit: Option<Hit>,
    last_dyn: Vec<(i32, i32, i32, i32)>,
}

impl Drop for Surf {
    fn drop(&mut self) {
        self.pool = None;
        if let Some(l) = self.layer.take() {
            l.destroy();
        }
        if let Some((xs, tl)) = self.xdg.take() {
            tl.destroy();
            xs.destroy();
        }
        self.surface.destroy();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Hit {
    Choice(Choice),
    PillPad,
    PillHide,
}

impl Hit {
    fn name(&self) -> &'static str {
        match self {
            Hit::Choice(Choice::UsePad) => "prompt.usepad",
            Hit::Choice(Choice::KeepDisplay) => "prompt.display",
            Hit::Choice(Choice::Always) => "prompt.always",
            Hit::Choice(Choice::Never) => "prompt.never",
            Hit::PillPad => "pill.usepad",
            Hit::PillHide => "pill.hide",
        }
    }
}

// ------------------------------------------------------------------ drawing

/// legend of one evdev code for a modifier state (M2.1: Shift / AltGr / Caps from the xkb levels; xkb symbol keys
/// follow the desktop layout, the rest the US table)
fn code_label_m(c: u16, shift: bool, altgr: bool, caps: bool, labels: Option<&crate::xkb::Labels>) -> String {
    if keycodes::is_symbol_key(c) {
        if let Some(l) = labels.and_then(|l| l.map.get(&c)) {
            return crate::xkb::pick(l, shift, altgr, caps);
        }
    }
    let (a, b) = keycodes::us_legend(c);
    let letter = matches!(c, 16..=25 | 30..=38 | 44..=50);
    let sh = if letter { shift ^ caps } else { shift };
    if sh && !b.is_empty() {
        b.to_string()
    } else {
        a.to_string()
    }
}

pub(crate) fn code_label(c: u16, shift: bool, labels: Option<&crate::xkb::Labels>) -> String {
    code_label_m(c, shift, false, false, labels)
}

pub(crate) fn key_label(k: &PlacedKey, m: &UiModel, labels: Option<&crate::xkb::Labels>) -> String {
    let caps = m.leds.0;
    let shift = m.shift;
    if shift {
        if let Some(s) = &k.shift_label {
            return s.clone();
        }
    }
    if let Some(l) = &k.label {
        return l.clone();
    }
    match &k.kind {
        KeyKind::Code(c) => {
            let c2 = if m.fn_on { keycodes::fn_map(*c) } else { *c };
            code_label_m(c2, shift, m.altgr, caps, labels)
        }
        KeyKind::Combo(v) => {
            // Shift+key: the shifted legend of the key in the desktop layout; other chords: "Ctrl+C" style
            let base = *v.last().unwrap_or(&0);
            if v.len() == 2 && keycodes::is_shift(v[0]) {
                return code_label(base, true, labels);
            }
            v.iter().map(|&c| if keycodes::is_modifier(c) { code_label(c, false, None) } else { code_label(c, false, labels).to_uppercase() }).collect::<Vec<_>>().join("+")
        }
        KeyKind::Action(Action::Page(p)) => p.clone(),
        KeyKind::Action(Action::LayoutNext) => "Layout".into(),
        KeyKind::Action(Action::KeyboardHide) => "Hide".into(),
        KeyKind::Action(Action::Fn) => "Fn".into(),
        KeyKind::Action(Action::SelectMode) => "Select".into(),
        KeyKind::Action(Action::Joystick) => "\u{2725} stick".into(),
        KeyKind::Action(Action::EmojiPanel) => "Emoji".into(),
        KeyKind::Action(Action::Timer(t)) => format!("Timer {}", t),
        KeyKind::Action(Action::Laser) => "Laser".into(),
        KeyKind::Gap => String::new(),
    }
}

fn pal(m: &UiModel) -> Palette {
    let (t, a, k) = &m.theme;
    if t == "desktop" {
        if let Some(d) = &m.desk {
            return feedback::palette_desk(d, if *k == 0 { 100 } else { *k });
        }
    }
    feedback::palette(if t.is_empty() || t == "desktop" { "dark" } else { t }, a, if *k == 0 { 100 } else { *k })
}

/// corner radius scale: the desktop's corners for theme "desktop" (Breeze-like 1.3 mm otherwise)
fn corner(m: &UiModel) -> f32 {
    match (&m.desk, m.theme.0.as_str()) {
        (Some(d), "desktop") => (d.radius_mm / 1.3).clamp(0.0, 3.0),
        _ => 1.0,
    }
}

/// M2.1: the legend state dump for the runner (`LEGENDS ...`, with --debug): what a few keys show now
pub fn legends_line(m: &UiModel, labels: Option<&crate::xkb::Labels>) -> String {
    let show = |name: &str| -> String {
        m.keys.iter().find(|k| matches!(k.kind, KeyKind::Code(c) if keycodes::name(c) == name)).map(|k| key_label(k, m, labels)).unwrap_or_else(|| "-".into())
    };
    format!(
        "LEGENDS shift={} altgr={} caps={} fn={} group={} Q={} A={} 2={} E={} MINUS={}",
        m.shift,
        m.altgr,
        m.leds.0,
        m.fn_on,
        labels.map(|l| l.group).unwrap_or(0),
        show("KEY_Q"),
        show("KEY_A"),
        show("KEY_2"),
        show("KEY_E"),
        show("KEY_MINUS")
    )
}

fn draw_key(cv: &mut Canvas, f: &mut Fonts, m: &UiModel, p: &Palette, k: &PlacedKey, lit: bool, labels: Option<&crate::xkb::Labels>) {
    let sym = matches!(k.kind, KeyKind::Code(c) if keycodes::is_symbol_key(c) || c == 57) || matches!(k.kind, KeyKind::Combo(_));
    // latched sticky modifiers, Caps / Num Lock on, Fn layer: drawn in the accent colour
    let on = match &k.kind {
        KeyKind::Code(c) => m.latched.iter().any(|(l, _)| l == c) || (*c == 58 && m.leds.0) || (*c == 69 && m.leds.1),
        KeyKind::Action(Action::Fn) => m.fn_on,
        // M3: select-while-moving on, laser on
        KeyKind::Action(Action::SelectMode) => m.select,
        KeyKind::Action(Action::Laser) => m.laser,
        _ => false,
    };
    let col = if lit { p.accent } else if sym { p.key } else { p.key_mod };
    let rad = m.px_per_mm * 1.2 * corner(m);
    cv.round_rect(&k.rect, rad, col);
    if on && !lit {
        cv.outline(&k.rect.inset(m.px_per_mm * 0.3), m.px_per_mm * 0.6, p.accent);
        let locked = matches!(&k.kind, KeyKind::Code(c) if m.latched.iter().any(|(l, lk)| l == c && *lk));
        if locked || matches!(&k.kind, KeyKind::Code(58 | 69)) {
            cv.circle(k.rect.right() - m.px_per_mm * 2.2, k.rect.y + m.px_per_mm * 2.2, m.px_per_mm * 0.9, p.accent);
        }
    }
    let l = key_label(k, m, labels);
    let size = (k.rect.h * 0.42).min(m.px_per_mm * 5.0);
    let size = if l.chars().count() > 2 { size * 0.72 } else { size };
    f.draw_centered(cv, &k.rect.inset(1.0), &l, size, if sym || lit { p.text } else { p.text_dim });
    if let Some(alt) = &k.long {
        let al = match alt {
            KeyKind::Code(c) => code_label(*c, false, labels),
            KeyKind::Combo(v) => code_label(*v.last().unwrap_or(&0), true, labels),
            _ => String::new(),
        };
        let r = Rect::new(k.rect.right() - k.rect.w * 0.4, k.rect.y + 1.0, k.rect.w * 0.36, k.rect.h * 0.3);
        f.draw_centered(cv, &r, &al, size * 0.45, p.text_dim);
    }
}

fn draw_button(cv: &mut Canvas, f: &mut Fonts, ppm: f32, p: &Palette, r: &Rect, label: &str, lit: bool) {
    cv.round_rect(r, ppm * 1.5, if lit { p.accent } else { p.btn });
    f.draw_centered(cv, r, label, (r.h * 0.42).min(ppm * 4.0), p.text);
}

fn draw_static(cv: &mut Canvas, f: &mut Fonts, m: &UiModel, labels: Option<&crate::xkb::Labels>) {
    let p = pal(m);
    cv.clear(p.bg);
    let ppm = m.px_per_mm;
    let cr = corner(m);
    cv.fill(&m.strip, p.strip);
    let tr = Rect::new(m.strip.x + ppm * 3.0, m.strip.y, ppm * 22.0, m.strip.h);
    f.draw_left(cv, &tr, &m.title, m.strip.h * 0.4, p.text);
    let first_btn = m.buttons.iter().filter(|b| m.strip.contains(b.rect.x, b.rect.y)).map(|b| b.rect.x).fold(m.strip.right(), f32::min);
    let hw = (first_btn - tr.right() - ppm * 2.0).min(ppm * 120.0);
    let hsize = m.strip.h * 0.28;
    let mut line = if m.edit { m.hint.clone() } else if !m.notice.is_empty() { m.notice.clone() } else { m.hint.clone() };
    if !m.edit && !m.info.is_empty() {
        line = format!("{}    {}", m.info, line);
    }
    if hw > ppm * 10.0 {
        let mut s = line;
        while !s.is_empty() && f.width(&s, hsize * cv.scale) > hw * cv.scale {
            s.pop();
        }
        let hr = Rect::new(tr.right(), m.strip.y, hw, m.strip.h);
        f.draw_left(cv, &hr, &s, hsize, if m.edit { p.edit } else if !m.notice.is_empty() { p.accent } else { p.text_dim });
    }
    for r in &m.regions {
        if r.id == dexhid_core::arrangement::TOUCHPAD {
            cv.round_rect(&r.rect, ppm * 2.5 * cr, p.pad_edge);
            cv.round_rect(&r.rect.inset(ppm * 0.4), ppm * 2.2 * cr, p.pad);
        }
        if r.id == dexhid_core::arrangement::QUICK {
            cv.round_rect(&r.rect, ppm * 2.0 * cr, p.zone);
        }
    }
    // M3: the presentation laser area
    if m.laser {
        if let Some(r) = m.regions.iter().find(|r| r.id == dexhid_core::arrangement::TOUCHPAD) {
            cv.outline(&r.rect.inset(ppm * 1.0), ppm * 0.8, 0xffd04040);
            f.draw_centered(cv, &Rect::new(r.rect.x, r.rect.bottom() - ppm * 9.0, r.rect.w, ppm * 7.0), "laser pointer: the pad is the monitor", ppm * 3.0, p.text_dim);
        }
    }
    // M3: bands (word suggestions, clipboard history)
    for b in &m.bands {
        cv.fill(&b.rect, p.strip);
        cv.fill(&Rect::new(b.rect.x, b.rect.bottom() - ppm * 0.15, b.rect.w, ppm * 0.15), p.pad_edge);
        if b.chips.is_empty() && !b.empty.is_empty() {
            f.draw_left(cv, &Rect::new(b.rect.x + ppm * 3.0, b.rect.y, b.rect.w, b.rect.h), &b.empty, b.rect.h * 0.32, p.text_dim);
        }
        for (l, r) in &b.chips {
            cv.round_rect(r, ppm * 1.5, p.btn);
            f.draw_centered(cv, r, l, (r.h * 0.4).min(ppm * 3.6), p.text);
        }
    }
    if let Some((l, r)) = &m.zones {
        cv.fill(&l.inset(ppm * 0.6), p.zone);
        cv.fill(&r.inset(ppm * 0.6), p.zone);
        cv.fill(&Rect::new(r.x - ppm * 0.15, r.y + ppm, ppm * 0.3, r.h - 2.0 * ppm), p.pad_edge);
        f.draw_centered(cv, l, "left click", ppm * 3.0, p.text_dim);
        f.draw_centered(cv, r, "right click", ppm * 3.0, p.text_dim);
    }
    for q in &m.quick {
        draw_button(cv, f, ppm, &p, &q.rect, &q.label, false);
    }
    for k in &m.keys {
        draw_key(cv, f, m, &p, k, false, labels);
    }
    if m.edit {
        for r in &m.regions {
            cv.outline(&r.rect, ppm * 0.7, p.edit);
            let hs = ppm * 6.0;
            for (x, y) in [(r.rect.x, r.rect.y), (r.rect.right() - hs, r.rect.y), (r.rect.x, r.rect.bottom() - hs), (r.rect.right() - hs, r.rect.bottom() - hs)] {
                cv.fill(&Rect::new(x, y, hs, hs), p.edit);
            }
            // edge handles (M2): short bars in the middle of each side
            let (bl, bt) = ((r.rect.w * 0.18).min(ppm * 16.0), ppm * 1.6);
            let (cx, cy) = r.rect.center();
            cv.round_rect(&Rect::new(cx - bl / 2.0, r.rect.y, bl, bt), bt / 2.0, p.edit);
            cv.round_rect(&Rect::new(cx - bl / 2.0, r.rect.bottom() - bt, bl, bt), bt / 2.0, p.edit);
            let bl2 = (r.rect.h * 0.18).min(ppm * 16.0);
            cv.round_rect(&Rect::new(r.rect.x, cy - bl2 / 2.0, bt, bl2), bt / 2.0, p.edit);
            cv.round_rect(&Rect::new(r.rect.right() - bt, cy - bl2 / 2.0, bt, bl2), bt / 2.0, p.edit);
            let c = Rect::new(r.rect.x, r.rect.y + r.rect.h / 2.0 - ppm * 5.0, r.rect.w, ppm * 10.0);
            cv.round_rect(&c.inset(ppm), ppm, 0xc0101318);
            f.draw_centered(cv, &c, &r.id.replace('_', " "), ppm * 5.0, p.edit);
        }
        // snap guides while dragging
        for g in &m.guides {
            let w = ppm * 0.35;
            let r = if g.vertical { Rect::new(g.pos - w / 2.0, g.from, w, g.to - g.from) } else { Rect::new(g.from, g.pos - w / 2.0, g.to - g.from, w) };
            cv.fill(&r, p.accent);
        }
    }
    if let Some(s) = &m.sheet {
        cv.round_rect(&s.rect, ppm * 3.0, p.card);
        if let Some(b) = m.buttons.iter().find(|b| matches!(b.btn, Btn::SheetTab(_))) {
            let tabs = Rect::new(b.rect.x - ppm, s.rect.y + ppm, b.rect.w + 2.0 * ppm, s.rect.h - 2.0 * ppm);
            cv.round_rect(&tabs, ppm * 2.0, p.strip);
        }
        let x0 = s.rows.first().map(|r| r.rect.x).unwrap_or(s.rect.x + ppm * 60.0);
        let tr = Rect::new(x0, s.rect.y + ppm * 2.0, s.rect.right() - x0 - ppm * 2.0, ppm * 8.0);
        f.draw_left(cv, &tr, &s.title, ppm * 4.5, p.text);
        if !s.subtitle.is_empty() {
            let sr = Rect::new(x0, tr.bottom() - ppm * 1.5, tr.w, ppm * 5.0);
            f.draw_left(cv, &sr, &s.subtitle, ppm * 2.8, p.edit);
        }
        for r in &s.rows {
            cv.fill(&Rect::new(r.rect.x, r.rect.bottom(), r.rect.w, ppm * 0.15), p.pad_edge);
            let lr = Rect::new(r.rect.x + ppm, r.rect.y, r.rect.w * 0.55, r.rect.h);
            f.draw_left(cv, &lr, &r.title, ppm * 3.4, if r.live { p.text } else { p.text_dim });
        }
    }
    // M3: the emoji panel card (over the keys, like the settings sheet)
    if let Some(e) = &m.emoji {
        cv.round_rect(&e.rect, ppm * 3.0, p.card);
    }
    for b in &m.buttons {
        let sel = matches!(b.btn, Btn::SheetTab(i) if m.sheet.as_ref().is_some_and(|s| s.tab == i as usize)) || matches!(b.btn, Btn::EmojiTab(i) if m.emoji.as_ref().is_some_and(|e| e.page == i as usize));
        if matches!(b.btn, Btn::Emoji(_)) {
            cv.round_rect(&b.rect, ppm * 1.5, p.btn);
            f.draw_centered(cv, &b.rect, &b.label, b.rect.h * 0.62, p.text);
            continue;
        }
        draw_button(cv, f, ppm, &p, &b.rect, &b.label, sel);
    }
}

/// dynamic layer: pressed keys, pressed button, touch dots. Returns the buffer rects touched.
fn draw_dynamic(cv: &mut Canvas, f: &mut Fonts, m: &UiModel, labels: Option<&crate::xkb::Labels>) -> Vec<(i32, i32, i32, i32)> {
    let p = pal(m);
    let mut d = vec![];
    for &i in &m.lit_keys {
        if let Some(k) = m.keys.get(i) {
            draw_key(cv, f, m, &p, k, true, labels);
            d.push(cv.damage_of(&k.rect));
        }
    }
    if let Some(b) = m.lit_button.and_then(|b| m.buttons.iter().find(|v| v.btn == b)) {
        draw_button(cv, f, m.px_per_mm, &p, &b.rect, &b.label, true);
        d.push(cv.damage_of(&b.rect));
    }
    if let Some(q) = m.lit_quick.and_then(|i| m.quick.get(i)) {
        draw_button(cv, f, m.px_per_mm, &p, &q.rect, &q.label, true);
        d.push(cv.damage_of(&q.rect));
    }
    let r = m.px_per_mm * 4.0;
    let dot = (p.accent & 0x00ff_ffff) | 0xc000_0000;
    for &(x, y) in &m.dots {
        cv.circle(x, y, r, dot);
        d.push(cv.damage_of(&Rect::new(x - r, y - r, 2.0 * r, 2.0 * r)));
    }
    d
}
/// M3: the mirror preview frame (scaled into its rect, nearest neighbour); a placeholder until the first frame
fn draw_mirror(cv: &mut Canvas, f: &mut Fonts, m: &UiModel, frame: Option<&(u32, u32, Vec<u32>)>) -> Option<(i32, i32, i32, i32)> {
    let r = m.mirror?;
    let p = pal(m);
    let d = cv.damage_of(&r);
    match frame {
        Some((w, h, px)) if *w > 0 && *h > 0 => {
            let (x0, y0, x1, y1) = (d.0.max(0), d.1.max(0), (d.0 + d.2).min(cv.bw), (d.1 + d.3).min(cv.bh));
            let (dw, dh) = ((x1 - x0).max(1) as u32, (y1 - y0).max(1) as u32);
            for y in y0..y1 {
                let sy = ((y - y0) as u32 * h / dh).min(h - 1);
                for x in x0..x1 {
                    let sx = ((x - x0) as u32 * w / dw).min(w - 1);
                    cv.px[(y * cv.bw + x) as usize] = px[(sy * w + sx) as usize];
                }
            }
            cv.outline(&r, m.px_per_mm * 0.3, p.pad_edge);
        }
        _ => {
            cv.round_rect(&r, m.px_per_mm * 1.5, 0xe0101318);
            f.draw_centered(cv, &r, "mirror preview: waiting for the screen", m.px_per_mm * 2.6, p.text_dim);
        }
    }
    Some(d)
}

fn prompt_layout(w: f32, h: f32, ppm: f32) -> (Rect, Vec<(Rect, Hit, &'static str)>) {
    let cw = (150.0 * ppm).min(w * 0.94);
    let ch = (78.0 * ppm).min(h * 0.9);
    let card = Rect::new((w - cw) / 2.0, (h - ch) / 2.0, cw, ch);
    let m = ppm * 5.0;
    let bw = (cw - 3.0 * m) / 2.0;
    let bh = ppm * 14.0;
    let y1 = card.y + ch - 2.0 * bh - 2.0 * m + ppm * 2.0;
    let y2 = y1 + bh + m * 0.6;
    let b = vec![
        (Rect::new(card.x + m, y1, bw, bh), Hit::Choice(Choice::UsePad), "Use as touchpad + keyboard"),
        (Rect::new(card.x + 2.0 * m + bw, y1, bw, bh), Hit::Choice(Choice::KeepDisplay), "Keep as display"),
        (Rect::new(card.x + m, y2, bw, bh * 0.8), Hit::Choice(Choice::Always), "Always for this monitor"),
        (Rect::new(card.x + 2.0 * m + bw, y2, bw, bh * 0.8), Hit::Choice(Choice::Never), "Never for this monitor"),
    ];
    (card, b)
}

fn pill_layout(w: f32, ppm: f32) -> (Rect, Vec<(Rect, Hit, &'static str)>) {
    let pw = 62.0 * ppm;
    let ph = 10.0 * ppm;
    let p = Rect::new((w - pw) / 2.0, ppm * 1.5, pw, ph);
    let hw = ph;
    (p, vec![(Rect::new(p.x, p.y, pw - hw, ph), Hit::PillPad, "Use as pad"), (Rect::new(p.right() - hw, p.y, hw, ph), Hit::PillHide, "\u{d7}")])
}

// ------------------------------------------------------------------ thread

pub fn run(sh: SharedRef, wake_rd: i32) {
    let cfg = sh.ui_cfg.clone();
    let lost = |why: String| {
        println!("UI-LOST reason={}", why);
        sh.send(Cmd::UiLost(why));
    };
    let conn = match Connection::connect_to_env() {
        Ok(c) => c,
        Err(e) => return lost(format!("wayland connect: {} (WAYLAND_DISPLAY={:?})", e, std::env::var("WAYLAND_DISPLAY").ok())),
    };
    let mut q = conn.new_event_queue::<St>();
    let qh = q.handle();
    conn.display().get_registry(&qh, ());
    let mut st = St {
        compositor: None,
        shm: None,
        layer: None,
        wm: None,
        seat: None,
        touch: None,
        pointer: None,
        keyboard: None,
        outputs: vec![],
        outputs_changed: true,
        configured: None,
        xdg_size: (0, 0),
        closed: false,
        frame_pending: false,
        frame_done: false,
        released: vec![],
        keymap: None,
        input: vec![],
        ptr_pos: (0.0, 0.0),
    };
    for _ in 0..3 {
        if q.roundtrip(&mut st).is_err() {
            return lost("wayland roundtrip".into());
        }
    }
    println!(
        "UI-GLOBALS layer_shell={} xdg_wm_base={} seat={} outputs={}",
        st.layer.is_some(),
        st.wm.is_some(),
        st.seat.is_some(),
        st.outputs.iter().map(|o| format!("{}:{}x{}@{}:t{}", o.name, o.mode.0, o.mode.1, o.scale, o.transform)).collect::<Vec<_>>().join(",")
    );
    let (Some(comp), Some(shm)) = (st.compositor.clone(), st.shm.clone()) else {
        return lost("no wl_compositor / wl_shm".into());
    };
    if st.layer.is_none() && st.wm.is_none() {
        return lost("neither zwlr_layer_shell_v1 nor xdg_wm_base".into());
    }
    let mut fonts = Fonts::load();
    println!("UI-FONT path={} ok={} emoji={}", fonts.path, fonts.ok(), fonts.emoji_path);
    let mut labels: Option<crate::xkb::Labels> = None;
    let mut labels_gen: u64 = 1;
    let mut last_keymap: Option<String> = None;
    let mut cur_group = sh.xkb_group.load(Ordering::SeqCst);
    let mut surf: Option<Surf> = None;
    let mut want = (UiKind::None, UiModel::default(), 0u64);
    let mut seen_ver = u64::MAX;
    let mut need_draw = false;
    let mut static_px: Vec<u32> = vec![];
    let mut static_key = (0u64, 0u64);
    let mut pool_gen = 0u64;
    let mut last_geo: Option<Geometry> = None;
    let codes = keycodes::all_codes();
    let mut last_legends = String::new();
    // M3: legends of another device's keyboard language; the labels published for the daemon
    let mut over_name: Option<String> = None;
    let mut published_gen = 0u64;
    let mut mirror_seen = 0u64;

    let internal = |outs: &[OutInfo]| -> Option<usize> {
        if cfg.internal_output != "auto" {
            if let Some(i) = outs.iter().position(|o| o.name == cfg.internal_output) {
                return Some(i);
            }
        }
        outs.iter().position(|o| o.name.starts_with("DSI") || o.name.starts_with("eDP") || o.name.starts_with("LVDS")).or(if outs.is_empty() { None } else { Some(0) })
    };

    loop {
        sh.ui_heartbeat.store(mono_ms(), Ordering::SeqCst);
        if st.outputs_changed {
            st.outputs_changed = false;
            let names: Vec<String> = st.outputs.iter().map(|o| o.name.clone()).filter(|n| !n.is_empty()).collect();
            sh.send(Cmd::Outputs(names));
        }
        // keymap / active group (KDE layout tracker) -> legends
        let grp = sh.xkb_group.load(Ordering::SeqCst);
        if st.keymap.is_some() || (grp != cur_group && last_keymap.is_some()) {
            if let Some(km) = st.keymap.take() {
                last_keymap = Some(km);
            }
            cur_group = grp;
            match crate::xkb::labels(last_keymap.as_deref().unwrap_or(""), grp, &codes) {
                Ok(l) => {
                    println!("XKB layouts={} group={} names={:?} labelled={} sample={}", l.layouts.len(), l.group, l.layouts, l.map.len(), [16u16, 17, 21, 44].iter().map(|c| l.map.get(c).map(|x| x[0].clone()).unwrap_or_default()).collect::<String>());
                    sh.send(Cmd::Labels(l.map.len()));
                    if over_name.is_none() {
                        labels = Some(l);
                    }
                }
                Err(e) => println!("XKB {} (legends stay US)", e),
            }
            labels_gen += 1;
            need_draw = true;
        }
        let lo = sh.legend_layout.lock().unwrap().clone();
        if lo != over_name {
            over_name = lo.clone();
            labels = match &lo {
                Some(lay) => match crate::xkb::keymap_from_layout(lay).and_then(|k| crate::xkb::labels(&k, 0, &codes)) {
                    Ok(l) => {
                        println!("XKB target layout={} labelled={} sample={}", lay, l.map.len(), [16u16, 17, 21, 44].iter().map(|c| l.map.get(c).map(|x| x[0].clone()).unwrap_or_default()).collect::<String>());
                        Some(l)
                    }
                    Err(e) => {
                        println!("XKB target layout {}: {} (desktop legends kept)", lay, e);
                        last_keymap.as_deref().and_then(|k| crate::xkb::labels(k, cur_group, &codes).ok())
                    }
                },
                None => last_keymap.as_deref().and_then(|k| crate::xkb::labels(k, cur_group, &codes).ok()),
            };
            labels_gen += 1;
            need_draw = true;
        }
        if published_gen != labels_gen {
            published_gen = labels_gen;
            *sh.labels.lock().unwrap() = labels.as_ref().map(|l| l.map.clone()).unwrap_or_default();
        }
        let mg = sh.mirror_gen.load(Ordering::SeqCst);
        if mg != mirror_seen {
            mirror_seen = mg;
            need_draw = true;
        }
        // what should be shown
        let v = sh.ui_ver.load(Ordering::SeqCst);
        if v != seen_ver {
            seen_ver = v;
            want = sh.ui.lock().unwrap().clone();
            need_draw = true;
        }
        let cur_kind = surf.as_ref().map(|s| s.kind).unwrap_or(UiKind::None);
        let cur_epoch = surf.as_ref().map(|s| s.epoch);
        if cur_kind != want.0 || (want.0 != UiKind::None && cur_epoch != Some(want.2)) {
            surf = None; // drop destroys
            st.configured = None;
            st.closed = false;
            st.frame_pending = false;
            st.frame_done = false;
            st.input.clear();
            if want.0 != UiKind::None {
                let Some(oi) = internal(&st.outputs) else {
                    lost("no output to show the UI on".into());
                    return;
                };
                let out = st.outputs[oi].proxy.clone().unwrap();
                let s = comp.create_surface(&qh, ());
                let scale = st.outputs[oi].scale.max(1);
                let mut layer = None;
                let mut xdg = None;
                if let Some(ls) = &st.layer {
                    use zwlr_layer_surface_v1::Anchor;
                    let l = ls.get_layer_surface(&s, Some(&out), zwlr_layer_shell_v1::Layer::Overlay, "dexhid".into(), &qh, ());
                    l.set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::None);
                    l.set_exclusive_zone(-1);
                    if want.0 == UiKind::Pill {
                        let ppm = ppm_of(&st.outputs[oi], &cfg);
                        l.set_anchor(Anchor::Top | Anchor::Left | Anchor::Right);
                        l.set_size(0, (14.0 * ppm).round() as u32);
                    } else {
                        l.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
                        l.set_size(0, 0);
                    }
                    layer = Some(l);
                } else if let Some(wm) = &st.wm {
                    let xs = wm.get_xdg_surface(&s, &qh, ());
                    let tl = xs.get_toplevel(&qh, ());
                    tl.set_title("DexHid".into());
                    tl.set_app_id("org.dexhid.DexHid".into());
                    tl.set_fullscreen(Some(&out));
                    xdg = Some((xs, tl));
                }
                s.set_buffer_scale(scale);
                s.commit();
                println!("UI-SURFACE kind={} via={} output={} scale={}", want.0.name(), if layer.is_some() { "layer-shell" } else { "xdg-fullscreen" }, st.outputs[oi].name, scale);
                surf = Some(Surf { kind: want.0, epoch: want.2, surface: s, layer, xdg, w: 0, h: 0, scale, pool: None, mapped: false, hits: vec![], pressed: HashMap::new(), pos: HashMap::new(), lit: None, last_dyn: vec![] });
            }
        }
        if st.closed && surf.is_some() {
            lost("surface closed by the compositor".into());
            return;
        }
        // configure -> size, geometry
        if let (Some(s), Some((w, h))) = (surf.as_mut(), st.configured) {
            let (mut w, mut h) = (w as i32, h as i32);
            if let Some(oi) = internal(&st.outputs) {
                let o = &st.outputs[oi];
                let (lw, lh) = logical_size(o);
                if w == 0 {
                    w = lw;
                }
                if h == 0 {
                    h = lh;
                }
                if s.kind != UiKind::Pill {
                    let t = Transform::from_name(&cfg.touch_transform).unwrap_or(Transform::from_wl(o.transform));
                    let g = Geometry { w: w as f32, h: h as f32, transform: t, native_mm: cfg.size_mm, tmax: cfg.tmax };
                    if last_geo != Some(g) {
                        last_geo = Some(g);
                        println!("UI-GEOMETRY w={} h={} transform={} ppm={:.2} output={}", w, h, t.name(), g.px_per_mm(), o.name);
                        sh.send(Cmd::Geometry(g));
                    }
                }
            }
            if (w, h) != (s.w, s.h) {
                s.w = w;
                s.h = h;
                s.pool = None;
                need_draw = true;
            }
        }
        // released buffers
        for (g, i) in st.released.drain(..) {
            if let Some(p) = surf.as_mut().and_then(|s| s.pool.as_mut()) {
                if p.gen == g && i < p.bufs.len() {
                    p.bufs[i].busy = false;
                }
            }
        }
        // prompt / pill input
        if let Some(s) = surf.as_mut() {
            let evs: Vec<In> = st.input.drain(..).collect();
            let hits = s.hits.clone();
            let hit = |x: f64, y: f64| hits.iter().find(|(r, _)| r.contains(x as f32, y as f32)).map(|(_, h)| *h);
            for e in evs {
                if !matches!(s.kind, UiKind::Prompt | UiKind::Pill) {
                    continue;
                }
                match e {
                    In::Down(id, x, y) => {
                        s.pos.insert(id, (x, y));
                        if let Some(h) = hit(x, y) {
                            s.pressed.insert(id, h);
                            s.lit = Some(h);
                            need_draw = true;
                        }
                    }
                    In::Motion(id, x, y) => {
                        s.pos.insert(id, (x, y));
                    }
                    In::Up(id) => {
                        let cancel = id == -100;
                        let ids: Vec<i32> = if cancel { s.pressed.keys().copied().collect() } else { vec![id] };
                        for id in ids {
                            if let Some(h) = s.pressed.remove(&id) {
                                let p = s.pos.remove(&id).unwrap_or((-1.0, -1.0));
                                let fire = !cancel && hit(p.0, p.1) == Some(h);
                                println!("UI-TAP t={} target={} fire={}", mono_ms(), h.name(), fire);
                                if fire {
                                    match h {
                                        Hit::Choice(c) => sh.send(Cmd::Prompt(c)),
                                        Hit::PillPad => sh.send(Cmd::PillUsePad),
                                        Hit::PillHide => sh.send(Cmd::PillHide),
                                    }
                                }
                                s.lit = None;
                                need_draw = true;
                            }
                        }
                    }
                }
            }
        } else {
            st.input.clear();
        }
        // draw
        if let Some(s) = surf.as_mut() {
            if need_draw && s.w > 0 && s.h > 0 && !st.frame_pending {
                let (bw, bh) = (s.w * s.scale, s.h * s.scale);
                if s.pool.is_none() {
                    pool_gen += 1;
                    s.pool = Pool::new(&shm, bw, bh, s.kind != UiKind::Overlay, pool_gen, &qh);
                    if s.pool.is_none() {
                        lost("shm buffer allocation failed".into());
                        return;
                    }
                }
                let pool = s.pool.as_mut().unwrap();
                if let Some(bi) = pool.bufs.iter().position(|b| !b.busy) {
                    let scale = s.scale as f32;
                    let mut damage: Vec<(i32, i32, i32, i32)> = vec![];
                    let model = &want.1;
                    let ppm = if model.px_per_mm > 0.0 { model.px_per_mm } else { s.w as f32 / if s.w >= s.h { cfg.size_mm.0.max(cfg.size_mm.1) } else { cfg.size_mm.0.min(cfg.size_mm.1) } };
                    match s.kind {
                        UiKind::Overlay => {
                            let key = (model.version, labels_gen);
                            if static_key != key || static_px.len() != (bw * bh) as usize {
                                static_px.resize((bw * bh) as usize, 0);
                                let mut cv = Canvas { px: &mut static_px, bw, bh, scale };
                                draw_static(&mut cv, &mut fonts, model, labels.as_ref());
                                static_key = key;
                                if sh.debug {
                                    let l = legends_line(model, labels.as_ref());
                                    if l != last_legends {
                                        println!("{} t={}", l, mono_ms());
                                        last_legends = l;
                                    }
                                }
                            }
                            let full = pool.bufs[bi].key != static_key;
                            let prev = std::mem::take(&mut pool.bufs[bi].dyn_rects);
                            let px = pool.px(bi);
                            let mut cv = Canvas { px, bw, bh, scale };
                            if full {
                                cv.px.copy_from_slice(&static_px);
                                damage.push((0, 0, bw, bh));
                            } else {
                                for r in &prev {
                                    cv.copy_from(&static_px, *r);
                                }
                                damage.extend(prev.iter().copied());
                                damage.extend(s.last_dyn.iter().copied());
                            }
                            let mut d = draw_dynamic(&mut cv, &mut fonts, model, labels.as_ref());
                            // M3: the mirror preview (redrawn with every frame)
                            if model.mirror.is_some() {
                                let fr = sh.mirror.lock().unwrap();
                                if let Some(md) = draw_mirror(&mut cv, &mut fonts, model, fr.as_ref()) {
                                    d.push(md);
                                }
                            }
                            damage.extend(d.iter().copied());
                            pool.bufs[bi].dyn_rects = d.clone();
                            pool.bufs[bi].key = static_key;
                            s.last_dyn = d;
                        }
                        UiKind::Prompt | UiKind::Pill => {
                            let px = pool.px(bi);
                            let mut cv = Canvas { px, bw, bh, scale };
                            cv.clear(render::SHADE);
                            let (frame, hits) = draw_prompt_m3(&mut cv, &mut fonts, s.kind, s.w as f32, s.h as f32, ppm, s.lit);
                            s.hits = hits;
                            damage.push((0, 0, bw, bh));
                            // only the card / pill takes input; the rest of the screen stays usable
                            let reg = comp.create_region(&qh, ());
                            let (x, y, w, h) = frame.round();
                            reg.add(x, y, w, h);
                            s.surface.set_input_region(Some(&reg));
                            reg.destroy();
                        }
                        UiKind::None => {}
                    }
                    s.surface.attach(Some(&pool.bufs[bi].wl), 0, 0);
                    for (x, y, w, h) in damage {
                        s.surface.damage_buffer(x, y, w, h);
                    }
                    s.surface.frame(&qh, ());
                    st.frame_pending = true;
                    s.surface.commit();
                    pool.bufs[bi].busy = true;
                    need_draw = false;
                }
            }
            if st.frame_done && !s.mapped {
                s.mapped = true;
                println!("UI-MAPPED t={} kind={} size={}x{} scale={}", mono_ms(), s.kind.name(), s.w, s.h, s.scale);
                if matches!(s.kind, UiKind::Prompt | UiKind::Pill) {
                    if let Some(oi) = internal(&st.outputs) {
                        let o = &st.outputs[oi];
                        let (lw, lh) = logical_size(o);
                        let t = Transform::from_name(&cfg.touch_transform).unwrap_or(Transform::from_wl(o.transform));
                        let g = Geometry { w: lw as f32, h: lh as f32, transform: t, native_mm: cfg.size_mm, tmax: cfg.tmax };
                        for (r, h) in &s.hits {
                            let (x0, y0, x1, y1) = g.rect_to_touch(r);
                            println!("GEOM name={} native={},{},{},{} logical={:.0},{:.0},{:.0},{:.0}", h.name(), x0, y0, x1, y1, r.x, r.y, r.w, r.h);
                        }
                    }
                }
                sh.send(Cmd::Mapped(s.kind, s.epoch));
            }
        }
        // wayland + wake poll
        if q.dispatch_pending(&mut st).is_err() {
            return lost("wayland dispatch error".into());
        }
        if let Err(e) = q.flush() {
            if !matches!(&e, wayland_client::backend::WaylandError::Io(io) if io.kind() == std::io::ErrorKind::WouldBlock) {
                return lost(format!("wayland flush: {}", e));
            }
        }
        let Some(guard) = q.prepare_read() else { continue };
        let wlfd = guard.connection_fd().as_raw_fd();
        let r = poll_in(&[wlfd, wake_rd], 500);
        if r[0] {
            if let Err(e) = guard.read() {
                if !matches!(&e, wayland_client::backend::WaylandError::Io(io) if io.kind() == std::io::ErrorKind::WouldBlock) {
                    return lost(format!("wayland read: {}", e));
                }
            }
        } else {
            drop(guard);
        }
        if r[1] {
            drain(wake_rd);
        }
    }
}

fn logical_size(o: &OutInfo) -> (i32, i32) {
    let s = o.scale.max(1);
    let (w, h) = (o.mode.0 / s, o.mode.1 / s);
    if matches!(o.transform, 1 | 3 | 5 | 7) {
        (h, w)
    } else {
        (w, h)
    }
}

fn ppm_of(o: &OutInfo, cfg: &crate::shared::UiCfg) -> f32 {
    let (w, _) = logical_size(o);
    let mm = if matches!(o.transform, 1 | 3 | 5 | 7) { cfg.size_mm.1 } else { cfg.size_mm.0 };
    (w as f32 / mm.max(1.0)).max(1.0)
}

/// `dexhid debug render --layout full [--portrait] [--edit] [--shift] [--keymap FILE] [--scale N] --out F.ppm`:
/// draw the pad-mode screen of a layout into a PPM file (PC check of layouts, legends and the renderer)
fn debug_scene(a: &crate::debug::Args) -> Result<(UiModel, Option<crate::xkb::Labels>, Vec<String>), i32> {
    use dexhid_core::router::{Router, RouterCfg};
    let name = a.val("--layout").unwrap_or_else(|| "full".into());
    let mut dirs = vec![std::path::PathBuf::from("/usr/share/dexhid/layouts")];
    if let Some(d) = a.val("--layout-dir") {
        dirs.push(d.into());
    }
    let (ls, errs) = dexhid_core::layout::load_all(&dirs);
    for e in errs {
        println!("WARN {}", e);
    }
    let Some(l) = ls.into_iter().find(|l| l.name == name) else {
        println!("FAIL no layout {}", name);
        return Err(1);
    };
    let portrait = a.flag("--portrait");
    let g = Geometry {
        w: if portrait { 1200.0 } else { 1696.0 },
        h: if portrait { 1696.0 } else { 1200.0 },
        transform: if portrait { Transform::R90 } else { Transform::Normal },
        native_mm: (274.0, 193.0),
        tmax: (33919, 23999),
    };
    let mut cfg = RouterCfg::default();
    let _ = &mut cfg;
    if let Some(o) = a.val("--options") {
        cfg.options = o.split(',').filter(|s| !s.is_empty()).map(|s| s.to_string()).collect();
    }
    if a.flag("--quick") {
        cfg.quick = true;
    }
    if a.flag("--laptop") {
        cfg.laptop = true;
        cfg.laptop_pad_pct = a.f64("--pad-pct", 100.0) as f32;
    }
    // M3 render options
    cfg.clip_band = a.flag("--bands");
    cfg.suggest_band = a.flag("--bands");
    cfg.emoji = a.flag("--bands") || a.val("--emoji").is_some();
    cfg.mirror_pct = if a.flag("--mirror") { 35.0 } else { 0.0 };
    cfg.textedit = a.flag("--select");
    cfg.laser = a.flag("--laser");
    cfg.target_label = a.val("--target");
    let mut r = Router::new(cfg, l);
    // M2.1: the desktop look from a kdeglobals file (+ optional plasmashellrc)
    if let Some(f) = a.val("--desk") {
        let kg = std::fs::read_to_string(&f).unwrap_or_default();
        let ps = a.val("--plasmashellrc").and_then(|p| std::fs::read_to_string(p).ok());
        let d = feedback::desk_look(&kg, ps.as_deref(), None, a.val("--radius-px").and_then(|v| v.parse().ok()));
        println!("DESK {:?}", d.as_ref().map(|d| (&d.scheme, d.translucent)));
        r.set_theme("desktop", "#4a8be0", 100);
        r.set_look(d);
    }
    if a.flag("--quick") {
        let q = dexhid_core::config::QuickCfg::default();
        let (items, _) = dexhid_core::quick::resolve(&q, &|_| true);
        r.set_quick(items.iter().map(|i| (i.id.clone(), i.label.clone(), if i.id == "paste" { 2 } else { i.size })).collect());
    }
    if let Some(t) = a.val("--theme") {
        r.set_theme(&t, &a.val("--accent").unwrap_or_else(|| "#4a8be0".into()), a.val("--key-alpha").and_then(|v| v.parse().ok()).unwrap_or(100));
    }
    r.set_info(&a.val("--info").unwrap_or_else(|| "84 % charging  |  DP-1".into()));
    r.set_geometry(g);
    if a.flag("--bands") {
        r.set_clips(vec!["git status".into(), "https://example.org/docs/page".into(), "Meeting at 10:30 tomorrow".into(), "PNG 214 KB".into()]);
        r.set_suggestions(vec!["the".into(), "that".into(), "this".into()]);
    }
    if let Some(pg) = a.val("--emoji").and_then(|v| v.parse::<usize>().ok()) {
        r.toggle_emoji();
        if pg > 0 {
            use dexhid_core::mt::Contact;
            if let Some(t) = r.model().buttons.iter().find(|b| b.name == format!("emoji.tab.{}", pg)).map(|b| b.rect) {
                let (x, y) = g.logical_to_touch(t.x + t.w / 2.0, t.y + t.h / 2.0);
                r.contact(Contact::Down { id: 920, x, y, major: 10 }, 9000);
                r.contact(Contact::Up { id: 920 }, 9030);
            }
        }
    }
    if a.flag("--select") {
        r.toggle_select();
    }
    if a.flag("--edit") || a.flag("--guides") {
        // long press on the strip -> edit mode (as a user would)
        use dexhid_core::mt::Contact;
        let s = r.model().strip;
        let (x, y) = g.logical_to_touch(s.x + s.w * 0.3, s.y + s.h / 2.0);
        r.contact(Contact::Down { id: 900, x, y, major: 10 }, 0);
        r.tick(700);
        r.contact(Contact::Up { id: 900 }, 800);
        if a.flag("--guides") {
            let tp = r.model().regions.iter().find(|x| x.id == "touchpad").map(|x| x.rect).unwrap();
            let kb = r.model().regions.iter().find(|x| x.id != "touchpad").map(|x| x.rect).unwrap();
            let (x, y) = g.logical_to_touch(tp.x + tp.w / 2.0, tp.y + 2.0);
            r.contact(Contact::Down { id: 901, x, y, major: 10 }, 1000);
            let (x2, y2) = g.logical_to_touch(tp.x + tp.w / 2.0, kb.bottom() + 4.0);
            r.contact(Contact::Move { id: 901, x: x2, y: y2, major: 10 }, 1100);
        }
    }
    if let Some(md) = a.val("--sheet") {
        let c = dexhid_core::config::Config::default();
        let v = toml::Value::try_from(&c).unwrap();
        let mut mods: Vec<dexhid_core::router::SheetModule> = dexhid_core::settings::MODULES
            .iter()
            .map(|m| dexhid_core::router::SheetModule { id: m.id.into(), title: m.title.into(), live: m.implemented, plan: m.plan.into(), rows: dexhid_core::settings::rows(m.id, &v, &Default::default()) })
            .collect();
        if let Some(q) = mods.iter_mut().find(|m| m.id == "quick") {
            for (id, label) in dexhid_core::quick::all_ids(&c.quick_actions) {
                let s = dexhid_core::quick::size_of(&c.quick_actions, &id);
                q.rows.push(dexhid_core::settings::Row { key: format!("quick:{}", id), title: label, value: if s == 0 { "off".into() } else { "on".into() }, kind: dexhid_core::settings::Kind::Choice(&["off", "on", "large"]), live: true });
            }
        }
        r.set_sheet(mods);
        use dexhid_core::mt::Contact;
        let b = r.model().buttons.iter().find(|b| b.name == "strip.settings").map(|b| b.rect).unwrap();
        let (x, y) = g.logical_to_touch(b.x + b.w / 2.0, b.y + b.h / 2.0);
        r.contact(Contact::Down { id: 910, x, y, major: 10 }, 5000);
        r.contact(Contact::Up { id: 910 }, 5030);
        if let Some(t) = r.model().buttons.iter().find(|b| b.name == format!("sheet.tab.{}", md)).map(|b| b.rect) {
            let (x, y) = g.logical_to_touch(t.x + t.w / 2.0, t.y + t.h / 2.0);
            r.contact(Contact::Down { id: 911, x, y, major: 10 }, 5100);
            r.contact(Contact::Up { id: 911 }, 5130);
        }
    }
    let mut m = r.model().clone();
    m.shift = a.flag("--shift");
    m.altgr = a.flag("--altgr");
    m.fn_on = a.flag("--fn");
    if a.flag("--caps") {
        m.leds = (true, false);
        m.latched = vec![(29, false)];
    }
    if a.flag("--press") {
        m.lit_keys = (0..m.keys.len()).step_by(7).collect();
        m.dots = vec![(m.w * 0.5, m.h * 0.8)];
    }
    let km = match a.val("--xkb-layout") {
        Some(l) => crate::xkb::keymap_from_layout(&l).map_err(|e| println!("XKB {}", e)).ok(),
        None => a.val("--keymap").and_then(|f| std::fs::read_to_string(f).ok()),
    };
    let labels = km.and_then(|k| match crate::xkb::labels(&k, 0, &keycodes::all_codes()) {
        Ok(l) => {
            println!("XKB layouts={:?} labelled={}", l.layouts, l.map.len());
            Some(l)
        }
        Err(e) => {
            println!("XKB {}", e);
            None
        }
    });
    Ok((m, labels, r.geom_lines()))
}

/// the old ("m3") prompt / pill drawing (the daemon's look until M4 is approved)
fn draw_prompt_m3(cv: &mut Canvas, fonts: &mut Fonts, kind: UiKind, w: f32, h: f32, ppm: f32, lit: Option<Hit>) -> (Rect, Vec<(Rect, Hit)>) {
    let (frame, hits) = if kind == UiKind::Prompt { prompt_layout(w, h, ppm) } else { pill_layout(w, ppm) };
    if kind == UiKind::Prompt {
        cv.round_rect(&frame, ppm * 4.0, render::CARD);
        let t = Rect::new(frame.x, frame.y + ppm * 5.0, frame.w, ppm * 12.0);
        fonts.draw_centered(cv, &t, "External display connected", ppm * 6.0, render::TEXT);
        let t2 = Rect::new(frame.x, t.bottom(), frame.w, ppm * 9.0);
        fonts.draw_centered(cv, &t2, "Use this tablet as a touchpad and keyboard?", ppm * 4.0, render::TEXT_DIM);
    } else {
        cv.round_rect(&frame, frame.h / 2.0, render::CARD);
    }
    for (r, h, l) in &hits {
        let on = lit == Some(*h);
        cv.round_rect(r, ppm * 2.0, if on { render::BTN_LIT } else { render::BTN });
        fonts.draw_centered(cv, r, l, (r.h * 0.38).min(ppm * 4.5), render::TEXT);
    }
    (frame, hits.iter().map(|(r, h, _)| (*r, *h)).collect())
}

/// a stand-in desktop behind the prompt / pill renders (they are translucent layer surfaces)
fn mock_desktop(cv: &mut Canvas, w: f32, h: f32) {
    for y in 0..cv.bh {
        let t = y as f32 / cv.bh.max(1) as f32;
        let c = feedback::mix(0xff2a3550, 0xff141821, t);
        cv.px[(y * cv.bw) as usize..((y + 1) * cv.bw) as usize].fill(c);
    }
    cv.round_rect(&Rect::new(w * 0.08, h * 0.1, w * 0.5, h * 0.55), 10.0, 0xff1e232b);
    cv.round_rect(&Rect::new(w * 0.08, h * 0.1, w * 0.5, h * 0.05), 10.0, 0xff2a303a);
    cv.round_rect(&Rect::new(w * 0.45, h * 0.3, w * 0.45, h * 0.5), 10.0, 0xff242a33);
    cv.fill(&Rect::new(0.0, h - h * 0.045, w, h * 0.045), 0xe0101318);
}

/// layouts (name, title) for the app-window picker
fn debug_layouts(a: &crate::debug::Args) -> Vec<(String, String)> {
    let mut dirs = vec![std::path::PathBuf::from("/usr/share/dexhid/layouts")];
    if let Some(d) = a.val("--layout-dir") {
        dirs.push(d.into());
    }
    let (ls, _) = dexhid_core::layout::load_all(&dirs);
    ls.into_iter().filter(|l| l.name != "none").map(|l| (l.name, l.title)).collect()
}

/// M4: `--window home|<module> [--win-w W --win-h H] [--monitor] [--press N]`: the app window prototype
fn render_window(a: &crate::debug::Args, out: &str) -> i32 {
    let page = a.val("--window").unwrap_or_else(|| "home".into());
    let (w, h) = (a.f64("--win-w", 1696.0) as f32, a.f64("--win-h", 1128.0) as f32);
    let ppm = a.f64("--ppm", 1696.0 / 274.0) as f32;
    let scale = a.f64("--scale", 1.0) as f32;
    let mut v = crate::appwin::debug_view(&page, debug_layouts(a), a.flag("--monitor"));
    // M4 mode switch states: --mode usb|pad|screen, --usb-hint host|none, --usb-line TEXT, --detail TEXT
    if let Some(m) = a.val("--mode") {
        v.mode = m;
    }
    v.usb_hint = a.val("--usb-hint").unwrap_or_default();
    if let Some(s) = a.val("--usb-line") {
        v.usb_line = s;
    }
    if let Some(s) = a.val("--detail") {
        v.mode_detail = s;
    }
    let theme = a.val("--theme").unwrap_or_else(|| "dark".into());
    let p = match a.val("--desk") {
        Some(f) => {
            let kg = std::fs::read_to_string(&f).unwrap_or_default();
            match feedback::desk_look(&kg, None, None, None) {
                Some(d) => feedback::palette_desk(&d, 100),
                None => feedback::palette(&theme, "#4a8be0", 100),
            }
        }
        None => feedback::palette(&theme, "#4a8be0", 100),
    };
    let (bw, bh) = ((w * scale) as i32, (h * scale) as i32);
    let mut px = vec![0u32; (bw * bh) as usize];
    let mut fonts = Fonts::load();
    let pressed = a.val("--press-hit").and_then(|s| match s.as_str() {
        "usb" => Some(crate::appwin::WinHit::Mode(0)),
        "pad" => Some(crate::appwin::WinHit::Mode(1)),
        "screen" => Some(crate::appwin::WinHit::Mode(2)),
        _ => None,
    });
    let l = {
        let mut cv = Canvas { px: &mut px, bw, bh, scale };
        crate::appwin::draw(&mut cv, &mut fonts, &p, w, h, ppm, &v, pressed)
    };
    for (r, hit) in &l.hits {
        println!("HIT {:?} {:.0},{:.0},{:.0},{:.0} h_mm={:.1}", hit, r.x, r.y, r.w, r.h, r.h / ppm);
    }
    println!("WINDOW page={} overflow={}", page, l.overflow);
    let mut f = format!("P6\n{} {}\n255\n", bw, bh).into_bytes();
    for p in &px {
        f.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, *p as u8]);
    }
    if let Err(e) = std::fs::write(out, f) {
        println!("FAIL write {}: {}", out, e);
        return 1;
    }
    println!("RENDERED {} {}x{} window", out, bw, bh);
    0
}

pub fn render_debug(a: &crate::debug::Args) -> i32 {
    let name = a.val("--layout").unwrap_or_else(|| "full".into());
    let out = a.val("--out").unwrap_or_else(|| format!("/tmp/dexhid-{}.ppm", name));
    if a.val("--window").is_some() {
        return render_window(a, &out);
    }
    let (m, labels, geom) = match debug_scene(a) {
        Ok(s) => s,
        Err(rc) => return rc,
    };
    let m4 = a.val("--style").as_deref() == Some("m4");
    let scale = a.f64("--scale", 1.0) as f32;
    let (bw, bh) = ((m.w * scale) as i32, (m.h * scale) as i32);
    let mut px = vec![0u32; (bw * bh) as usize];
    let mut fonts = Fonts::load();
    println!("FONT {} ok={}", fonts.path, fonts.ok());
    {
        let mut cv = Canvas { px: &mut px, bw, bh, scale };
        let surface = if a.flag("--prompt") { Some(UiKind::Prompt) } else if a.flag("--pill") { Some(UiKind::Pill) } else { None };
        if let Some(kind) = surface {
            // M4: the prompt / pill over a stand-in desktop; `--lit N` shows button N pressed
            mock_desktop(&mut cv, m.w, m.h);
            let lit = a.val("--lit").and_then(|v| v.parse::<usize>().ok());
            let hits_all = if kind == UiKind::Prompt { prompt_layout(m.w, m.h, m.px_per_mm).1 } else { pill_layout(m.w, m.px_per_mm).1 };
            let lit_m3 = lit.and_then(|i| hits_all.get(i)).map(|x| x.1);
            let p = pal(&m);
            let (frame, hits) = match (m4, kind) {
                (true, UiKind::Prompt) => crate::look::draw_prompt(&mut cv, &mut fonts, &p, m.w, m.h, m.px_per_mm, lit.and_then(|i| crate::look::prompt_layout(m.w, m.h, m.px_per_mm).4.get(i).map(|x| x.1))),
                (true, _) => crate::look::draw_pill(&mut cv, &mut fonts, &p, m.w, m.px_per_mm, lit.map(|i| if i == 0 { Hit::PillPad } else { Hit::PillHide })),
                _ => draw_prompt_m3(&mut cv, &mut fonts, kind, m.w, m.h, m.px_per_mm, lit_m3),
            };
            println!("SURFACE kind={} frame={:.0},{:.0},{:.0},{:.0}", kind.name(), frame.x, frame.y, frame.w, frame.h);
            for (r, h) in &hits {
                println!("HIT {} {:.0},{:.0},{:.0},{:.0} h_mm={:.1}", h.name(), r.x, r.y, r.w, r.h, r.h / m.px_per_mm);
            }
        } else if m4 {
            let p = pal(&m);
            crate::look::draw_static(&mut cv, &mut fonts, &m, &p, corner(&m), labels.as_ref());
            let mut d = vec![];
            crate::look::draw_dynamic(&mut cv, &mut fonts, &m, &p, corner(&m), labels.as_ref(), &mut d);
        } else {
            draw_static(&mut cv, &mut fonts, &m, labels.as_ref());
            draw_dynamic(&mut cv, &mut fonts, &m, labels.as_ref());
        }
        // M3: a synthetic preview frame (a gradient with a "window") for the mirror render
        if a.flag("--mirror") {
            let (w, h) = (480u32, 270u32);
            let px: Vec<u32> = (0..w * h)
                .map(|i| {
                    let (x, y) = (i % w, i / w);
                    if (120..360).contains(&x) && (60..210).contains(&y) { 0xffe8e8e8 } else { 0xff000000 | (30 + x * 60 / w) << 16 | (50 + y * 80 / h) << 8 | 120 }
                })
                .collect();
            draw_mirror(&mut cv, &mut fonts, &m, Some(&(w, h, px)));
        }
    }
    println!("FONT emoji={}", fonts.emoji_path);
    println!("{}", legends_line(&m, labels.as_ref()));
    let mut f = format!("P6\n{} {}\n255\n", bw, bh).into_bytes();
    for p in &px {
        f.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, *p as u8]);
    }
    if let Err(e) = std::fs::write(&out, f) {
        println!("FAIL write {}: {}", out, e);
        return 1;
    }
    for l in geom.iter().take(4) {
        println!("{}", l);
    }
    println!("RENDERED {} {}x{} keys={} regions={}", out, bw, bh, m.keys.len(), m.regions.len());
    let _ = Btn::Display;
    0
}

/// M4 measurement: `dexhid debug bench [--scale 2] [--iters 40] [--desk F --plasmashellrc F]`: per state and look
/// (current "m3" and the M4 prototype) the static-layer redraw, the per-frame dynamic redraw (key press, touch
/// dot), the shift path (static redraw + full copy), the model clone the daemon -> UI hand-off does twice per input
/// event; median ms, heap allocations per call (with `--features alloc-count`), damaged px; then glyph cache, RSS
/// and the buffer memory the UI holds for that size.
pub fn bench_debug(a: &crate::debug::Args) -> i32 {
    use std::time::Instant;
    let scale = a.f64("--scale", 2.0) as f32;
    let iters = a.f64("--iters", 40.0).max(3.0) as usize;
    let mut extra: Vec<String> = vec![];
    for k in ["--desk", "--plasmashellrc", "--theme"] {
        if let Some(v) = a.val(k) {
            extra.push(k.into());
            extra.push(v);
        }
    }
    let cases: &[(&str, &[&str])] = &[
        ("pad+full", &["--layout", "full", "--quick"]),
        ("pad+full+bands", &["--layout", "full", "--quick", "--bands"]),
        ("settings-open", &["--layout", "full", "--quick", "--sheet", "pointer"]),
        ("portrait", &["--layout", "full", "--quick", "--portrait"]),
        ("laptop+usb", &["--layout", "laptop", "--laptop", "--target", "To: USB device"]),
    ];
    let mut fonts = Fonts::load();
    println!("BENCH font={} ok={} scale={} iters={} alloc_count={}", fonts.path, fonts.ok(), scale, iters, cfg!(feature = "alloc-count"));
    let med = |v: &mut Vec<f64>| -> f64 {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[v.len() / 2]
    };
    let mut buf_px = 0usize;
    for (name, args) in cases {
        let mut argv: Vec<String> = vec!["bench".into()];
        argv.extend(args.iter().map(|s| s.to_string()));
        argv.extend(extra.iter().cloned());
        let sa = crate::debug::Args(argv);
        let (m, labels, _) = match debug_scene(&sa) {
            Ok(s) => s,
            Err(rc) => return rc,
        };
        let (bw, bh) = ((m.w * scale) as i32, (m.h * scale) as i32);
        buf_px = buf_px.max((bw * bh) as usize);
        let p = pal(&m);
        let cr = corner(&m);
        let pad = m.regions.iter().find(|r| r.id == dexhid_core::arrangement::TOUCHPAD).map(|r| r.rect).unwrap_or(Rect::new(0.0, 0.0, m.w, m.h));
        // model hand-off (daemon publish + UI take = 2 clones per input event)
        let mut tv = vec![];
        let a0 = crate::allocs();
        for _ in 0..iters {
            let t0 = Instant::now();
            let c = std::hint::black_box(m.clone());
            tv.push(t0.elapsed().as_secs_f64() * 1e3);
            drop(c);
        }
        let a1 = crate::allocs();
        println!("BENCH {:<15} model-clone      {:>8.3} ms  allocs={:>5} bytes={:>7} keys={} buttons={}", name, med(&mut tv), (a1.0 - a0.0) / iters as u64, (a1.1 - a0.1) / iters as u64, m.keys.len(), m.buttons.len());
        for style in ["m3", "m4"] {
            let m4 = style == "m4";
            let mut stat = vec![0u32; (bw * bh) as usize];
            let mut buf = vec![0u32; (bw * bh) as usize];
            // static layer
            let mut tv = vec![];
            let a0 = crate::allocs();
            for _ in 0..iters.min(12) {
                let t0 = Instant::now();
                let mut cv = Canvas { px: &mut stat, bw, bh, scale };
                if m4 {
                    crate::look::draw_static(&mut cv, &mut fonts, &m, &p, cr, labels.as_ref());
                } else {
                    draw_static(&mut cv, &mut fonts, &m, labels.as_ref());
                }
                tv.push(t0.elapsed().as_secs_f64() * 1e3);
            }
            let a1 = crate::allocs();
            let n_st = iters.min(12) as u64;
            println!("BENCH {:<15} {} static        {:>8.3} ms  allocs={:>5} bytes={:>7}", name, style, med(&mut tv), (a1.0 - a0.0) / n_st, (a1.1 - a0.1) / n_st);
            // full copy into a pool buffer (the first frame after any static change, once per buffer)
            let mut tv = vec![];
            for _ in 0..iters {
                let t0 = Instant::now();
                buf.copy_from_slice(&stat);
                tv.push(t0.elapsed().as_secs_f64() * 1e3);
            }
            let copy = med(&mut tv);
            println!("BENCH {:<15} {} full-copy     {:>8.3} ms  px={}", name, style, copy, bw * bh);
            // dynamic frames: restore the previous rects from the static layer, draw the new state
            for (what, keyed) in [("key-frame", true), ("dot-frame", false)] {
                let mut mm = m.clone();
                let mut prev: Vec<(i32, i32, i32, i32)> = vec![];
                let mut tv = vec![];
                let mut dmg = 0i64;
                let nk = mm.keys.len().max(1);
                let a0 = crate::allocs();
                for i in 0..iters {
                    if keyed {
                        mm.lit_keys = if mm.keys.is_empty() { vec![] } else { vec![(i * 7) % nk] };
                        mm.dots = vec![];
                    } else {
                        mm.lit_keys = vec![];
                        mm.dots = vec![(pad.x + pad.w * (0.2 + 0.6 * (i as f32 / iters as f32)), pad.y + pad.h * 0.5)];
                    }
                    let t0 = Instant::now();
                    let mut cv = Canvas { px: &mut buf, bw, bh, scale };
                    for r in &prev {
                        cv.copy_from(&stat, *r);
                    }
                    let d: Vec<(i32, i32, i32, i32)> = if m4 {
                        let mut lr = vec![];
                        crate::look::draw_dynamic(&mut cv, &mut fonts, &mm, &p, cr, labels.as_ref(), &mut lr);
                        lr.iter().map(|r| cv.damage_of(r)).collect()
                    } else {
                        draw_dynamic(&mut cv, &mut fonts, &mm, labels.as_ref())
                    };
                    tv.push(t0.elapsed().as_secs_f64() * 1e3);
                    dmg += d.iter().chain(prev.iter()).map(|r| (r.2 * r.3) as i64).sum::<i64>();
                    prev = d;
                }
                let a1 = crate::allocs();
                println!("BENCH {:<15} {} {:<13} {:>8.3} ms  allocs={:>5} bytes={:>7} damaged_px={}", name, style, what, med(&mut tv), (a1.0 - a0.0) / iters as u64, (a1.1 - a0.1) / iters as u64, dmg / iters as i64);
            }
            // shift / latch / Fn / info text: today a static change (model version bump) = static redraw + full copy
            let mut tv = vec![];
            for _ in 0..iters.min(12) {
                let t0 = Instant::now();
                let mut cv = Canvas { px: &mut stat, bw, bh, scale };
                if m4 {
                    crate::look::draw_static(&mut cv, &mut fonts, &m, &p, cr, labels.as_ref());
                } else {
                    draw_static(&mut cv, &mut fonts, &m, labels.as_ref());
                }
                buf.copy_from_slice(&stat);
                tv.push(t0.elapsed().as_secs_f64() * 1e3);
            }
            println!("BENCH {:<15} {} shift-frame   {:>8.3} ms  damaged_px={}", name, style, med(&mut tv), bw * bh);
        }
    }
    // prompt (full redraw on each press today)
    let (m, _, _) = match debug_scene(&crate::debug::Args(vec!["bench".into(), "--layout".into(), "full".into()])) {
        Ok(s) => s,
        Err(rc) => return rc,
    };
    let (bw, bh) = ((m.w * scale) as i32, (m.h * scale) as i32);
    let mut px = vec![0u32; (bw * bh) as usize];
    for style in ["m3", "m4"] {
        let mut tv = vec![];
        for _ in 0..iters.min(12) {
            let t0 = Instant::now();
            let mut cv = Canvas { px: &mut px, bw, bh, scale };
            cv.clear(render::SHADE);
            if style == "m4" {
                crate::look::draw_prompt(&mut cv, &mut fonts, &pal(&m), m.w, m.h, m.px_per_mm, None);
            } else {
                draw_prompt_m3(&mut cv, &mut fonts, UiKind::Prompt, m.w, m.h, m.px_per_mm, None);
            }
            tv.push(t0.elapsed().as_secs_f64() * 1e3);
        }
        println!("BENCH prompt          {} full-redraw   {:>8.3} ms  px={}", style, med(&mut tv), bw * bh);
    }
    // M4 app window (1696x1128 logical, the panel minus the Plasma panel): full redraw per page, the press redraw
    // of one button (damage = that rect), buffer memory (2 x window size while open, 0 while closed)
    {
        let (w, h, ppm) = (1696.0f32, 1128.0f32, 1696.0f32 / 274.0);
        let lays = debug_layouts(&crate::debug::Args(vec![]));
        let (bw, bh) = ((w * scale) as i32, (h * scale) as i32);
        let mut px = vec![0u32; (bw * bh) as usize];
        for page in ["home", "pointer", "usbhid"] {
            let v = crate::appwin::debug_view(page, lays.clone(), false);
            let p = feedback::palette("dark", "#4a8be0", 100);
            let mut tv = vec![];
            let a0 = crate::allocs();
            for _ in 0..iters.min(12) {
                let t0 = Instant::now();
                let mut cv = Canvas { px: &mut px, bw, bh, scale };
                crate::appwin::draw(&mut cv, &mut fonts, &p, w, h, ppm, &v, None);
                tv.push(t0.elapsed().as_secs_f64() * 1e3);
            }
            let a1 = crate::allocs();
            let n = iters.min(12) as u64;
            println!("BENCH window-{:<8} full-redraw   {:>8.3} ms  allocs={:>5} px={}", page, med(&mut tv), (a1.0 - a0.0) / n, bw * bh);
        }
        // one pressed button: redraw only its rect over the cached page (what the implementation will do)
        let v = crate::appwin::debug_view("home", lays.clone(), true);
        let p = feedback::palette("dark", "#4a8be0", 100);
        let l = crate::appwin::layout(w, h, ppm, &v);
        let br = l.hits.iter().find(|x| x.1 == crate::appwin::WinHit::Mode(1)).map(|x| x.0).unwrap_or(Rect::new(0.0, 0.0, 1.0, 1.0));
        let t = crate::look::Tokens::new(ppm, 1.0);
        let c = crate::look::Roles::from(&p);
        let mut tv = vec![];
        for _ in 0..iters {
            let t0 = Instant::now();
            let mut cv = Canvas { px: &mut px, bw, bh, scale };
            cv.rrect(&br, t.r_md, c.raised | 0xff00_0000);
            crate::look::button(&mut cv, &mut fonts, &t, &c, &br, "Start touchpad mode", Some(crate::look::Icon::Play), crate::look::Tone::Filled, true);
            tv.push(t0.elapsed().as_secs_f64() * 1e3);
        }
        println!("BENCH window-press     button-frame  {:>8.3} ms  damaged_px={}", med(&mut tv), ((br.w * scale) * (br.h * scale)) as i64);
        println!("BENCH window-memory    open: 2 buffers = {:.1} MiB (+ 1 cached page {:.1} MiB); closed: 0 (no surface, no pool, no timer)", 2.0 * (bw * bh) as f64 * 4.0 / 1048576.0, (bw * bh) as f64 * 4.0 / 1048576.0);
    }
    let (n, b) = fonts.cache_stats();
    println!("BENCH glyph-cache entries={} bytes={}", n, b);
    let st = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let get = |k: &str| st.lines().find(|l| l.starts_with(k)).map(|l| l.split_whitespace().nth(1).unwrap_or("0").to_string()).unwrap_or_default();
    let mb = |px: usize| px as f64 * 4.0 / 1048576.0;
    println!(
        "BENCH memory buffer={}px ({:.1} MiB) overlay holds static + 2 pool = {:.1} MiB; prompt/pill pool 2 = {:.1} MiB; bench VmRSS={} kB VmHWM={} kB",
        buf_px,
        mb(buf_px),
        3.0 * mb(buf_px),
        2.0 * mb(buf_px),
        get("VmRSS:"),
        get("VmHWM:")
    );
    0
}
