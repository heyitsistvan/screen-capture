//! X11 screen access: monitors, the pointer, screen pixels (GetImage on the
//! root window), the window list for click-to-select, and a frame grabber
//! for GIF recording that draws the real cursor (XFixes).
//!
//! On Wayland, screenshots come from the desktop portal instead (see
//! `portal.rs`).

use x11rb::connection::Connection;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, ImageFormat, ImageOrder, Window};
use x11rb::rust_connection::RustConnection;

use crate::editor::Screen;
use crate::util::Rect;

pub struct X11 {
    pub conn: RustConnection,
    pub root: Window,
}

impl X11 {
    pub fn connect() -> Option<X11> {
        let (conn, screen) = x11rb::connect(None).ok()?;
        let root = conn.setup().roots.get(screen)?.root;
        // XFixes must be initialized before its requests can be used.
        let _ = conn.xfixes_query_version(5, 0).ok()?.reply();
        Some(X11 { conn, root })
    }

    fn root_rect(&self) -> Rect {
        let s = self.conn.setup().roots.iter().find(|s| s.root == self.root);
        s.map_or(Rect::xywh(0, 0, 1920, 1080), |s| Rect::xywh(0, 0, s.width_in_pixels as i32, s.height_in_pixels as i32))
    }

    pub fn monitors(&self) -> Vec<Rect> {
        let mons = self.conn.randr_get_monitors(self.root, true).ok().and_then(|c| c.reply().ok());
        let v: Vec<Rect> = mons
            .map(|m| m.monitors.iter().map(|m| Rect::xywh(m.x as i32, m.y as i32, m.width as i32, m.height as i32)).collect())
            .unwrap_or_default();
        if v.is_empty() {
            vec![self.root_rect()]
        } else {
            v
        }
    }

    pub fn pointer(&self) -> (i32, i32) {
        self.conn
            .query_pointer(self.root)
            .ok()
            .and_then(|c| c.reply().ok())
            .map_or((0, 0), |p| (p.root_x as i32, p.root_y as i32))
    }

    /// Opaque BGRA pixels of a root-window rectangle.
    pub fn capture(&self, r: Rect) -> Option<Vec<u8>> {
        let r = r.intersect(&self.root_rect());
        if r.is_empty() {
            return None;
        }
        let img = self
            .conn
            .get_image(ImageFormat::Z_PIXMAP, self.root, r.l as i16, r.t as i16, r.w() as u16, r.h() as u16, !0)
            .ok()?
            .reply()
            .ok()?;
        let setup = self.conn.setup();
        let bpp = setup.pixmap_formats.iter().find(|f| f.depth == img.depth).map_or(32, |f| f.bits_per_pixel);
        if bpp != 32 || setup.image_byte_order != ImageOrder::LSB_FIRST {
            return None; // only the universal 24/32-bit little-endian layout
        }
        let mut px = img.data;
        px.truncate(r.w() as usize * r.h() as usize * 4);
        for a in px.iter_mut().skip(3).step_by(4) {
            *a = 255;
        }
        Some(px)
    }

    fn atom(&self, name: &str) -> Option<u32> {
        Some(self.conn.intern_atom(false, name.as_bytes()).ok()?.reply().ok()?.atom)
    }

    fn cardinals(&self, win: Window, prop: u32, ty: impl Into<u32>) -> Vec<u32> {
        self.conn
            .get_property(false, win, prop, ty.into(), 0, 1024)
            .ok()
            .and_then(|c| c.reply().ok())
            .and_then(|r| r.value32().map(|v| v.collect()))
            .unwrap_or_default()
    }

