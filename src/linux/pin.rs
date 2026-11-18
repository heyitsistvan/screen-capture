//! Pinned captures on Linux: a borderless window per pin, drawn by
//! [`crate::pin::Pin`]. Drag to move, drag an edge or corner to resize,
//! wheel to zoom; the toolbar that appears on hover has zoom, opacity, copy,
//! save and close. Esc or double-click closes, Ctrl+C / Ctrl+S copy or save,
//! right-click shows the controls. On X11 the pin stays above other windows,
//! opacity works (with a compositing window manager) and the arrow keys nudge
//! it; Wayland leaves stacking, placement and opacity to the compositor.

use std::num::NonZeroU32;
use std::rc::Rc;
use std::time::{Duration, Instant};

use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::{Key, NamedKey};
use winit::platform::wayland::WindowAttributesExtWayland;
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::{CursorIcon, ResizeDirection, Window, WindowLevel};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, PropMode};
use x11rb::wrapper::ConnectionExt as _;

use super::capture::X11;
use crate::annotate::Shape;
use crate::pin::{Edge, Pin, PinBtn, BORDER};
use crate::text::Fonts;

const HINT_X11: &[&str] = &[
    "Drag to move, drag the edges to resize",
    "Wheel: zoom · Ctrl+wheel: opacity",
    "Arrows: nudge · Esc or double-click: close",
];
const HINT_WAYLAND: &[&str] = &["Drag to move, drag the edges to resize", "Wheel: zoom · Esc or double-click: close"];
/// After the user stops resizing, the window settles on the picture's
/// proportions.
const SETTLE: Duration = Duration::from_millis(300);

/// What a pin asks of the app.
pub enum PinEvent {
    Close,
    Copy,
    Save,
    Edit,
}

pub struct PinWindow {
    pub window: Rc<Window>,
    _context: softbuffer::Context<Rc<Window>>,
    surface: softbuffer::Surface<Rc<Window>, Rc<Window>>,
    pub pin: Pin,
    wayland: bool,
    ctrl: bool,
    shift: bool,
    cursor: PhysicalPosition<f64>,
    last_click: Option<(Instant, PhysicalPosition<f64>)>,
    /// Where we last put the window (X11). Asking the window manager lags
    /// behind quick successive moves.
    pos: Option<PhysicalPosition<i32>>,
    /// The edge the user is resizing by (the zoom follows its direction).
    resizing: Option<Edge>,
    /// When to snap the window back to the picture's proportions after a
    /// resize by the user.
    settle_at: Option<Instant>,
    /// Leftover wheel movement from touchpads.
    wheel: f64,
    last_wheel: Option<(Instant, MouseScrollDelta)>,
    frame: Vec<u8>,
}

impl PinWindow {
    /// Pins an image with its top-left corner at screen position `at` (X11;
    /// Wayland places it where it likes). Doesn't take the focus; a click does.
    pub fn open(
        el: &ActiveEventLoop,
        w: i32,
        h: i32,
        pixels: Vec<u8>,
        layers: Option<(Vec<u8>, Vec<Shape>)>,
        at: (i32, i32),
        wayland: bool,
    ) -> Result<PinWindow, String> {
        let mut pin = Pin::new(w, h, pixels, if wayland { HINT_WAYLAND } else { HINT_X11 }, !wayland);
        pin.set_layers(layers);
        let (ww, wh) = pin.size();
        let mut attrs = Window::default_attributes()
            .with_title("Pinned capture")
            .with_decorations(false)
            .with_active(false)
            .with_window_level(WindowLevel::AlwaysOnTop)
            .with_min_inner_size(PhysicalSize::new(16u32, 16u32))
            .with_inner_size(PhysicalSize::new(ww as u32, wh as u32))
            .with_position(PhysicalPosition::new(at.0 - BORDER, at.1 - BORDER));
        attrs = WindowAttributesExtWayland::with_name(attrs, "screen-capture", "screen-capture");
        let window = Rc::new(el.create_window(attrs).map_err(|e| e.to_string())?);
        let context = softbuffer::Context::new(window.clone()).map_err(|e| e.to_string())?;
        let surface = softbuffer::Surface::new(&context, window.clone()).map_err(|e| e.to_string())?;
        window.set_cursor(CursorIcon::Move);
        window.request_redraw();
        Ok(PinWindow {
            window,
            _context: context,
            surface,
            pin,
            wayland,
            ctrl: false,
            shift: false,
            cursor: PhysicalPosition::new(0.0, 0.0),
            last_click: None,
            pos: (!wayland).then(|| PhysicalPosition::new(at.0 - BORDER, at.1 - BORDER)),
            resizing: None,
            settle_at: None,
            wheel: 0.0,
            last_wheel: None,
            frame: Vec::new(),
        })
    }

