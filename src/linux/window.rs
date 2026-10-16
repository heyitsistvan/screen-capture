//! The editor's full-screen window on Linux (winit + softbuffer, X11 and
//! Wayland): feeds input to the [`Editor`] and presents its frames.

use std::cell::RefCell;
use std::collections::HashMap;
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use winit::dpi::PhysicalPosition;
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::{Key, NamedKey};
use winit::monitor::MonitorHandle;
use winit::platform::startup_notify::WindowAttributesExtStartupNotify;
use winit::platform::wayland::WindowAttributesExtWayland;
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::{ActivationToken, CursorIcon, Fullscreen, Window};

use crate::editor::{key, Action, Cursor, Editor, Host, Mods, Screen};
use crate::text::Fonts;
use crate::util::{Rect, APP_NAME};

/// What the editor asked of its window since the last event.
#[derive(Default)]
struct HostState {
    damage: Vec<Rect>,
    cursor: Option<Cursor>,
    /// id → (next deadline, period).
    timers: HashMap<usize, (Instant, Duration)>,
}

struct LinuxHost(Rc<RefCell<HostState>>);

impl Host for LinuxHost {
    fn invalidate(&mut self, r: Rect) {
        self.0.borrow_mut().damage.push(r);
    }

    fn set_cursor(&mut self, c: Cursor) {
        self.0.borrow_mut().cursor = Some(c);
    }

    fn set_timer(&mut self, id: usize, ms: u32) {
        let period = Duration::from_millis(ms as u64);
        self.0.borrow_mut().timers.insert(id, (Instant::now() + period, period));
    }

    fn kill_timer(&mut self, id: usize) {
        self.0.borrow_mut().timers.remove(&id);
    }

    // X11 and Wayland keep sending motion to the window while a button is
    // held, so there's nothing to do.
    fn capture_mouse(&mut self, _: bool) {}

    fn clipboard_text(&mut self) -> Option<String> {
        arboard::Clipboard::new().ok()?.get_text().ok()
    }
}

pub struct EditorWindow {
    pub window: Rc<Window>,
    _context: softbuffer::Context<Rc<Window>>,
    surface: softbuffer::Surface<Rc<Window>, Rc<Window>>,
    pub editor: Box<Editor>,
    host: Rc<RefCell<HostState>>,
    /// The frame as presented (0RGB), updated only where the editor redrew.
    frame: Vec<u32>,
    mods: Mods,
    mouse: (i32, i32),
    last_click: Option<(Instant, i32, i32)>,
    full_redraw: bool,
}

fn icon(c: Cursor) -> CursorIcon {
    match c {
        Cursor::Arrow => CursorIcon::Default,
        Cursor::Cross => CursorIcon::Crosshair,
        Cursor::Hand => CursorIcon::Pointer,
        Cursor::IBeam => CursorIcon::Text,
        Cursor::Move => CursorIcon::Move,
        Cursor::SizeNS => CursorIcon::NsResize,
        Cursor::SizeWE => CursorIcon::EwResize,
        Cursor::SizeNWSE => CursorIcon::NwseResize,
        Cursor::SizeNESW => CursorIcon::NeswResize,
        Cursor::Wait => CursorIcon::Wait,
    }
}

impl EditorWindow {
    /// Opens a full-screen editor over a screenshot, on `monitor` (or wherever
    /// the compositor puts it).
    pub fn open(
        el: &ActiveEventLoop,
        screen: Screen,
        monitor: Option<MonitorHandle>,
        fonts: Arc<Fonts>,
        gif_mode: bool,
        activation: Option<String>,
    ) -> Result<EditorWindow, String> {
        let mut attrs = Window::default_attributes()
            .with_title(APP_NAME)
            .with_decorations(false)
            .with_fullscreen(Some(Fullscreen::Borderless(monitor)));
        // The Wayland app id / X11 class, matching the .desktop file.
        attrs = WindowAttributesExtWayland::with_name(attrs, "screen-capture", "screen-capture");
        if let Some(token) = activation {
            attrs = attrs.with_activation_token(ActivationToken::from_raw(token));
        }
        let window = Rc::new(el.create_window(attrs).map_err(|e| e.to_string())?);
        let context = softbuffer::Context::new(window.clone()).map_err(|e| e.to_string())?;
        let surface = softbuffer::Surface::new(&context, window.clone()).map_err(|e| e.to_string())?;
        let host = Rc::new(RefCell::new(HostState::default()));
        let (w, h) = (screen.w, screen.h);
        let scale = window.scale_factor() as f32;
        let editor = Editor::new(Box::new(LinuxHost(host.clone())), screen, fonts, gif_mode, scale);
        window.focus_window();
        window.request_redraw();
        Ok(EditorWindow {
            window,
            _context: context,
            surface,
            editor,
            host,
            frame: vec![0; w as usize * h as usize],
            mods: Mods::default(),
            mouse: (0, 0),
            last_click: None,
            full_redraw: true,
        })
    }

    /// The X11 window id, so dialogs can stay above this window.
    pub fn x11_id(&self) -> Option<u32> {
        match self.window.window_handle().ok()?.as_raw() {
            RawWindowHandle::Xlib(h) => Some(h.window as u32),
            RawWindowHandle::Xcb(h) => Some(h.window.get()),
            _ => None,
        }
    }

    /// Editor coordinates of a window position (the window may differ in
    /// size from the screenshot, e.g. with fractional scaling).
    fn to_editor(&self, p: PhysicalPosition<f64>) -> (i32, i32) {
        let size = self.window.inner_size();
        let (ew, eh) = self.editor.size();
        let sx = if size.width > 0 { ew as f64 / size.width as f64 } else { 1.0 };
        let sy = if size.height > 0 { eh as f64 / size.height as f64 } else { 1.0 };
        ((p.x * sx) as i32, (p.y * sy) as i32)
    }

