// SPDX-License-Identifier: GPL-3.0-or-later
//! The UI thread: one Wayland connection for the daemon's life. Main surface (one at a time) on the internal output:
//! - overlay: full-screen layer-shell overlay (exclusive zone -1, no keyboard focus) drawing the pad-mode model;
//!   input comes from the grabbed evdev device, not from Wayland;
//! - prompt: the "ask" card on a small centred layer surface (its buttons take wl_touch / wl_pointer, the rest of
//!   the screen stays usable);
//! - pill: a small "Use as pad" button at the top edge while the monitor is used as a plain display.
//!
//! Plus (M4) the app window: a normal xdg_toplevel, made on request (`OpenWindow`) and destroyed on close.
//! Fallback without zwlr_layer_shell_v1 (GNOME): xdg_toplevel fullscreen for the main surface.
//!
//! M4 drawing: no static copy of the screen. Each surface has two shm buffers; a frame repaints only the rects that
//! changed since that buffer was last painted (the scene drawn with a clip per rect), damage = the rects that changed
//! since the previous frame. Static changes are found by diffing per-element hashes (`look::scene_items`), touch
//! changes come as a small dynamic part (no model clone). The thread sleeps in poll() with no timeout: it wakes only
//! for Wayland events, the daemon's pokes (model, dynamic state, window, mirror frame) and heartbeat requests.

use crate::appwin::{self, WinHit, WinLayout, WinView};
use crate::look;
use crate::render::{BRect, Canvas, Fonts};
use crate::shared::{Cmd, DynState, SharedRef};
use crate::sys::*;
use dexhid_core::feedback;
use dexhid_core::geom::{Geometry, Rect, Transform};
use dexhid_core::keycodes;
use dexhid_core::layout::{Action, KeyKind, PlacedKey};
use dexhid_core::router::UiModel;
use dexhid_core::state::{Choice, UiKind};
use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::Ordering;
use wayland_client::backend::ObjectId;
use wayland_client::protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_keyboard, wl_output, wl_pointer, wl_region, wl_registry, wl_seat, wl_shm,
    wl_shm_pool, wl_surface, wl_touch,
};
use wayland_client::{delegate_noop, Connection, Dispatch, Proxy, QueueHandle, WEnum};
use wayland_protocols::wp::viewporter::client::{wp_viewport, wp_viewporter};
use wayland_protocols::xdg::decoration::zv1::client::{zxdg_decoration_manager_v1, zxdg_toplevel_decoration_v1};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

/// surface slots: the main surface (overlay / prompt / pill) and the app window
const MAIN: usize = 0;
const WIN: usize = 1;

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
    Down(usize, i32, f64, f64),
    Motion(usize, i32, f64, f64),
    Up(usize, i32),
}

struct St {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    layer: Option<zwlr_layer_shell_v1::ZwlrLayerShellV1>,
    wm: Option<xdg_wm_base::XdgWmBase>,
    viewporter: Option<wp_viewporter::WpViewporter>,
    deco: Option<zxdg_decoration_manager_v1::ZxdgDecorationManagerV1>,
    seat: Option<wl_seat::WlSeat>,
    touch: Option<wl_touch::WlTouch>,
    pointer: Option<wl_pointer::WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    outputs: Vec<OutInfo>,
    outputs_changed: bool,
    /// per slot: configured size, xdg toplevel size, closed, frame callback state
    configured: [Option<(u32, u32)>; 2],
    xdg_size: [(i32, i32); 2],
    closed: [bool; 2],
    frame_pending: [bool; 2],
    frame_done: [bool; 2],
    released: Vec<(u64, usize)>,
    keymap: Option<String>,
    input: Vec<In>,
    /// the surfaces' ids by slot (to route wl_touch / wl_pointer events)
    surf_ids: [Option<ObjectId>; 2],
    touch_slot: HashMap<i32, usize>,
    ptr_slot: Option<usize>,
    ptr_pos: (f64, f64),
}