    fn x11_id(&self) -> Option<u32> {
        match self.window.window_handle().ok()?.as_raw() {
            RawWindowHandle::Xlib(h) => Some(h.window as u32),
            RawWindowHandle::Xcb(h) => Some(h.window.get()),
            _ => None,
        }
    }

    /// The window size and its UI scale (toolbar, labels).
    fn metrics(&self) -> (i32, i32, f32) {
        let s = self.window.inner_size();
        (s.width as i32, s.height as i32, self.window.scale_factor() as f32)
    }

    fn mouse(&self) -> (i32, i32) {
        (self.cursor.x as i32, self.cursor.y as i32)
    }

    /// What editing the pin starts from: its picture without drawings, the
    /// drawings, and where the picture is on screen (X11 only).
    #[allow(clippy::type_complexity)]
    pub fn content(&self) -> (i32, i32, Vec<u8>, Vec<Shape>, Option<(i32, i32)>) {
        let (w, h, _) = self.pin.image();
        let (base, shapes) = self.pin.layers();
        let at = self.pos.map(|p| (p.x + BORDER, p.y + BORDER));
        (w, h, base, shapes, at)
    }

    /// Shows the edited picture, keeping the pin's place, zoom and opacity.
    pub fn replace(&mut self, w: i32, h: i32, pixels: Vec<u8>, layers: Option<(Vec<u8>, Vec<Shape>)>) {
        self.pin.replace(w, h, pixels, layers);
        let (ww, wh) = self.pin.size();
        let _ = self.window.request_inner_size(PhysicalSize::new(ww as u32, wh as u32));
        self.window.request_redraw();
    }

    /// Shows a short message on the pin (e.g. "Copied").
    pub fn badge(&mut self, text: &str) {
        self.pin.set_badge(text.into());
        self.window.request_redraw();
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        [self.pin.next_deadline(), self.settle_at].into_iter().flatten().min()
    }

    /// Repaints if a label has expired; settles the size after a resize.
    pub fn tick(&mut self, now: Instant) {
        if self.pin.expire(now) {
            self.window.request_redraw();
        }
        if self.settle_at.is_some_and(|t| t <= now) {
            self.settle_at = None;
            self.resizing = None;
            let (w, h) = self.pin.size();
            let _ = self.window.request_inner_size(PhysicalSize::new(w as u32, h as u32));
        }
    }

    /// Moves the window by (`dx`, `dy`) (X11 only: Wayland windows can't).
    fn nudge(&mut self, dx: i32, dy: i32) {
        let Some(p) = self.pos.or_else(|| self.window.outer_position().ok()) else { return };
        let to = PhysicalPosition::new(p.x + dx, p.y + dy);
        self.window.set_outer_position(to);
        self.pos = Some(to);
        // The mouse stays put, so it's somewhere else on the window now.
        self.cursor = PhysicalPosition::new(self.cursor.x - dx as f64, self.cursor.y - dy as f64);
    }

    /// Zooms around window position (`ax`, `ay`), moving the window so that
    /// point stays put.
    fn zoom(&mut self, steps: i32, actual: bool, (ax, ay): (i32, i32)) {
        let moved = if actual { self.pin.actual_size(ax, ay) } else { self.pin.zoom_by(steps, ax, ay) };
        if let Some((dx, dy)) = moved {
            let (w, h) = self.pin.size();
            let _ = self.window.request_inner_size(PhysicalSize::new(w as u32, h as u32));
            self.nudge(dx, dy);
        }
        self.window.request_redraw();
    }