    pub fn event(&mut self, ev: WindowEvent) -> Option<Action> {
        let action = match ev {
            WindowEvent::RedrawRequested => {
                self.redraw();
                None
            }
            WindowEvent::Resized(_) => {
                self.full_redraw = true;
                self.window.request_redraw();
                None
            }
            WindowEvent::CloseRequested => Some(Action::Close),
            WindowEvent::ModifiersChanged(m) => {
                self.mods = Mods { ctrl: m.state().control_key(), shift: m.state().shift_key() };
                None
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.mouse = self.to_editor(position);
                self.editor.on_move(self.mouse.0, self.mouse.1, self.mods.shift);
                None
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let (x, y) = self.mouse;
                match (button, state) {
                    (MouseButton::Left, ElementState::Pressed) => {
                        // winit has no double-click; detect it here.
                        let dbl = self.last_click.is_some_and(|(t, cx, cy)| {
                            t.elapsed() < Duration::from_millis(450) && (cx - x).abs() < 5 && (cy - y).abs() < 5
                        });
                        self.last_click = if dbl { None } else { Some((Instant::now(), x, y)) };
                        self.editor.on_down(x, y, dbl)
                    }
                    (MouseButton::Left, ElementState::Released) => self.editor.on_up(x, y),
                    (MouseButton::Right, ElementState::Pressed) => self.editor.on_right(),
                    _ => None,
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let d = match delta {
                    MouseScrollDelta::LineDelta(_, y) => (y * 120.0) as i32,
                    MouseScrollDelta::PixelDelta(p) => (p.y.signum() * 120.0) as i32,
                };
                self.editor.on_wheel(d);
                None
            }
            WindowEvent::KeyboardInput { event, is_synthetic: false, .. } if event.state == ElementState::Pressed => {
                self.key(event)
            }
            _ => None,
        };
        self.sync();
        action
    }

    fn key(&mut self, ev: KeyEvent) -> Option<Action> {
        let code = match &ev.logical_key {
            Key::Named(NamedKey::Escape) => Some(key::ESCAPE),
            Key::Named(NamedKey::Enter) => Some(key::ENTER),
            Key::Named(NamedKey::ArrowLeft) => Some(key::LEFT),
            Key::Named(NamedKey::ArrowRight) => Some(key::RIGHT),
            Key::Named(NamedKey::ArrowUp) => Some(key::UP),
            Key::Named(NamedKey::ArrowDown) => Some(key::DOWN),
            Key::Named(NamedKey::Delete) => Some(key::DELETE),
            Key::Named(NamedKey::Backspace) => Some(key::BACKSPACE),
            Key::Character(s) => s.chars().next().filter(char::is_ascii_alphabetic).map(|c| c.to_ascii_uppercase() as u16),
            _ => None,
        };
        let action = code.and_then(|c| self.editor.on_key(c, self.mods));
        // Typed text for the text tool; Enter and Backspace arrive as text too.
        // Outside text editing the editor ignores it.
        if let Some(text) = &ev.text {
            for ch in text.chars() {
                self.editor.on_char(ch);
            }
        }
        action
    }

    /// Applies what the editor asked for: cursor, repaint.
    pub fn sync(&mut self) {
        let mut host = self.host.borrow_mut();
        if let Some(c) = host.cursor.take() {
            self.window.set_cursor(icon(c));
        }
        if !host.damage.is_empty() {
            self.window.request_redraw();
        }
    }

    pub fn next_timer(&self) -> Option<Instant> {
        self.host.borrow().timers.values().map(|t| t.0).min()
    }

    pub fn fire_timers(&mut self) {
        let now = Instant::now();
        let due: Vec<usize> = {
            let mut host = self.host.borrow_mut();
            let due: Vec<usize> = host.timers.iter().filter(|(_, t)| t.0 <= now).map(|(id, _)| *id).collect();
            for id in &due {
                if let Some(t) = host.timers.get_mut(id) {
                    t.0 = now + t.1;
                }
            }
            due
        };
        for id in due {
            self.editor.on_timer(id);
        }
        self.sync();
    }

    fn redraw(&mut self) {
        let (ew, eh) = self.editor.size();
        let full = Rect::new(0, 0, ew, eh);
        let mut damage = std::mem::take(&mut self.host.borrow_mut().damage);
        if self.full_redraw {
            damage = vec![full];
            self.full_redraw = false;
        }
        if !damage.is_empty() {
            self.editor.render(&damage);
            let back = self.editor.back();
            for r in damage.iter().map(|r| r.intersect(&full)).filter(|r| !r.is_empty()) {
                for y in r.t..r.b {
                    let row = (y * ew) as usize;
                    for x in r.l..r.r {
                        let i = (row + x as usize) * 4;
                        self.frame[row + x as usize] = u32::from_le_bytes([back[i], back[i + 1], back[i + 2], 0]);
                    }
                }
            }
        }
        let size = self.window.inner_size();
        let (Some(ww), Some(wh)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else { return };
        if self.surface.resize(ww, wh).is_err() {
            return;
        }
        let Ok(mut buf) = self.surface.buffer_mut() else { return };
        let (ww, wh) = (size.width as usize, size.height as usize);
        if (ww, wh) == (ew as usize, eh as usize) {
            buf.copy_from_slice(&self.frame);
        } else {
            // Nearest-neighbor scale when the compositor sized us differently.
            for y in 0..wh {
                let sy = y * eh as usize / wh;
                for x in 0..ww {
                    buf[y * ww + x] = self.frame[sy * ew as usize + x * ew as usize / ww];
                }
            }
        }
        let _ = buf.present();
    }
}