impl St {
    fn slot_of(&self, s: &wl_surface::WlSurface) -> Option<usize> {
        let id = s.id();
        self.surf_ids.iter().position(|x| x.as_ref() == Some(&id))
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for St {
    fn event(st: &mut Self, reg: &wl_registry::WlRegistry, ev: wl_registry::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        match ev {
            wl_registry::Event::Global { name, interface, version } => match interface.as_str() {
                "wl_compositor" => st.compositor = Some(reg.bind(name, version.min(4), qh, ())),
                "wl_shm" => st.shm = Some(reg.bind(name, 1, qh, ())),
                "zwlr_layer_shell_v1" => st.layer = Some(reg.bind(name, version.min(4), qh, ())),
                "xdg_wm_base" => st.wm = Some(reg.bind(name, version.min(2), qh, ())),
                "wp_viewporter" => st.viewporter = Some(reg.bind(name, 1, qh, ())),
                "zxdg_decoration_manager_v1" => st.deco = Some(reg.bind(name, 1, qh, ())),
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
                st.configured[MAIN] = Some((width, height));
            }
            zwlr_layer_surface_v1::Event::Closed => st.closed[MAIN] = true,
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
impl Dispatch<xdg_surface::XdgSurface, usize> for St {
    fn event(st: &mut Self, xs: &xdg_surface::XdgSurface, ev: xdg_surface::Event, slot: &usize, _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_surface::Event::Configure { serial } = ev {
            xs.ack_configure(serial);
            let (w, h) = st.xdg_size[*slot];
            st.configured[*slot] = Some((w.max(0) as u32, h.max(0) as u32));
        }
    }
}
impl Dispatch<xdg_toplevel::XdgToplevel, usize> for St {
    fn event(st: &mut Self, _: &xdg_toplevel::XdgToplevel, ev: xdg_toplevel::Event, slot: &usize, _: &Connection, _: &QueueHandle<Self>) {
        match ev {
            xdg_toplevel::Event::Configure { width, height, .. } => st.xdg_size[*slot] = (width, height),
            xdg_toplevel::Event::Close => st.closed[*slot] = true,
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

impl Dispatch<wl_callback::WlCallback, usize> for St {
    fn event(st: &mut Self, _: &wl_callback::WlCallback, ev: wl_callback::Event, slot: &usize, _: &Connection, _: &QueueHandle<Self>) {
        if let wl_callback::Event::Done { .. } = ev {
            st.frame_pending[*slot] = false;
            st.frame_done[*slot] = true;
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
            wl_touch::Event::Down { id, x, y, surface, .. } => {
                if let Some(s) = st.slot_of(&surface) {
                    st.touch_slot.insert(id, s);
                    st.input.push(In::Down(s, id, x, y));
                }
            }
            wl_touch::Event::Motion { id, x, y, .. } => {
                if let Some(&s) = st.touch_slot.get(&id) {
                    st.input.push(In::Motion(s, id, x, y));
                }
            }
            wl_touch::Event::Up { id, .. } => {
                if let Some(s) = st.touch_slot.remove(&id) {
                    st.input.push(In::Up(s, id));
                }
            }
            wl_touch::Event::Cancel => {
                for s in [MAIN, WIN] {
                    st.input.push(In::Up(s, -100));
                }
                st.touch_slot.clear();
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for St {
    fn event(st: &mut Self, _: &wl_pointer::WlPointer, ev: wl_pointer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match ev {
            wl_pointer::Event::Enter { surface, surface_x, surface_y, .. } => {
                st.ptr_slot = st.slot_of(&surface);
                st.ptr_pos = (surface_x, surface_y);
                if let Some(s) = st.ptr_slot {
                    st.input.push(In::Motion(s, -1, surface_x, surface_y));
                }
            }
            wl_pointer::Event::Leave { .. } => {
                if let Some(s) = st.ptr_slot.take() {
                    st.input.push(In::Up(s, -1));
                }
            }
            wl_pointer::Event::Motion { surface_x, surface_y, .. } => {
                st.ptr_pos = (surface_x, surface_y);
                if let Some(s) = st.ptr_slot {
                    st.input.push(In::Motion(s, -1, surface_x, surface_y));
                }
            }
            wl_pointer::Event::Button { state: WEnum::Value(b), .. } => {
                if let Some(s) = st.ptr_slot {
                    if b == wl_pointer::ButtonState::Pressed {
                        st.input.push(In::Down(s, -1, st.ptr_pos.0, st.ptr_pos.1));
                    } else {
                        st.input.push(In::Up(s, -1));
                    }
                }
            }
            wl_pointer::Event::Axis { axis: WEnum::Value(wl_pointer::Axis::VerticalScroll), value, .. } => {
                // the window scrolls with a mouse wheel too: a synthetic drag
                if st.ptr_slot == Some(WIN) {
                    let (x, y) = st.ptr_pos;
                    st.input.push(In::Down(WIN, -2, x, y));
                    st.input.push(In::Motion(WIN, -2, x, y - value * 4.0));
                    st.input.push(In::Up(WIN, -3));
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
delegate_noop!(St: ignore wp_viewporter::WpViewporter);
delegate_noop!(St: ignore wp_viewport::WpViewport);
delegate_noop!(St: ignore zxdg_decoration_manager_v1::ZxdgDecorationManagerV1);
delegate_noop!(St: ignore zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1);

// ------------------------------------------------------------------ buffers + partial painting

struct Buf {
    wl: wl_buffer::WlBuffer,
    off: usize,
    busy: bool,
    /// rects (buffer px) changed in the scene since this buffer was last painted
    pending: Vec<BRect>,
    /// the whole buffer is stale
    full: bool,
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
        let bsize = (bw.max(1) * bh.max(1) * 4) as usize;
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
            .map(|i| Buf { wl: pool.create_buffer((i * bsize) as i32, bw, bh, bw * 4, fmt, qh, (gen, i)), off: i * bsize / 4, busy: false, pending: vec![], full: true })
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

/// the buffer rect a logical rect may paint (anti-aliasing margin), clamped to the buffer
pub fn brect_of(r: &Rect, scale: f32, bw: i32, bh: i32) -> BRect {
    let x0 = (((r.x - 2.0) * scale).floor() as i32).clamp(0, bw);
    let y0 = (((r.y - 2.0) * scale).floor() as i32).clamp(0, bh);
    let x1 = (((r.right() + 2.0) * scale).ceil() as i32).clamp(0, bw);
    let y1 = (((r.bottom() + 2.0) * scale).ceil() as i32).clamp(0, bh);
    (x0, y0, (x1 - x0).max(0), (y1 - y0).max(0))
}

/// Paint one buffer: the stale rects (`pending`, or everything when `full` or when they cover most of it) are
/// repainted by drawing the scene with a clip per rect. Returns the painted pixel count.
pub fn paint(px: &mut [u32], bw: i32, bh: i32, scale: f32, pending: &mut Vec<BRect>, full: &mut bool, mut draw: impl FnMut(&mut Canvas)) -> i64 {
    let total = bw as i64 * bh as i64;
    pending.retain(|r| r.2 > 0 && r.3 > 0);
    pending.sort_unstable();
    pending.dedup();
    let area: i64 = pending.iter().map(|r| r.2 as i64 * r.3 as i64).sum();
    let mut cv = Canvas::new(px, bw, bh, scale);
    let painted = if *full || pending.len() > 48 || area * 10 > total * 6 {
        draw(&mut cv);
        total
    } else {
        for r in pending.iter() {
            cv.set_clip(Some(*r));
            draw(&mut cv);
        }
        area
    };
    pending.clear();
    *full = false;
    painted
}

/// pressed state of the prompt / pill buttons
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

struct Surf {
    slot: usize,
    kind: UiKind,
    /// the daemon's surface epoch (bumps on every kind change, so overlay -> pill -> overlay while this thread was
    /// busy still makes a fresh surface and a fresh UI-MAPPED)
    epoch: u64,
    surface: wl_surface::WlSurface,
    layer: Option<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1>,
    xdg: Option<(xdg_surface::XdgSurface, xdg_toplevel::XdgToplevel, Option<zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1>)>,
    viewport: Option<wp_viewport::WpViewport>,
    /// logical size, output scale, render scale (buffer px per logical px)
    w: i32,
    h: i32,
    scale: i32,
    rs: f32,
    pool: Option<Pool>,
    mapped: bool,
    /// where the surface's top-left sits on the screen (prompt / pill: small centred surfaces)
    origin: (f32, f32),
    /// the screen it was laid out for (logical)
    screen: (f32, f32),
    /// prompt / pill buttons (surface coordinates) for hit tests
    hits: Vec<(Rect, Hit)>,
    pressed: HashMap<i32, Hit>,
    pos: HashMap<i32, (f64, f64)>,
    lit: Option<Hit>,
    /// logical rects changed since the last frame, or the whole surface
    dirty: Vec<Rect>,
    dirty_full: bool,
    first: bool,
}

impl Drop for Surf {
    fn drop(&mut self) {
        self.pool = None;
        if let Some(v) = self.viewport.take() {
            v.destroy();
        }
        if let Some(l) = self.layer.take() {
            l.destroy();
        }
        if let Some((xs, tl, d)) = self.xdg.take() {
            if let Some(d) = d {
                d.destroy();
            }
            tl.destroy();
            xs.destroy();
        }
        self.surface.destroy();
    }
}

impl Surf {
    fn new(slot: usize, kind: UiKind, epoch: u64, surface: wl_surface::WlSurface, scale: i32) -> Surf {
        Surf {
            slot,
            kind,
            epoch,
            surface,
            layer: None,
            xdg: None,
            viewport: None,
            w: 0,
            h: 0,
            scale,
            rs: scale as f32,
            pool: None,
            mapped: false,
            origin: (0.0, 0.0),
            screen: (0.0, 0.0),
            hits: vec![],
            pressed: HashMap::new(),
            pos: HashMap::new(),
            lit: None,
            dirty: vec![],
            dirty_full: true,
            first: true,
        }
    }

    fn mark(&mut self, r: Rect) {
        if !self.dirty_full {
            if self.dirty.len() > 64 {
                self.dirty_full = true;
                self.dirty.clear();
            } else {
                self.dirty.push(r);
            }
        }
    }

    fn mark_all(&mut self, v: &[Rect]) {
        for r in v {
            self.mark(*r);
        }
    }

    fn wants_frame(&self) -> bool {
        self.dirty_full || !self.dirty.is_empty()
    }

    /// Render a frame if one is due and a buffer is free: every buffer learns the new dirty rects, the free one is
    /// repainted where stale, attached with damage = this frame's rects. Err = shm allocation failed.
    #[allow(clippy::too_many_arguments)]
    fn frame(&mut self, st: &mut St, shm: &wl_shm::WlShm, qh: &QueueHandle<St>, pool_gen: &mut u64, argb: bool, draw: impl FnMut(&mut Canvas)) -> Result<bool, String> {
        if self.w <= 0 || self.h <= 0 || st.frame_pending[self.slot] || !self.wants_frame() {
            return Ok(false);
        }
        let (bw, bh) = (((self.w as f32) * self.rs).round().max(1.0) as i32, ((self.h as f32) * self.rs).round().max(1.0) as i32);
        if self.pool.as_ref().is_none_or(|p| (p.bw, p.bh) != (bw, bh)) {
            self.pool = None;
            *pool_gen += 1;
            self.pool = Pool::new(shm, bw, bh, argb, *pool_gen, qh);
            if self.pool.is_none() {
                return Err("shm buffer allocation failed".into());
            }
            self.dirty_full = true;
            self.first = true;
        }
        let rs = self.rs;
        let pool = self.pool.as_mut().unwrap();
        let Some(bi) = pool.bufs.iter().position(|b| !b.busy) else { return Ok(false) };
        let full = self.dirty_full;
        let damage: Vec<BRect> = if full { vec![(0, 0, bw, bh)] } else { self.dirty.iter().map(|r| brect_of(r, rs, bw, bh)).collect() };
        for b in pool.bufs.iter_mut() {
            if full {
                b.full = true;
                b.pending.clear();
            } else {
                b.pending.extend(damage.iter().copied());
            }
        }
        let mut pending = std::mem::take(&mut pool.bufs[bi].pending);
        let mut bfull = pool.bufs[bi].full;
        let px = pool.px(bi);
        paint(px, bw, bh, rs, &mut pending, &mut bfull, draw);
        pool.bufs[bi].pending = pending;
        pool.bufs[bi].full = false;
        self.surface.attach(Some(&pool.bufs[bi].wl), 0, 0);
        if self.first {
            self.surface.damage_buffer(0, 0, bw, bh);
        } else {
            for (x, y, w, h) in damage {
                self.surface.damage_buffer(x, y, w, h);
            }
        }
        self.first = false;
        self.surface.frame(qh, self.slot);
        st.frame_pending[self.slot] = true;
        self.surface.commit();
        pool.bufs[bi].busy = true;
        self.dirty.clear();
        self.dirty_full = false;
        Ok(true)
    }
}

/// the app window's thread-side state
struct Win {
    s: Surf,
    view: WinView,
    layout: WinLayout,
    ppm: f32,
    /// touch / pointer id -> (hit, start, scroll at start, scrolling)
    touches: HashMap<i32, (Option<WinHit>, (f64, f64), f32, bool)>,
    lit: Option<WinHit>,
}

impl Win {
    fn relayout(&mut self) {
        self.layout = appwin::layout(self.s.w as f32, self.s.h as f32, self.ppm, &self.view);
    }
    fn hit(&self, x: f64, y: f64) -> Option<WinHit> {
        self.layout.hits.iter().find(|(r, _)| r.contains(x as f32, y as f32)).map(|(_, h)| *h)
    }
    fn hit_rect(&self, h: WinHit) -> Option<Rect> {
        self.layout.hits.iter().find(|(_, x)| *x == h).map(|(r, _)| *r)
    }
}

// ------------------------------------------------------------------ labels

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

// ------------------------------------------------------------------ thread

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
        viewporter: None,
        deco: None,
        seat: None,
        touch: None,
        pointer: None,
        keyboard: None,
        outputs: vec![],
        outputs_changed: true,
        configured: [None, None],
        xdg_size: [(0, 0), (0, 0)],
        closed: [false, false],
        frame_pending: [false, false],
        frame_done: [false, false],
        released: vec![],
        keymap: None,
        input: vec![],
        surf_ids: [None, None],
        touch_slot: HashMap::new(),
        ptr_slot: None,
        ptr_pos: (0.0, 0.0),
    };
    for _ in 0..3 {
        if q.roundtrip(&mut st).is_err() {
            return lost("wayland roundtrip".into());
        }
    }
    println!(
        "UI-GLOBALS layer_shell={} xdg_wm_base={} seat={} outputs={} viewporter={} decoration={}",
        st.layer.is_some(),
        st.wm.is_some(),
        st.seat.is_some(),
        st.outputs.iter().map(|o| format!("{}:{}x{}@{}:t{}", o.name, o.mode.0, o.mode.1, o.scale, o.transform)).collect::<Vec<_>>().join(","),
        st.viewporter.is_some(),
        st.deco.is_some()
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
    let mut win: Option<Win> = None;
    let mut want = (UiKind::None, UiModel::default(), 0u64);
    let mut seen_ver = u64::MAX;
    let mut seen_dyn = u64::MAX;
    let mut seen_win = u64::MAX;
    let mut items: Vec<(Rect, u64)> = vec![];
    let mut items_new: Vec<(Rect, u64)> = vec![];
    let mut items_gen = 0u64;
    let mut dyn_old: Vec<Rect> = vec![];
    let mut dyn_new: Vec<Rect> = vec![];
    let mut dyn_tmp = DynState::default();
    let mut pool_gen = 0u64;
    let mut last_geo: Option<Geometry> = None;
    let codes = keycodes::all_codes();
    let mut last_legends = String::new();
    // M3: legends of another device's keyboard language; the labels published for the daemon
    let mut over_name: Option<String> = None;
    let mut published_gen = 0u64;
    let mut mirror_seen = 0u64;
    let mut half = sh.half_res.load(Ordering::SeqCst);

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
        if sh.hb_req.swap(false, Ordering::SeqCst) {
            poke(sh.wake_core);
        }
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
        }
        if published_gen != labels_gen {
            published_gen = labels_gen;
            *sh.labels.lock().unwrap() = labels.as_ref().map(|l| l.map.clone()).unwrap_or_default();
        }
        // what should be shown: the whole model on a static change, the dynamic part on a touch change
        let v = sh.ui_ver.load(Ordering::SeqCst);
        let static_changed = v != seen_ver;
        if static_changed {
            seen_ver = v;
            want = sh.ui.lock().unwrap().clone();
        }
        let dv = sh.dyn_ver.load(Ordering::SeqCst);
        if dv != seen_dyn || static_changed {
            seen_dyn = dv;
            dyn_tmp.clone_from(&sh.dyn_ui.lock().unwrap());
            dyn_tmp.copy_into_model(&mut want.1);
            dyn_new.clear();
            look::dyn_rects(&want.1, &mut dyn_new);
            if let Some(s) = surf.as_mut().filter(|s| s.kind == UiKind::Overlay) {
                s.mark_all(&dyn_old);
                s.mark_all(&dyn_new);
            }
            std::mem::swap(&mut dyn_old, &mut dyn_new);
        }
        // static diff: per-element hashes (layout, legends, info text, sheet values ...)
        if static_changed || items_gen != labels_gen {
            items_gen = labels_gen;
            look::scene_items(&want.1, labels.as_ref(), &mut items_new);
            if let Some(s) = surf.as_mut().filter(|s| s.kind == UiKind::Overlay) {
                match look::diff_items(&items, &items_new) {
                    Some(rects) => s.mark_all(&rects),
                    None => s.dirty_full = true,
                }
            }
            std::mem::swap(&mut items, &mut items_new);
            if sh.debug && want.0 == UiKind::Overlay {
                let l = legends_line(&want.1, labels.as_ref());
                if l != last_legends {
                    println!("{} t={}", l, mono_ms());
                    last_legends = l;
                }
            }
            if let Some(s) = surf.as_mut().filter(|s| s.kind != UiKind::Overlay) {
                s.dirty_full = true;
            }
        }
        let mg = sh.mirror_gen.load(Ordering::SeqCst);
        if mg != mirror_seen {
            mirror_seen = mg;
            if let (Some(s), Some(r)) = (surf.as_mut().filter(|s| s.kind == UiKind::Overlay), want.1.mirror) {
                s.mark(r);
            }
        }
        let hr = sh.half_res.load(Ordering::SeqCst);
        if hr != half {
            half = hr;
            if let Some(s) = surf.as_mut() {
                s.rs = if half && st.viewporter.is_some() { s.scale as f32 * 0.5 } else { s.scale as f32 };
                s.dirty_full = true;
            }
        }
        let cur_kind = surf.as_ref().map(|s| s.kind).unwrap_or(UiKind::None);
        let cur_epoch = surf.as_ref().map(|s| s.epoch);
        if cur_kind != want.0 || (want.0 != UiKind::None && cur_epoch != Some(want.2)) {
            surf = None; // drop destroys
            st.surf_ids[MAIN] = None;
            st.configured[MAIN] = None;
            st.closed[MAIN] = false;
            st.frame_pending[MAIN] = false;
            st.frame_done[MAIN] = false;
            st.input.retain(|e| !matches!(e, In::Down(MAIN, ..) | In::Motion(MAIN, ..) | In::Up(MAIN, _)));
            if want.0 != UiKind::None {
                let Some(oi) = internal(&st.outputs) else {
                    lost("no output to show the UI on".into());
                    return;
                };
                let out = st.outputs[oi].proxy.clone().unwrap();
                let s = comp.create_surface(&qh, ());
                st.surf_ids[MAIN] = Some(s.id());
                let scale = st.outputs[oi].scale.max(1);
                let (lw, lh) = logical_size(&st.outputs[oi]);
                let ppm = ppm_of(&st.outputs[oi], &cfg);
                let mut sf = Surf::new(MAIN, want.0, want.2, s.clone(), scale);
                sf.screen = (lw as f32, lh as f32);
                if let Some(ls) = &st.layer {
                    use zwlr_layer_surface_v1::Anchor;
                    let l = ls.get_layer_surface(&s, Some(&out), zwlr_layer_shell_v1::Layer::Overlay, "dexhid".into(), &qh, ());
                    l.set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::None);
                    l.set_exclusive_zone(-1);
                    match want.0 {
                        UiKind::Pill => {
                            let ((sw, sh_), origin) = look::pill_surface(lw as f32, ppm);
                            l.set_anchor(Anchor::Top);
                            l.set_size(sw as u32, sh_ as u32);
                            sf.origin = origin;
                        }
                        UiKind::Prompt => {
                            // a small centred surface: the card + its shadow (no anchor = centred)
                            let ((sw, sh_), origin) = look::prompt_surface(lw as f32, lh as f32, ppm);
                            l.set_size(sw as u32, sh_ as u32);
                            sf.origin = origin;
                        }
                        _ => {
                            l.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
                            l.set_size(0, 0);
                        }
                    }
                    sf.layer = Some(l);
                } else if let Some(wm) = &st.wm {
                    let xs = wm.get_xdg_surface(&s, &qh, MAIN);
                    let tl = xs.get_toplevel(&qh, MAIN);
                    tl.set_title("DexHid".into());
                    tl.set_app_id("org.dexhid.DexHid".into());
                    tl.set_fullscreen(Some(&out));
                    sf.xdg = Some((xs, tl, None));
                }
                if let Some(vp) = &st.viewporter {
                    sf.viewport = Some(vp.get_viewport(&s, &qh, ()));
                    sf.rs = if half && want.0 == UiKind::Overlay { scale as f32 * 0.5 } else { scale as f32 };
                } else {
                    s.set_buffer_scale(scale);
                }
                s.commit();
                println!("UI-SURFACE kind={} via={} output={} scale={} render_scale={}", want.0.name(), if sf.layer.is_some() { "layer-shell" } else { "xdg-fullscreen" }, st.outputs[oi].name, scale, sf.rs);
                surf = Some(sf);
            }
        }
        if st.closed[MAIN] && surf.is_some() {
            lost("surface closed by the compositor".into());
            return;
        }
        // configure -> size, geometry
        if let (Some(s), Some((w, h))) = (surf.as_mut(), st.configured[MAIN]) {
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
                if s.kind == UiKind::Overlay {
                    let t = Transform::from_name(&cfg.touch_transform).unwrap_or(Transform::from_wl(o.transform));
                    let g = Geometry { w: w as f32, h: h as f32, transform: t, native_mm: cfg.size_mm, tmax: cfg.tmax };
                    if last_geo != Some(g) {
                        last_geo = Some(g);
                        println!("UI-GEOMETRY w={} h={} transform={} ppm={:.2} output={}", w, h, t.name(), g.px_per_mm(), o.name);
                        sh.send(Cmd::Geometry(g));
                    }
                }
                if s.layer.is_none() {
                    s.screen = (w as f32, h as f32);
                }
            }
            if (w, h) != (s.w, s.h) {
                s.w = w;
                s.h = h;
                if let Some(vp) = &s.viewport {
                    vp.set_destination(w.max(1), h.max(1));
                }
                s.dirty_full = true;
            }
        }
        // the app window: open on request, follow the daemon's view
        if let Some(page) = sh.win_req.lock().unwrap().take() {
            if win.is_none() {
                if let (Some(wm), Some(oi)) = (st.wm.as_ref(), internal(&st.outputs)) {
                    let s = comp.create_surface(&qh, ());
                    st.surf_ids[WIN] = Some(s.id());
                    let scale = st.outputs[oi].scale.max(1);
                    let xs = wm.get_xdg_surface(&s, &qh, WIN);
                    let tl = xs.get_toplevel(&qh, WIN);
                    tl.set_title("DexHid".into());
                    tl.set_app_id("org.dexhid.DexHid".into());
                    tl.set_min_size(320, 400);
                    let d = st.deco.as_ref().map(|dm| {
                        let d = dm.get_toplevel_decoration(&tl, &qh, ());
                        d.set_mode(zxdg_toplevel_decoration_v1::Mode::ServerSide);
                        d
                    });
                    let mut sf = Surf::new(WIN, UiKind::None, 0, s.clone(), scale);
                    sf.xdg = Some((xs, tl, d));
                    if let Some(vp) = &st.viewporter {
                        sf.viewport = Some(vp.get_viewport(&s, &qh, ()));
                    } else {
                        s.set_buffer_scale(scale);
                    }
                    let (lw, lh) = logical_size(&st.outputs[oi]);
                    sf.screen = (lw as f32, lh as f32);
                    st.configured[WIN] = None;
                    st.closed[WIN] = false;
                    st.frame_pending[WIN] = false;
                    s.commit();
                    let ppm = ppm_of(&st.outputs[oi], &cfg);
                    let view = sh.win.lock().unwrap().clone().unwrap_or_default();
                    seen_win = sh.win_ver.load(Ordering::SeqCst);
                    let mut w = Win { s: sf, view, layout: WinLayout::default(), ppm, touches: HashMap::new(), lit: None };
                    let p = w.view.page_of(&page);
                    w.view.select(p);
                    println!("UI-WINDOW t={} open page={} scale={}", mono_ms(), page, scale);
                    win = Some(w);
                } else {
                    println!("UI-WINDOW no xdg_wm_base / output: cannot open the window");
                    sh.send(Cmd::WindowClosed);
                }
            } else if let Some(w) = win.as_mut() {
                let p = w.view.page_of(&page);
                w.view.select(p);
                w.view.scroll = 0.0;
                w.relayout();
                w.s.dirty_full = true;
            }
        }
        // closed by request (the daemon dropped the view): the window goes with everything it holds
        if win.is_some() && sh.win_ver.load(Ordering::SeqCst) != seen_win && sh.win.lock().unwrap().is_none() {
            seen_win = sh.win_ver.load(Ordering::SeqCst);
            win = None;
            st.surf_ids[WIN] = None;
            st.frame_pending[WIN] = false;
            println!("UI-WINDOW t={} closed by request", mono_ms());
        }
        if let Some(w) = win.as_mut() {
            let wv = sh.win_ver.load(Ordering::SeqCst);
            if wv != seen_win {
                seen_win = wv;
                if let Some(nv) = sh.win.lock().unwrap().clone() {
                    let (page, scroll) = (w.view.page, w.view.scroll);
                    let mut nv = nv;
                    nv.select(page);
                    nv.scroll = scroll;
                    match appwin::diff(&w.view, &nv, &w.layout) {
                        Some(rects) => w.s.mark_all(&rects),
                        None => w.s.dirty_full = true,
                    }
                    w.view = nv;
                    w.relayout();
                }
            }
            if let Some((cw, ch)) = st.configured[WIN].take() {
                let (dw, dh) = ((w.s.screen.0 * 0.66).max(360.0) as i32, (w.s.screen.1 * 0.8).max(480.0) as i32);
                let (cw, ch) = (if cw == 0 { dw } else { cw as i32 }, if ch == 0 { dh } else { ch as i32 });
                if (cw, ch) != (w.s.w, w.s.h) {
                    w.s.w = cw;
                    w.s.h = ch;
                    if let Some(vp) = &w.s.viewport {
                        vp.set_destination(cw.max(1), ch.max(1));
                    }
                    w.relayout();
                    w.s.dirty_full = true;
                }
            }
        }
        if st.closed[WIN] && win.is_some() {
            st.closed[WIN] = false;
            win = None;
            st.surf_ids[WIN] = None;
            println!("UI-WINDOW t={} closed", mono_ms());
            sh.send(Cmd::WindowClosed);
        }
        // released buffers
        for (g, i) in st.released.drain(..) {
            for p in [surf.as_mut().map(|s| &mut s.pool), win.as_mut().map(|w| &mut w.s.pool)].into_iter().flatten().flatten() {
                if p.gen == g && i < p.bufs.len() {
                    p.bufs[i].busy = false;
                }
            }
        }
        // input: prompt / pill buttons, the window
        let evs: Vec<In> = st.input.drain(..).collect();
        for e in evs {
            match e {
                In::Down(MAIN, ..) | In::Motion(MAIN, ..) | In::Up(MAIN, _) => {
                    let Some(s) = surf.as_mut().filter(|s| matches!(s.kind, UiKind::Prompt | UiKind::Pill)) else { continue };
                    let hit = |s: &Surf, x: f64, y: f64| s.hits.iter().find(|(r, _)| r.contains(x as f32, y as f32)).map(|(_, h)| *h);
                    match e {
                        In::Down(_, id, x, y) => {
                            s.pos.insert(id, (x, y));
                            if let Some(h) = hit(s, x, y) {
                                s.pressed.insert(id, h);
                                s.lit = Some(h);
                                s.dirty_full = true;
                            }
                        }
                        In::Motion(_, id, x, y) => {
                            s.pos.insert(id, (x, y));
                        }
                        In::Up(_, id) => {
                            let cancel = id == -100;
                            let ids: Vec<i32> = if cancel { s.pressed.keys().copied().collect() } else { vec![id] };
                            for id in ids {
                                if let Some(h) = s.pressed.remove(&id) {
                                    let p = s.pos.remove(&id).unwrap_or((-1.0, -1.0));
                                    let fire = !cancel && hit(s, p.0, p.1) == Some(h);
                                    println!("UI-TAP t={} target={} fire={}", mono_ms(), h.name(), fire);
                                    if fire {
                                        match h {
                                            Hit::Choice(c) => sh.send(Cmd::Prompt(c)),
                                            Hit::PillPad => sh.send(Cmd::PillUsePad),
                                            Hit::PillHide => sh.send(Cmd::PillHide),
                                        }
                                    }
                                    s.lit = None;
                                    s.dirty_full = true;
                                }
                            }
                        }
                    }
                }
                _ => {
                    if let Some(w) = win.as_mut() {
                        win_input(w, e, &sh);
                    }
                }
            }
        }
        // draw the main surface
        if let Some(s) = surf.as_mut() {
            let model = &want.1;
            let ppm = if model.px_per_mm > 0.0 { model.px_per_mm } else { s.w as f32 / if s.w >= s.h { cfg.size_mm.0.max(cfg.size_mm.1) } else { cfg.size_mm.0.min(cfg.size_mm.1) } };
            let p = look::palette_of(model);
            let kind = s.kind;
            let (screen, origin, lit) = (s.screen, s.origin, s.lit);
            let mut hits_out: Option<(Rect, Vec<(Rect, Hit)>)> = None;
            let r = match kind {
                UiKind::Overlay => {
                    let corner = look::corner_of(model);
                    let fr = if model.mirror.is_some() { Some(sh.mirror.lock().unwrap()) } else { None };
                    let mframe = fr.as_ref().and_then(|g| g.as_ref());
                    s.frame(&mut st, &shm, &qh, &mut pool_gen, false, |cv| look::draw_scene(cv, &mut fonts, model, &p, corner, labels.as_ref(), mframe))
                }
                UiKind::Prompt | UiKind::Pill => s.frame(&mut st, &shm, &qh, &mut pool_gen, true, |cv| {
                    cv.clear(0);
                    hits_out = Some(if kind == UiKind::Prompt {
                        look::draw_prompt(cv, &mut fonts, &p, screen.0, screen.1, ppm, lit, origin)
                    } else {
                        look::draw_pill(cv, &mut fonts, &p, screen.0, ppm, lit, origin)
                    });
                }),
                UiKind::None => Ok(false),
            };
            match r {
                Err(e) => return lost(e),
                Ok(true) => {
                    if let Some((frame, hits)) = hits_out {
                        s.hits = hits;
                        // only the card / pill takes input; the rest of the screen stays usable
                        let reg = comp.create_region(&qh, ());
                        let (x, y, w, h) = frame.round();
                        reg.add(x, y, w, h);
                        s.surface.set_input_region(Some(&reg));
                        reg.destroy();
                        s.surface.commit();
                    }
                }
                Ok(false) => {}
            }
            if st.frame_done[MAIN] && !s.mapped {
                s.mapped = true;
                println!("UI-MAPPED t={} kind={} size={}x{} scale={}", mono_ms(), s.kind.name(), s.w, s.h, s.scale);
                if matches!(s.kind, UiKind::Prompt | UiKind::Pill) {
                    if let Some(oi) = internal(&st.outputs) {
                        let o = &st.outputs[oi];
                        let (lw, lh) = logical_size(o);
                        let t = Transform::from_name(&cfg.touch_transform).unwrap_or(Transform::from_wl(o.transform));
                        let g = Geometry { w: lw as f32, h: lh as f32, transform: t, native_mm: cfg.size_mm, tmax: cfg.tmax };
                        for (r, h) in &s.hits {
                            // hits are surface-local; the runner taps screen coordinates
                            let sr = Rect::new(r.x + s.origin.0, r.y + s.origin.1, r.w, r.h);
                            let (x0, y0, x1, y1) = g.rect_to_touch(&sr);
                            println!("GEOM name={} native={},{},{},{} logical={:.0},{:.0},{:.0},{:.0}", h.name(), x0, y0, x1, y1, sr.x, sr.y, sr.w, sr.h);
                        }
                    }
                }
                sh.send(Cmd::Mapped(s.kind, s.epoch));
            }
        }
        // draw the window
        if let Some(w) = win.as_mut() {
            let (ww, wh, ppm, lit) = (w.s.w as f32, w.s.h as f32, w.ppm, w.lit);
            let view = &w.view;
            let r = w.s.frame(&mut st, &shm, &qh, &mut pool_gen, false, |cv| {
                appwin::draw(cv, &mut fonts, &view.palette, ww, wh, ppm, view, lit);
            });
            if let Err(e) = r {
                println!("UI-WINDOW {}: closing the window", e);
                win = None;
                st.surf_ids[WIN] = None;
                sh.send(Cmd::WindowClosed);
            }
        }
        // wayland + wake poll (no timeout: every wake-up has a reason)
        let dispatched = match q.dispatch_pending(&mut st) {
            Ok(n) => n,
            Err(_) => return lost("wayland dispatch error".into()),
        };
        if let Err(e) = q.flush() {
            if !matches!(&e, wayland_client::backend::WaylandError::Io(io) if io.kind() == std::io::ErrorKind::WouldBlock) {
                return lost(format!("wayland flush: {}", e));
            }
        }
        // events just dispatched (configure, frame done, buffer release, input) are handled at the top of the loop
        // before sleeping
        if dispatched > 0 {
            continue;
        }
        let Some(guard) = q.prepare_read() else { continue };
        let wlfd = guard.connection_fd().as_raw_fd();
        let r = poll_in(&[wlfd, wake_rd], -1);
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

/// the window's touch / pointer input: taps act on release over the same target; a vertical drag scrolls
fn win_input(w: &mut Win, e: In, sh: &SharedRef) {
    let slop = (w.ppm * 3.0) as f64;
    match e {
        In::Down(_, id, x, y) => {
            let h = w.hit(x, y);
            w.touches.insert(id, (h, (x, y), w.view.scroll, false));
            if let Some(h) = h {
                w.lit = Some(h);
                if let Some(r) = w.hit_rect(h) {
                    w.s.mark(r);
                }
            }
        }
        In::Motion(_, id, _x, y) => {
            let max = w.layout.max_scroll(w.s.h as f32, w.ppm);
            let Some(t) = w.touches.get_mut(&id) else { return };
            let dy = y - t.1 .1;
            if !t.3 && (dy.abs() > slop || id == -2) && max > 0.0 {
                t.3 = true;
                if w.lit.take().is_some() {
                    w.s.dirty_full = true;
                }
            }
            if t.3 {
                let s = (t.2 - dy as f32).clamp(0.0, max);
                if (s - w.view.scroll).abs() > 0.5 {
                    w.view.scroll = s;
                    w.relayout();
                    w.s.dirty_full = true;
                }
            }
        }
        In::Up(_, id) => {
            let ids: Vec<i32> = if id == -100 { w.touches.keys().copied().collect() } else if id == -3 { vec![-2] } else { vec![id] };
            for id in ids {
                let Some((h, start, _, scrolling)) = w.touches.remove(&id) else { continue };
                if let Some(r) = w.lit.take().and_then(|l| w.hit_rect(l)) {
                    w.s.mark(r);
                }
                let Some(h) = h.filter(|_| !scrolling && id != -100) else { continue };
                if w.hit(start.0, start.1) != Some(h) {
                    continue;
                }
                println!("UI-WINDOW-TAP t={} target={:?}", mono_ms(), h);
                match h {
                    WinHit::Nav(i) => {
                        w.view.select(i as usize);
                        w.view.scroll = 0.0;
                        w.relayout();
                        w.s.dirty_full = true;
                    }
                    WinHit::Mode(i) => {
                        if let Some(m) = dexhid_core::modes::Mode::from_index(i as usize) {
                            sh.send(Cmd::SetMode(m.name().into(), None));
                        }
                    }
                    WinHit::Layout(i) => {
                        if let Some((name, _)) = w.view.layouts.get(i as usize) {
                            sh.send(Cmd::SetLayout(name.clone()));
                        }
                    }
                    WinHit::RowToggle(i) | WinHit::RowChoice(i) | WinHit::RowPlus(i) | WinHit::RowMinus(i) => {
                        if let Some(r) = w.view.rows.get(i as usize) {
                            let dir = if matches!(h, WinHit::RowMinus(_)) { -1 } else { 1 };
                            sh.send(Cmd::WinSetting(r.key.clone(), dir));
                        }
                    }
                }
            }
        }
    }
}

// ------------------------------------------------------------------ debug render + bench

/// `dexhid debug render ...` (PC check of layouts, legends, the look; see render_debug)
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

/// a stand-in desktop behind the prompt / pill renders (they are translucent layer surfaces)
fn mock_desktop(cv: &mut Canvas, w: f32, h: f32) {
    for y in 0..cv.bh {
        let t = y as f32 / cv.bh.max(1) as f32;
        let c = feedback::mix(0xff2a3550, 0xff141821, t);
        cv.px[(y * cv.bw) as usize..((y + 1) * cv.bw) as usize].fill(c);
    }
    cv.rrect(&Rect::new(w * 0.08, h * 0.1, w * 0.5, h * 0.55), 10.0, 0xff1e232b);
    cv.rrect(&Rect::new(w * 0.08, h * 0.1, w * 0.5, h * 0.05), 10.0, 0xff2a303a);
    cv.rrect(&Rect::new(w * 0.45, h * 0.3, w * 0.45, h * 0.5), 10.0, 0xff242a33);
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

fn write_ppm(out: &str, px: &[u32], bw: i32, bh: i32) -> bool {
    let mut f = format!("P6\n{} {}\n255\n", bw, bh).into_bytes();
    for p in px {
        f.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, *p as u8]);
    }
    match std::fs::write(out, f) {
        Ok(()) => true,
        Err(e) => {
            println!("FAIL write {}: {}", out, e);
            false
        }
    }
}

/// `--window home|<module> [--win-w W --win-h H] [--monitor] [--mode M] [--usb-hint H] [--press-hit usb|pad|screen]`
fn render_window(a: &crate::debug::Args, out: &str) -> i32 {
    let page = a.val("--window").unwrap_or_else(|| "home".into());
    let (w, h) = (a.f64("--win-w", 1696.0) as f32, a.f64("--win-h", 1128.0) as f32);
    let ppm = a.f64("--ppm", 1696.0 / 274.0) as f32;
    let scale = a.f64("--scale", 1.0) as f32;
    let mut v = appwin::debug_view(&page, debug_layouts(a), a.flag("--monitor"));
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
    v.palette = match a.val("--desk") {
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
    let pressed = a.val("--press-hit").and_then(|s| dexhid_core::modes::Mode::parse(&s)).map(|m| WinHit::Mode(m.index() as u8));
    let l = {
        let mut cv = Canvas::new(&mut px, bw, bh, scale);
        appwin::draw(&mut cv, &mut fonts, &v.palette, w, h, ppm, &v, pressed)
    };
    for (r, hit) in &l.hits {
        println!("HIT {:?} {:.0},{:.0},{:.0},{:.0} h_mm={:.1}", hit, r.x, r.y, r.w, r.h, r.h / ppm);
    }
    println!("WINDOW page={} overflow={}", page, l.overflow);
    if !write_ppm(out, &px, bw, bh) {
        return 1;
    }
    println!("RENDERED {} {}x{} window", out, bw, bh);
    0
}

/// `dexhid debug render --layout full [--portrait] [--edit] [--shift] [--keymap FILE] [--scale N] [--prompt|--pill
/// [--lit N]] [--window PAGE] --out F.ppm`: draw a screen into a PPM file (PC check of layouts, legends, the look)
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
    let scale = a.f64("--scale", 1.0) as f32;
    let (bw, bh) = ((m.w * scale) as i32, (m.h * scale) as i32);
    let mut px = vec![0u32; (bw * bh) as usize];
    let mut fonts = Fonts::load();
    println!("FONT {} ok={}", fonts.path, fonts.ok());
    {
        let mut cv = Canvas::new(&mut px, bw, bh, scale);
        let p = look::palette_of(&m);
        if a.flag("--prompt") || a.flag("--pill") {
            // the prompt / pill over a stand-in desktop; `--lit N` shows button N pressed
            mock_desktop(&mut cv, m.w, m.h);
            let lit = a.val("--lit").and_then(|v| v.parse::<usize>().ok());
            let (frame, hits) = if a.flag("--prompt") {
                let lh = lit.and_then(|i| look::prompt_layout(m.w, m.h, m.px_per_mm).4.get(i).map(|x| x.1));
                look::draw_prompt(&mut cv, &mut fonts, &p, m.w, m.h, m.px_per_mm, lh, (0.0, 0.0))
            } else {
                look::draw_pill(&mut cv, &mut fonts, &p, m.w, m.px_per_mm, lit.map(|i| if i == 0 { Hit::PillPad } else { Hit::PillHide }), (0.0, 0.0))
            };
            println!("SURFACE frame={:.0},{:.0},{:.0},{:.0}", frame.x, frame.y, frame.w, frame.h);
            for (r, h) in &hits {
                println!("HIT {} {:.0},{:.0},{:.0},{:.0} h_mm={:.1}", h.name(), r.x, r.y, r.w, r.h, r.h / m.px_per_mm);
            }
        } else {
            // M3: a synthetic preview frame (a gradient with a "window") for the mirror render
            let mframe: Option<look::MirrorFrame> = a.flag("--mirror").then(|| {
                let (w, h) = (480u32, 270u32);
                let px: Vec<u32> = (0..w * h)
                    .map(|i| {
                        let (x, y) = (i % w, i / w);
                        if (120..360).contains(&x) && (60..210).contains(&y) { 0xffe8e8e8 } else { 0xff000000 | (30 + x * 60 / w) << 16 | (50 + y * 80 / h) << 8 | 120 }
                    })
                    .collect();
                (w, h, px)
            });
            look::draw_scene(&mut cv, &mut fonts, &m, &p, look::corner_of(&m), labels.as_ref(), mframe.as_ref());
        }
    }
    println!("FONT emoji={}", fonts.emoji_path);
    println!("{}", legends_line(&m, labels.as_ref()));
    if !write_ppm(&out, &px, bw, bh) {
        return 1;
    }
    for l in geom.iter().take(4) {
        println!("{}", l);
    }
    println!("RENDERED {} {}x{} keys={} regions={}", out, bw, bh, m.keys.len(), m.regions.len());
    0
}

/// M4 measurement: `dexhid debug bench [--scale 2] [--iters 40] [--desk F --plasmashellrc F] [--half]`: the UI
/// thread's real frame path (two buffers, per-buffer stale rects, the scene drawn with a clip per rect) for the
/// typical events: first frame, key press, touch dot, Shift / latch (legend change), info text (timer tick), a
/// settings-sheet tap; the per-touch hand-off; median ms, heap allocations per frame (`--features alloc-count`),
/// painted px; the prompt (small surface) and the app window; buffer memory.
pub fn bench_debug(a: &crate::debug::Args) -> i32 {
    use std::time::Instant;
    let mut scale = a.f64("--scale", 2.0) as f32;
    if a.flag("--half") {
        scale *= 0.5;
    }
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
        let (m0, labels, _) = match debug_scene(&crate::debug::Args(argv)) {
            Ok(s) => s,
            Err(rc) => return rc,
        };
        let (bw, bh) = ((m0.w * scale) as i32, (m0.h * scale) as i32);
        buf_px = buf_px.max((bw * bh) as usize);
        let p = look::palette_of(&m0);
        let cr = look::corner_of(&m0);
        let pad = m0.regions.iter().find(|r| r.id == dexhid_core::arrangement::TOUCHPAD).map(|r| r.rect).unwrap_or(Rect::new(0.0, 0.0, m0.w, m0.h));
        // the two buffers of a surface with their stale rects, painted alternately like the UI thread does
        let mut bufs = [vec![0u32; (bw * bh) as usize], vec![0u32; (bw * bh) as usize]];
        let mut pend: [Vec<BRect>; 2] = [vec![], vec![]];
        let mut full = [true, true];
        let mut next = 0usize;
        let mut frame = |m: &UiModel, dirty: Option<&[Rect]>, fonts: &mut Fonts| -> (f64, i64) {
            for i in 0..2 {
                match dirty {
                    None => full[i] = true,
                    Some(d) => pend[i].extend(d.iter().map(|r| brect_of(r, scale, bw, bh))),
                }
            }
            let t0 = Instant::now();
            let painted = paint(&mut bufs[next], bw, bh, scale, &mut pend[next], &mut full[next], |cv| look::draw_scene(cv, fonts, m, &p, cr, labels.as_ref(), None));
            next ^= 1;
            (t0.elapsed().as_secs_f64() * 1e3, painted)
        };
        let mut m = m0.clone();
        let mut items = vec![];
        let mut items2 = vec![];
        look::scene_items(&m, labels.as_ref(), &mut items);
        // first frame (both buffers)
        let mut tv = vec![];
        for _ in 0..iters.min(10) {
            tv.push(frame(&m, None, &mut fonts).0);
        }
        println!("BENCH {:<15} full-frame      {:>8.3} ms  px={}", name, med(&mut tv), bw * bh);
        // per-touch hand-off: the dynamic part only (clone_from: no allocation in steady state)
        let mut ds = DynState::default();
        let mut tv = vec![];
        let a0 = crate::allocs();
        for i in 0..iters {
            m.dots = vec![(pad.x + i as f32, pad.y + 5.0)];
            let t0 = Instant::now();
            ds.copy_from_model(&m);
            let mut m2 = std::hint::black_box(&mut m);
            ds.copy_into_model(&mut m2);
            tv.push(t0.elapsed().as_secs_f64() * 1e3);
        }
        let a1 = crate::allocs();
        // (the measured loop also allocates the test's own dots vector once per iteration)
        println!("BENCH {:<15} dyn-handoff     {:>8.4} ms  allocs={:>5}", name, med(&mut tv), (a1.0 - a0.0) / iters as u64 - 1);
        m.dots.clear();
        // dynamic frames: key press, touch dot
        let mut olddyn: Vec<Rect> = vec![];
        for what in ["key-frame", "dot-frame"] {
            let mut tv = vec![];
            let mut px_sum = 0i64;
            let nk = m.keys.len().max(1);
            let mut dirty: Vec<Rect> = vec![];
            let mut newdyn: Vec<Rect> = vec![];
            let a0 = crate::allocs();
            for i in 0..iters {
                if what == "key-frame" {
                    m.lit_keys.clear();
                    if !m.keys.is_empty() {
                        m.lit_keys.push((i * 7) % nk);
                    }
                    m.dots.clear();
                } else {
                    m.lit_keys.clear();
                    m.dots.clear();
                    m.dots.push((pad.x + pad.w * (0.2 + 0.6 * (i as f32 / iters as f32)), pad.y + pad.h * 0.5));
                }
                newdyn.clear();
                look::dyn_rects(&m, &mut newdyn);
                dirty.clear();
                dirty.extend(olddyn.iter().copied());
                dirty.extend(newdyn.iter().copied());
                std::mem::swap(&mut olddyn, &mut newdyn);
                let (ms, painted) = frame(&m, Some(&dirty), &mut fonts);
                tv.push(ms);
                px_sum += painted;
            }
            let a1 = crate::allocs();
            println!("BENCH {:<15} {:<15} {:>8.3} ms  allocs={:>5} painted_px={}", name, what, med(&mut tv), (a1.0 - a0.0) / iters as u64, px_sum / iters as i64);
        }
        m.lit_keys.clear();
        m.dots.clear();
        // static changes: Shift (legends), info text (timer tick), a sheet tap; diff -> rects -> clipped repaint
        for what in ["shift-frame", "info-frame", "sheet-tap"] {
            let mut tv = vec![];
            let mut px_sum = 0i64;
            let mut full_n = 0;
            for i in 0..iters.min(16) {
                match what {
                    "shift-frame" => m.shift = i % 2 == 0,
                    "info-frame" => m.info = format!("Timer 12:{:02} left  |  84 % charging  |  DP-1", i),
                    _ => {
                        if let Some(b) = m.buttons.iter_mut().find(|b| matches!(b.btn, dexhid_core::router::Btn::SheetRow(1))) {
                            b.label = format!("{:.1}", 1.0 + i as f32 * 0.1);
                        }
                    }
                }
                let t0 = Instant::now();
                look::scene_items(&m, labels.as_ref(), &mut items2);
                let d = look::diff_items(&items, &items2);
                std::mem::swap(&mut items, &mut items2);
                let el = t0.elapsed().as_secs_f64() * 1e3;
                if d.is_none() {
                    full_n += 1;
                }
                let (ms, painted) = frame(&m, d.as_deref(), &mut fonts);
                tv.push(el + ms);
                px_sum += painted;
            }
            m.shift = false;
            println!("BENCH {:<15} {:<15} {:>8.3} ms  painted_px={} full_frames={}", name, what, med(&mut tv), px_sum / iters.min(16) as i64, full_n);
        }
    }
    // prompt: a small surface, full redraw on each press
    let (m, _, _) = match debug_scene(&crate::debug::Args(vec!["bench".into(), "--layout".into(), "full".into()])) {
        Ok(s) => s,
        Err(rc) => return rc,
    };
    let ((sw, sh_), origin) = look::prompt_surface(m.w, m.h, m.px_per_mm);
    let (pbw, pbh) = ((sw as f32 * scale) as i32, (sh_ as f32 * scale) as i32);
    let mut ppx = vec![0u32; (pbw * pbh) as usize];
    let mut tv = vec![];
    for _ in 0..iters.min(12) {
        let t0 = Instant::now();
        let mut cv = Canvas::new(&mut ppx, pbw, pbh, scale);
        cv.clear(0);
        look::draw_prompt(&mut cv, &mut fonts, &look::palette_of(&m), m.w, m.h, m.px_per_mm, None, origin);
        tv.push(t0.elapsed().as_secs_f64() * 1e3);
    }
    println!("BENCH prompt          full-redraw     {:>8.3} ms  surface={}x{} px={} ({:.1} MiB for 2 buffers)", med(&mut tv), sw, sh_, pbw * pbh, 2.0 * (pbw * pbh) as f64 * 4.0 / 1048576.0);
    // the app window (1696x1128 logical: the panel minus the Plasma panel)
    {
        let (w, h, ppm) = (1696.0f32, 1128.0f32, 1696.0f32 / 274.0);
        let lays = debug_layouts(&crate::debug::Args(vec![]));
        let (bw, bh) = ((w * scale) as i32, (h * scale) as i32);
        let mut px = vec![0u32; (bw * bh) as usize];
        for page in ["home", "pointer", "usbhid"] {
            let v = appwin::debug_view(page, lays.clone(), false);
            let mut tv = vec![];
            let a0 = crate::allocs();
            for _ in 0..iters.min(12) {
                let t0 = Instant::now();
                let mut cv = Canvas::new(&mut px, bw, bh, scale);
                appwin::draw(&mut cv, &mut fonts, &v.palette, w, h, ppm, &v, None);
                tv.push(t0.elapsed().as_secs_f64() * 1e3);
            }
            let a1 = crate::allocs();
            let n = iters.min(12) as u64;
            println!("BENCH window-{:<8} full-redraw     {:>8.3} ms  allocs={:>5} px={}", page, med(&mut tv), (a1.0 - a0.0) / n, bw * bh);
        }
        // a press on a mode segment: only its rect, the scene drawn with a clip
        let v = appwin::debug_view("home", lays.clone(), true);
        let l = appwin::layout(w, h, ppm, &v);
        let br = l.hits.iter().find(|x| x.1 == WinHit::Mode(1)).map(|x| x.0).unwrap_or(Rect::new(0.0, 0.0, 1.0, 1.0));
        let mut tv = vec![];
        let mut pend = vec![];
        let mut full = false;
        for _ in 0..iters {
            pend.push(brect_of(&br, scale, bw, bh));
            let t0 = Instant::now();
            paint(&mut px, bw, bh, scale, &mut pend, &mut full, |cv| {
                appwin::draw(cv, &mut fonts, &v.palette, w, h, ppm, &v, Some(WinHit::Mode(1)));
            });
            tv.push(t0.elapsed().as_secs_f64() * 1e3);
        }
        println!("BENCH window-press     segment-frame   {:>8.3} ms  painted_px={}", med(&mut tv), { let r = brect_of(&br, scale, bw, bh); r.2 as i64 * r.3 as i64 });
        println!("BENCH window-memory    open: 2 buffers = {:.1} MiB; closed: 0 (no surface, no pool, no timer)", 2.0 * (bw * bh) as f64 * 4.0 / 1048576.0);
    }
    let (n, b) = fonts.cache_stats();
    println!("BENCH glyph-cache entries={} bytes={}", n, b);
    let st = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let get = |k: &str| st.lines().find(|l| l.starts_with(k)).map(|l| l.split_whitespace().nth(1).unwrap_or("0").to_string()).unwrap_or_default();
    let mb = |px: usize| px as f64 * 4.0 / 1048576.0;
    println!("BENCH memory buffer={}px ({:.1} MiB) overlay holds 2 buffers = {:.1} MiB; bench VmRSS={} kB VmHWM={} kB", buf_px, mb(buf_px), 2.0 * mb(buf_px), get("VmRSS:"), get("VmHWM:"));
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scene(args: &[&str]) -> (UiModel, Option<crate::xkb::Labels>) {
        let mut v: Vec<String> = vec!["test".into()];
        v.extend(args.iter().map(|s| s.to_string()));
        let (m, l, _) = debug_scene(&crate::debug::Args(v)).expect("scene");
        (m, l)
    }

    fn full(m: &UiModel, labels: Option<&crate::xkb::Labels>, f: &mut Fonts, bw: i32, bh: i32, scale: f32) -> Vec<u32> {
        let mut px = vec![0u32; (bw * bh) as usize];
        let (mut p, mut fl) = (vec![], true);
        paint(&mut px, bw, bh, scale, &mut p, &mut fl, |cv| look::draw_scene(cv, f, m, &look::palette_of(m), look::corner_of(m), labels, None));
        px
    }

    /// The partial repaint (scene drawn with a clip per changed rect) gives exactly the pixels of a full repaint:
    /// key press / release, touch dots, Shift legends, Caps LED, info text (timer), a sheet value, a pressed button.
    /// Fractional scale on purpose (anti-aliased edges across clip borders). Skipped without a font.
    #[test]
    fn partial_repaint_matches_full_repaint() {
        let mut f = Fonts::load();
        if !f.ok() {
            return;
        }
        for args in [&["--layout", "full", "--quick", "--bands"][..], &["--layout", "full", "--quick", "--sheet", "pointer"][..], &["--layout", "full", "--quick", "--portrait"][..]] {
            let (mut m, labels) = scene(args);
            let scale = 1.5f32;
            let (bw, bh) = ((m.w * scale) as i32, (m.h * scale) as i32);
            let mut buf = full(&m, labels.as_ref(), &mut f, bw, bh, scale);
            let mut items = vec![];
            look::scene_items(&m, labels.as_ref(), &mut items);
            let mut dyn_old = vec![];
            look::dyn_rects(&m, &mut dyn_old);
            let steps: Vec<(&str, Box<dyn Fn(&mut UiModel)>)> = vec![
                ("key down", Box::new(|m: &mut UiModel| m.lit_keys = vec![3, 40])),
                ("key up", Box::new(|m: &mut UiModel| m.lit_keys.clear())),
                ("dot", Box::new(|m: &mut UiModel| m.dots = vec![(m.w * 0.5, m.h * 0.85)])),
                ("dot moves", Box::new(|m: &mut UiModel| m.dots = vec![(m.w * 0.52, m.h * 0.84)])),
                ("dot up", Box::new(|m: &mut UiModel| m.dots.clear())),
                ("shift", Box::new(|m: &mut UiModel| m.shift = true)),
                ("caps", Box::new(|m: &mut UiModel| m.leds = (true, false))),
                ("unshift", Box::new(|m: &mut UiModel| m.shift = false)),
                ("timer", Box::new(|m: &mut UiModel| m.info = "Timer 12:34 left  |  84 % charging  |  DP-1".into())),
                ("sheet value", Box::new(|m: &mut UiModel| {
                    if let Some(b) = m.buttons.iter_mut().find(|b| matches!(b.btn, dexhid_core::router::Btn::SheetRow(1))) {
                        b.label = "2.5".into();
                    }
                })),
                ("button down", Box::new(|m: &mut UiModel| m.lit_button = m.buttons.first().map(|b| b.btn))),
                ("button up", Box::new(|m: &mut UiModel| m.lit_button = None)),
            ];
            let (mut pend, mut fl) = (vec![], false);
            for (name, step) in &steps {
                step(&mut m);
                let mut items2 = vec![];
                look::scene_items(&m, labels.as_ref(), &mut items2);
                let mut rects = match look::diff_items(&items, &items2) {
                    Some(r) => r,
                    None => panic!("{:?} {}: a small change must not need a full frame", args, name),
                };
                items = items2;
                let mut dyn_new = vec![];
                look::dyn_rects(&m, &mut dyn_new);
                rects.extend(dyn_old.iter().copied());
                rects.extend(dyn_new.iter().copied());
                dyn_old = dyn_new;
                pend.extend(rects.iter().map(|r| brect_of(r, scale, bw, bh)));
                let painted = paint(&mut buf, bw, bh, scale, &mut pend, &mut fl, |cv| look::draw_scene(cv, &mut f, &m, &look::palette_of(&m), look::corner_of(&m), labels.as_ref(), None));
                let want = full(&m, labels.as_ref(), &mut f, bw, bh, scale);
                let bad = buf.iter().zip(&want).filter(|(a, b)| a != b).count();
                assert_eq!(bad, 0, "{:?} {}: {} px differ from a full repaint (painted {} of {})", args, name, bad, painted, bw * bh);
                assert!(painted < (bw * bh) as i64 / 2, "{:?} {}: partial ({} px)", args, name, painted);
            }
        }
    }

    /// structural changes (layout, orientation, theme, sheet open) are full frames
    #[test]
    fn structural_changes_are_full_frames() {
        let (m, l) = scene(&["--layout", "full", "--quick"]);
        let mut a = vec![];
        look::scene_items(&m, l.as_ref(), &mut a);
        for (name, other) in [("layout", scene(&["--layout", "tablet", "--quick"]).0), ("portrait", scene(&["--layout", "full", "--quick", "--portrait"]).0), ("sheet", scene(&["--layout", "full", "--quick", "--sheet", "pointer"]).0), ("theme", scene(&["--layout", "full", "--quick", "--theme", "light"]).0)] {
            let mut b = vec![];
            look::scene_items(&other, l.as_ref(), &mut b);
            assert!(look::diff_items(&a, &b).is_none(), "{} needs a full frame", name);
        }
        let mut b = vec![];
        look::scene_items(&m, l.as_ref(), &mut b);
        assert_eq!(look::diff_items(&a, &b), Some(vec![]), "no change, no rects");
    }

    /// paint never touches pixels outside its stale rects
    #[test]
    fn paint_stays_inside_the_stale_rects() {
        let (bw, bh) = (200, 120);
        let mut px = vec![0x11223344u32; (bw * bh) as usize];
        let mut pend = vec![(10, 10, 30, 20), (100, 50, 40, 40)];
        let mut fl = false;
        paint(&mut px, bw, bh, 1.0, &mut pend, &mut fl, |cv| {
            cv.clear(0xffffffff);
            cv.rrect(&Rect::new(-50.0, -50.0, 400.0, 400.0), 20.0, 0xff0000ff);
            look::icon(cv, &Rect::new(0.0, 0.0, 200.0, 120.0), look::Icon::Gear, 100.0, 0xff00ff00);
        });
        for y in 0..bh {
            for x in 0..bw {
                let inside = (10..40).contains(&x) && (10..30).contains(&y) || (100..140).contains(&x) && (50..90).contains(&y);
                assert_eq!(px[(y * bw + x) as usize] != 0x11223344, inside, "pixel {},{}", x, y);
            }
        }
        assert!(pend.is_empty() && !fl);
    }
}