    /// Changes the opacity and applies it through `_NET_WM_WINDOW_OPACITY`,
    /// which compositing window managers honor.
    fn opacity(&mut self, x11: Option<&X11>, change: impl FnOnce(&mut Pin) -> bool) {
        if self.wayland {
            self.pin.set_badge("Opacity isn't available on Wayland".into());
        } else if change(&mut self.pin) {
            if let (Some(x), Some(id)) = (x11, self.x11_id()) {
                let value = (self.pin.opacity() as f64 * u32::MAX as f64) as u32;
                let atom = x.conn.intern_atom(false, b"_NET_WM_WINDOW_OPACITY").ok().and_then(|c| c.reply().ok());
                if let Some(atom) = atom {
                    let _ = x.conn.change_property32(PropMode::REPLACE, id, atom.atom, AtomEnum::CARDINAL, &[value]);
                    let _ = x.conn.flush();
                }
            }
        }
        self.window.request_redraw();
    }

    /// A toolbar button was clicked.
    fn press(&mut self, btn: PinBtn, x11: Option<&X11>) -> Option<PinEvent> {
        match btn {
            // Back to 100% from the top-left corner, so the pin lines up with
            // where it was captured again.
            PinBtn::Zoom => self.zoom(0, true, (BORDER, BORDER)),
            PinBtn::Opacity => self.opacity(x11, |pin| {
                pin.next_opacity();
                true
            }),
            PinBtn::Edit => return Some(PinEvent::Edit),
            PinBtn::Copy => return Some(PinEvent::Copy),
            PinBtn::Save => return Some(PinEvent::Save),
            PinBtn::Close => return Some(PinEvent::Close),
        }
        None
    }

    /// The mouse moved over the pin: toolbar, button highlight, cursor shape.
    fn hover(&mut self, pos: Option<(i32, i32)>) {
        let (w, h, scale) = self.metrics();
        if self.pin.set_hover(pos, w, h, scale) {
            self.window.request_redraw();
        }
        let Some((x, y)) = pos else { return };
        let icon = if self.pin.button_at(x, y, w, h, scale).is_some() {
            CursorIcon::Pointer
        } else {
            match self.pin.edge_at(x, y, w, h, scale) {
                Some(Edge::N | Edge::S) => CursorIcon::NsResize,
                Some(Edge::E | Edge::W) => CursorIcon::EwResize,
                Some(Edge::NW | Edge::SE) => CursorIcon::NwseResize,
                Some(Edge::NE | Edge::SW) => CursorIcon::NeswResize,
                None => CursorIcon::Move,
            }
        };
        self.window.set_cursor(icon);
    }