    /// Visible top-level windows, topmost first, in root coordinates and
    /// including server-side decorations.
    pub fn windows(&self) -> Vec<Rect> {
        let (Some(stacking), Some(state), Some(hidden), Some(frame), Some(gtk_frame), Some(pid)) = (
            self.atom("_NET_CLIENT_LIST_STACKING"),
            self.atom("_NET_WM_STATE"),
            self.atom("_NET_WM_STATE_HIDDEN"),
            self.atom("_NET_FRAME_EXTENTS"),
            self.atom("_GTK_FRAME_EXTENTS"),
            self.atom("_NET_WM_PID"),
        ) else {
            return Vec::new();
        };
        let me = std::process::id();
        let mut out = Vec::new();
        for win in self.cardinals(self.root, stacking, AtomEnum::WINDOW).into_iter().rev() {
            if self.cardinals(win, state, AtomEnum::ATOM).contains(&hidden)
                || self.cardinals(win, pid, AtomEnum::CARDINAL).first() == Some(&me)
            {
                continue;
            }
            let Some(geo) = self.conn.get_geometry(win).ok().and_then(|c| c.reply().ok()) else { continue };
            let Some(pos) = self.conn.translate_coordinates(win, self.root, 0, 0).ok().and_then(|c| c.reply().ok()) else {
                continue;
            };
            let mut r = Rect::xywh(pos.dst_x as i32, pos.dst_y as i32, geo.width as i32, geo.height as i32);
            // Window-manager decorations around the window...
            if let [l, rt, t, b] = self.cardinals(win, frame, AtomEnum::CARDINAL)[..] {
                r = Rect::new(r.l - l as i32, r.t - t as i32, r.r + rt as i32, r.b + b as i32);
            }
            // ...or invisible client-side shadows inside it.
            if let [l, rt, t, b] = self.cardinals(win, gtk_frame, AtomEnum::CARDINAL)[..] {
                r = Rect::new(r.l + l as i32, r.t + t as i32, r.r - rt as i32, r.b - b as i32);
            }
            if r.w() >= 8 && r.h() >= 8 {
                out.push(r);
            }
        }
        out
    }

    /// Draws the mouse cursor onto BGRA words whose (0, 0) is at `(ox, oy)`.
    fn draw_cursor(&self, px: &mut [u32], w: i32, h: i32, ox: i32, oy: i32) {
        let Some(cur) = self.conn.xfixes_get_cursor_image().ok().and_then(|c| c.reply().ok()) else { return };
        let (cw, ch) = (cur.width as i32, cur.height as i32);
        let (x0, y0) = (cur.x as i32 - cur.xhot as i32 - ox, cur.y as i32 - cur.yhot as i32 - oy);
        for cy in 0..ch {
            for cx in 0..cw {
                let (x, y) = (x0 + cx, y0 + cy);
                if x < 0 || y < 0 || x >= w || y >= h {
                    continue;
                }
                // Premultiplied ARGB.
                let s = cur.cursor_image[(cy * cw + cx) as usize];
                let a = s >> 24;
                if a == 0 {
                    continue;
                }
                let d = &mut px[(y * w + x) as usize];
                let blend = |shift: u32| {
                    let sc = (s >> shift) & 0xFF;
                    let dc = (*d >> shift) & 0xFF;
                    (sc + dc * (255 - a) / 255).min(255) << shift
                };
                *d = 0xFF00_0000 | blend(16) | blend(8) | blend(0);
            }
        }
    }
}

/// Repeatedly captures one rectangle for GIF recording.
pub struct Grabber {
    x: X11,
    w: i32,
    h: i32,
}

impl Grabber {
    pub fn new(w: i32, h: i32) -> Option<Grabber> {
        Some(Grabber { x: X11::connect()?, w, h })
    }

    /// Pixels as little-endian BGRA words (alpha 255).
    pub fn grab(&mut self, x: i32, y: i32, cursor: bool) -> Vec<u32> {
        let r = Rect::xywh(x, y, self.w, self.h);
        let mut px: Vec<u32> = match self.x.capture(r) {
            Some(bytes) if bytes.len() == self.w as usize * self.h as usize * 4 => {
                bytes.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], 255])).collect()
            }
            _ => vec![0xFF00_0000; self.w as usize * self.h as usize],
        };
        if cursor {
            self.x.draw_cursor(&mut px, self.w, self.h, x, y);
        }
        px
    }
}

/// The monitor under the pointer as an editor screen (X11).
pub fn monitor_screen(x: &X11) -> Option<Screen> {
    let (mx, my) = x.pointer();
    let monitors = x.monitors();
    let mon = monitors.iter().copied().find(|m| m.contains(mx, my)).or_else(|| monitors.first().copied())?;
    let pixels = x.capture(mon)?;
    let local = Rect::new(0, 0, mon.w(), mon.h());
    let windows = x
        .windows()
        .into_iter()
        .map(|r| r.offset(-mon.l, -mon.t).intersect(&local))
        .filter(|r| !r.is_empty())
        .collect();
    Some(Screen {
        w: mon.w(),
        h: mon.h(),
        pixels,
        monitors: vec![local],
        windows,
        mouse: (mx - mon.l, my - mon.t),
        origin: (mon.l, mon.t),
    })
}

/// The whole X11 screen as an editor screen (debug self-test).
#[cfg(debug_assertions)]
pub fn screen_for_test() -> Option<Screen> {
    let x = X11::connect()?;
    let root = x.root_rect();
    let pixels = x.capture(root)?;
    Some(Screen { w: root.w(), h: root.h(), pixels, monitors: x.monitors(), windows: x.windows(), mouse: (0, 0), origin: (0, 0) })
}