    pub fn event(&mut self, ev: WindowEvent, x11: Option<&X11>, fonts: &Fonts) -> Option<PinEvent> {
        match ev {
            WindowEvent::RedrawRequested => self.redraw(fonts),
            WindowEvent::Resized(size) => {
                // The user resized it: the zoom follows, and the window
                // settles on the picture's proportions once they stop.
                let by_width = match self.resizing {
                    Some(Edge::N | Edge::S) => Some(false),
                    Some(_) => Some(true),
                    None => None,
                };
                if self.pin.fit_to(size.width as i32, size.height as i32, by_width) {
                    self.settle_at = Some(Instant::now() + SETTLE);
                }
                self.window.request_redraw();
            }
            WindowEvent::Moved(p) if !self.wayland => self.pos = Some(p),
            WindowEvent::CloseRequested => return Some(PinEvent::Close),
            WindowEvent::ModifiersChanged(m) => {
                self.ctrl = m.state().control_key();
                self.shift = m.state().shift_key();
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = position;
                self.hover(Some(self.mouse()));
            }
            WindowEvent::CursorLeft { .. } if self.pin.pressed().is_none() => self.hover(None),
            WindowEvent::MouseInput { state, button, .. } => {
                let (x, y) = self.mouse();
                let (w, h, scale) = self.metrics();
                match (button, state) {
                    (MouseButton::Left, ElementState::Pressed) => {
                        if let Some(btn) = self.pin.button_at(x, y, w, h, scale) {
                            self.pin.set_pressed(Some(btn));
                        } else if let Some(edge) = self.pin.edge_at(x, y, w, h, scale) {
                            let dir = match edge {
                                Edge::N => ResizeDirection::North,
                                Edge::S => ResizeDirection::South,
                                Edge::E => ResizeDirection::East,
                                Edge::W => ResizeDirection::West,
                                Edge::NE => ResizeDirection::NorthEast,
                                Edge::NW => ResizeDirection::NorthWest,
                                Edge::SE => ResizeDirection::SouthEast,
                                Edge::SW => ResizeDirection::SouthWest,
                            };
                            self.resizing = Some(edge);
                            let _ = self.window.drag_resize_window(dir);
                        } else {
                            // winit has no double-click; detect it here.
                            let p = self.cursor;
                            let dbl = self.last_click.is_some_and(|(t, c)| {
                                t.elapsed() < Duration::from_millis(450) && (c.x - p.x).abs() < 5.0 && (c.y - p.y).abs() < 5.0
                            });
                            if dbl {
                                return Some(PinEvent::Close);
                            }
                            self.last_click = Some((Instant::now(), p));
                            let _ = self.window.drag_window();
                        }
                    }
                    (MouseButton::Left, ElementState::Released) => {
                        let pressed = self.pin.pressed();
                        self.pin.set_pressed(None);
                        if let Some(btn) = pressed.filter(|&b| self.pin.button_at(x, y, w, h, scale) == Some(b)) {
                            return self.press(btn, x11);
                        }
                    }
                    (MouseButton::Right, ElementState::Pressed) => {
                        self.pin.show_hint();
                        self.window.request_redraw();
                    }
                    _ => {}
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                // Simulated input (xdotool, VNC, xrdp) arrives twice on X11, as
                // a wheel button and as a scroll axis: skip the echo.
                if self.last_wheel.is_some_and(|(t, d)| d == delta && t.elapsed() < Duration::from_millis(3)) {
                    return None;
                }
                self.last_wheel = Some((Instant::now(), delta));
                self.wheel += match delta {
                    MouseScrollDelta::LineDelta(_, y) => y as f64,
                    MouseScrollDelta::PixelDelta(p) => p.y / 40.0,
                };
                let steps = self.wheel.trunc() as i32;
                self.wheel -= steps as f64;
                if steps != 0 {
                    if self.ctrl {
                        self.opacity(x11, |pin| pin.opacity_by(steps));
                    } else {
                        let anchor = self.mouse();
                        self.zoom(steps, false, anchor);
                    }
                }
            }
            WindowEvent::KeyboardInput { event, is_synthetic: false, .. } if event.state == ElementState::Pressed => {
                let step = if self.shift { 10 } else { 1 };
                match &event.logical_key {
                    Key::Named(NamedKey::Escape) => return Some(PinEvent::Close),
                    Key::Named(NamedKey::ArrowLeft) => self.nudge(-step, 0),
                    Key::Named(NamedKey::ArrowRight) => self.nudge(step, 0),
                    Key::Named(NamedKey::ArrowUp) => self.nudge(0, -step),
                    Key::Named(NamedKey::ArrowDown) => self.nudge(0, step),
                    Key::Character(c) if self.ctrl => match c.to_lowercase().as_str() {
                        "e" => return Some(PinEvent::Edit),
                        "c" => return Some(PinEvent::Copy),
                        "s" => return Some(PinEvent::Save),
                        "0" => self.zoom(0, true, (BORDER, BORDER)),
                        _ => {}
                    },
                    _ => {}
                }
            }
            _ => {}
        }
        None
    }

    fn redraw(&mut self, fonts: &Fonts) {
        let size = self.window.inner_size();
        let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else { return };
        if self.surface.resize(w, h).is_err() {
            return;
        }
        let (w, h) = (size.width as i32, size.height as i32);
        self.frame.resize(w as usize * h as usize * 4, 0);
        self.pin.render(&mut self.frame, w, h, fonts, self.window.scale_factor() as f32);
        let Ok(mut buf) = self.surface.buffer_mut() else { return };
        for (d, s) in buf.iter_mut().zip(self.frame.chunks_exact(4)) {
            *d = u32::from_le_bytes([s[0], s[1], s[2], 0]);
        }
        let _ = buf.present();
    }
}
