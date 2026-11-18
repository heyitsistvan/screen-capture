//! A pinned capture: a borderless, always-on-top window showing an image, for
//! comparing it with the screen (lower the opacity and lay it over the live
//! area, nudging it into place with the arrow keys) or with other pins. This
//! is the view state, layout and drawing; each platform's window feeds it
//! input.
//!
//! While the mouse is over a pin it shows a small toolbar (zoom, opacity,
//! edit, copy, save, close) and a resize grip; its edges resize it, keeping
//! the picture's proportions. A pin keeps its drawings apart from the
//! picture, so editing it again can still move or delete them.

use std::time::{Duration, Instant};

use crate::annotate::{Rgba, Shape};
use crate::text::{self, Fonts};
use crate::ui::{Canvas, Icon};
use crate::util::{self, Rect};

/// The frame around the image, so a pin stands out from the live screen.
pub const BORDER: i32 = 1;
const FRAME: Rgba = Rgba(10, 132, 255, 255);
/// Wheel zoom steps.
const ZOOMS: [f32; 14] = [0.1, 0.25, 0.33, 0.5, 0.67, 0.75, 1.0, 1.25, 1.5, 2.0, 3.0, 4.0, 6.0, 8.0];
const MIN_ZOOM: f32 = 0.05;
const MAX_ZOOM: f32 = 8.0;
/// Opacity presets: the toolbar button cycles through them.
pub const OPACITIES: [f32; 4] = [1.0, 0.8, 0.6, 0.4];
const MIN_OPACITY: f32 = 0.1;
const HINT_TIME: Duration = Duration::from_secs(4);
const BADGE_TIME: Duration = Duration::from_millis(1200);
const PANEL: Rgba = Rgba(24, 25, 28, 225);
const WHITE: Rgba = Rgba(255, 255, 255, 255);

/// A toolbar button.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PinBtn {
    /// Shows the zoom when it isn't 100%; resets it.
    Zoom,
    /// Cycles the opacity presets.
    Opacity,
    /// Reopens it in the editor.
    Edit,
    Copy,
    Save,
    Close,
}

/// An edge or corner that resizes the pin when dragged.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Edge {
    N,
    S,
    E,
    W,
    NE,
    NW,
    SE,
    SW,
}

pub struct Pin {
    w: i32,
    h: i32,
    /// The picture as shown: drawings included.
    pixels: Vec<u8>,
    /// The picture without drawings, and the drawings (relative to its
    /// top-left corner), for editing it again.
    layers: Option<(Vec<u8>, Vec<Shape>)>,
    zoom: f32,
    opacity: f32,
    /// The platform can change window opacity (not Wayland).
    opacity_supported: bool,
    /// The image at the size it's shown, when that isn't 1:1.
    scaled: Option<(i32, i32, Vec<u8>)>,
    /// Lines explaining the controls, shown when the pin opens.
    hint: &'static [&'static str],
    hint_until: Option<Instant>,
    /// A short-lived label for the zoom or opacity just set.
    badge: Option<(String, Instant)>,
    /// The mouse position while it's over the pin (window coordinates).
    hover: Option<(i32, i32)>,
    /// The toolbar button being clicked.
    pressed: Option<PinBtn>,
}

impl Pin {
    pub fn new(w: i32, h: i32, pixels: Vec<u8>, hint: &'static [&'static str], opacity_supported: bool) -> Pin {
        Pin {
            w,
            h,
            pixels,
            layers: None,
            zoom: 1.0,
            opacity: 1.0,
            opacity_supported,
            scaled: None,
            hint,
            hint_until: Some(Instant::now() + HINT_TIME),
            badge: None,
            hover: None,
            pressed: None,
        }
    }

    /// The pinned image, BGRA.
    pub fn image(&self) -> (i32, i32, &[u8]) {
        (self.w, self.h, &self.pixels)
    }

    /// Keeps the picture without drawings and the drawings, for editing.
    pub fn set_layers(&mut self, layers: Option<(Vec<u8>, Vec<Shape>)>) {
        self.layers = layers.filter(|(base, _)| base.len() == self.pixels.len());
    }

    /// The picture without drawings and the drawings (relative to its
    /// top-left corner); a pin without them is all picture.
    pub fn layers(&self) -> (Vec<u8>, Vec<Shape>) {
        self.layers.clone().unwrap_or_else(|| (self.pixels.clone(), Vec::new()))
    }

    /// Shows an edited version, keeping the zoom and opacity.
    pub fn replace(&mut self, w: i32, h: i32, pixels: Vec<u8>, layers: Option<(Vec<u8>, Vec<Shape>)>) {
        (self.w, self.h, self.pixels) = (w, h, pixels);
        self.scaled = None;
        self.set_layers(layers);
    }

    pub fn zoom(&self) -> f32 {
        self.zoom
    }

    pub fn opacity(&self) -> f32 {
        self.opacity
    }

    /// The image's size on screen at the current zoom.
    fn shown_size(&self) -> (i32, i32) {
        (((self.w as f32 * self.zoom).round() as i32).max(1), ((self.h as f32 * self.zoom).round() as i32).max(1))
    }

    /// The window size, frame included.
    pub fn size(&self) -> (i32, i32) {
        let (w, h) = self.shown_size();
        (w + 2 * BORDER, h + 2 * BORDER)
    }

    /// Zooms `steps` wheel steps in (positive) or out, keeping the image point
    /// under window position (`ax`, `ay`) where it is on screen. Returns how
    /// far the window must move (its new size is [`size`](Self::size)), or
    /// `None` if the zoom is already at its limit.
    pub fn zoom_by(&mut self, steps: i32, ax: i32, ay: i32) -> Option<(i32, i32)> {
        let mut z = self.zoom;
        for _ in 0..steps.unsigned_abs() {
            let next = if steps > 0 {
                ZOOMS.iter().copied().find(|&s| s > z * 1.001)
            } else {
                ZOOMS.iter().rev().copied().find(|&s| s < z * 0.999)
            };
            z = next.unwrap_or(z);
        }
        self.zoom_to(z, ax, ay)
    }

    /// Back to 100%, like [`zoom_by`](Self::zoom_by).
    pub fn actual_size(&mut self, ax: i32, ay: i32) -> Option<(i32, i32)> {
        self.zoom_to(1.0, ax, ay)
    }

    fn zoom_to(&mut self, z: f32, ax: i32, ay: i32) -> Option<(i32, i32)> {
        if (z - self.zoom).abs() < 1e-4 {
            return None;
        }
        let (ow, oh) = self.shown_size();
        self.zoom = z;
        self.scaled = None;
        let (nw, nh) = self.shown_size();
        // Where the anchor's image point lands after zooming, relative to the
        // window; the window moves by the difference.
        let shift = |a: i32, old: i32, new: i32| {
            let f = (a - BORDER).clamp(0, old) as f32 / old as f32;
            (a - BORDER) - (f * new as f32).round() as i32
        };
        self.zoom_badge();
        Some((shift(ax, ow, nw), shift(ay, oh, nh)))
    }

    fn zoom_badge(&mut self) {
        self.set_badge(format!("{}%", (self.zoom * 100.0).round()));
    }

    /// The user resized the window to `ww` × `wh`: the zoom follows its
    /// width (`by_width`: `Some(true)`), its height (`Some(false)`), or fits
    /// the picture inside (`None`). False if the size already matches.
    pub fn fit_to(&mut self, ww: i32, wh: i32, by_width: Option<bool>) -> bool {
        if (ww, wh) == self.size() {
            return false;
        }
        let (zw, zh) = ((ww - 2 * BORDER) as f32 / self.w as f32, (wh - 2 * BORDER) as f32 / self.h as f32);
        let z = match by_width {
            Some(true) => zw,
            Some(false) => zh,
            None => zw.min(zh),
        }
        .clamp(MIN_ZOOM, MAX_ZOOM);
        if (z - self.zoom).abs() < 1e-4 {
            return false;
        }
        self.zoom = z;
        self.scaled = None;
        self.zoom_badge();
        true
    }

    /// The height that keeps the picture's proportions at window width `ww`
    /// (or the width at window height `wh`), for resizing by an edge.
    pub fn keep_aspect(&self, ww: i32, wh: i32, by_width: bool) -> (i32, i32) {
        let (iw, ih) = ((ww - 2 * BORDER).max(1) as f32, (wh - 2 * BORDER).max(1) as f32);
        if by_width {
            (ww, (iw * self.h as f32 / self.w as f32).round().max(1.0) as i32 + 2 * BORDER)
        } else {
            ((ih * self.w as f32 / self.h as f32).round().max(1.0) as i32 + 2 * BORDER, wh)
        }
    }

    /// Changes the opacity by `steps` tenths; false if already at the limit.
    pub fn opacity_by(&mut self, steps: i32) -> bool {
        let tenths = ((self.opacity * 10.0).round() as i32 + steps).clamp((MIN_OPACITY * 10.0) as i32, 10);
        let o = tenths as f32 / 10.0;
        if (o - self.opacity).abs() < 0.01 {
            return false;
        }
        self.set_opacity(o);
        true
    }

    /// The next lower opacity preset, or back to opaque.
    pub fn next_opacity(&mut self) {
        let o = OPACITIES.iter().copied().find(|&o| o < self.opacity - 0.01).unwrap_or(1.0);
        self.set_opacity(o);
    }

    pub fn set_opacity(&mut self, o: f32) {
        self.opacity = o.clamp(MIN_OPACITY, 1.0);
        self.set_badge(format!("Opacity {}%", (self.opacity * 100.0).round()));
    }

    /// Shows a short message in the corner (e.g. "Copied").
    pub fn set_badge(&mut self, text: String) {
        self.badge = Some((text, Instant::now() + BADGE_TIME));
    }

    /// Shows the controls again.
    #[cfg_attr(windows, allow(dead_code))] // Windows has a right-click menu instead
    pub fn show_hint(&mut self) {
        self.hint_until = Some(Instant::now() + HINT_TIME);
    }

    /// When a label disappears next (the window should repaint then).
    pub fn next_deadline(&self) -> Option<Instant> {
        [self.hint_until, self.badge.as_ref().map(|b| b.1)].into_iter().flatten().min()
    }

    /// Drops expired labels; true if anything changed.
    pub fn expire(&mut self, now: Instant) -> bool {
        let mut changed = false;
        if self.hint_until.is_some_and(|t| t <= now) {
            self.hint_until = None;
            changed = true;
        }
        if self.badge.as_ref().is_some_and(|b| b.1 <= now) {
            self.badge = None;
            changed = true;
        }
        changed
    }

    // ----- mouse ----------------------------------------------------------

    /// Where the mouse is over the pin (`None` once it left). True if the
    /// pin needs a repaint (toolbar shown, hidden or a button highlighted).
    pub fn set_hover(&mut self, pos: Option<(i32, i32)>, ww: i32, wh: i32, scale: f32) -> bool {
        let before = (self.hover.is_some(), self.hover.and_then(|(x, y)| self.button_at(x, y, ww, wh, scale)));
        self.hover = pos;
        let after = (self.hover.is_some(), self.hover.and_then(|(x, y)| self.button_at(x, y, ww, wh, scale)));
        before != after
    }

    pub fn set_pressed(&mut self, b: Option<PinBtn>) {
        self.pressed = b;
    }

    pub fn pressed(&self) -> Option<PinBtn> {
        self.pressed
    }

    /// How close to an edge resizes instead of moving.
    fn grab(scale: f32) -> i32 {
        (6.0 * scale).round().max(4.0) as i32
    }

    /// The toolbar buttons in a `ww` × `wh` window, right to left from the
    /// top-right corner; on a small pin the less important ones drop out,
    /// the close button stays.
    pub fn buttons(&self, ww: i32, wh: i32, scale: f32) -> Vec<(PinBtn, Rect)> {
        let _ = wh;
        let b = (24.0 * scale).round() as i32;
        let pad = (3.0 * scale).round() as i32;
        let inset = Self::grab(scale) + (2.0 * scale).round() as i32;
        let pill = (46.0 * scale).round() as i32;
        let mut wanted = vec![PinBtn::Close, PinBtn::Edit, PinBtn::Save, PinBtn::Copy];
        if self.opacity_supported {
            wanted.push(PinBtn::Opacity);
        }
        if (self.zoom - 1.0).abs() > 1e-4 {
            wanted.push(PinBtn::Zoom);
        }
        let room = ww - 2 * inset - 2 * pad;
        let mut out = Vec::new();
        let mut x = ww - inset - pad;
        for btn in wanted {
            let w = if btn == PinBtn::Zoom { pill } else { b };
            if !out.is_empty() && ww - inset - pad - (x - w) > room {
                break;
            }
            x -= w;
            out.push((btn, Rect::xywh(x, inset + pad, w, b)));
        }
        out
    }

    /// The toolbar button at a window position (only while it's shown).
    pub fn button_at(&self, x: i32, y: i32, ww: i32, wh: i32, scale: f32) -> Option<PinBtn> {
        self.hover?;
        self.buttons(ww, wh, scale).into_iter().find(|(_, r)| r.contains(x, y)).map(|(b, _)| b)
    }

    /// The edge or corner at a window position, if it's close enough to
    /// resize from (corners grab a little further in).
    pub fn edge_at(&self, x: i32, y: i32, ww: i32, wh: i32, scale: f32) -> Option<Edge> {
        let g = Self::grab(scale);
        let c = g * 2;
        let (l, r, t, b) = (x < g, x >= ww - g, y < g, y >= wh - g);
        let (lc, rc, tc, bc) = (x < c, x >= ww - c, y < c, y >= wh - c);
        Some(match () {
            _ if (t && lc) || (l && tc) => Edge::NW,
            _ if (t && rc) || (r && tc) => Edge::NE,
            _ if (b && lc) || (l && bc) => Edge::SW,
            _ if (b && rc) || (r && bc) => Edge::SE,
            _ if t => Edge::N,
            _ if b => Edge::S,
            _ if l => Edge::W,
            _ if r => Edge::E,
            _ => return None,
        })
    }

    // ----- drawing --------------------------------------------------------

    /// Draws the pin into a BGRA buffer the size of its window (which may
    /// differ from [`size`](Self::size) while it's being resized: the picture
    /// then fits inside, keeping its proportions).
    pub fn render(&mut self, out: &mut [u8], ow: i32, oh: i32, fonts: &Fonts, scale: f32) {
        let inner = Rect::new(BORDER, BORDER, ow - BORDER, oh - BORDER);
        let mut cv = Canvas::new(out, ow, oh);
        cv.fill_px(Rect::new(0, 0, ow, oh), FRAME);
        if inner.is_empty() {
            return;
        }
        let s = (inner.w() as f32 / self.w as f32).min(inner.h() as f32 / self.h as f32);
        let (dw, dh) = (((self.w as f32 * s).round() as i32).clamp(1, inner.w()), ((self.h as f32 * s).round() as i32).clamp(1, inner.h()));
        if (dw, dh) != (inner.w(), inner.h()) {
            cv.fill_px(inner, Rgba(32, 33, 36, 255));
        }
        let src: &[u8] = if (dw, dh) == (self.w, self.h) {
            &self.pixels
        } else {
            if self.scaled.as_ref().is_none_or(|s| (s.0, s.1) != (dw, dh)) {
                self.scaled = Some((dw, dh, util::resize(&self.pixels, self.w, self.h, dw, dh)));
            }
            &self.scaled.as_ref().expect("just set").2
        };
        let (x0, y0) = (inner.l + (inner.w() - dw) / 2, inner.t + (inner.h() - dh) / 2);
        let n = dw as usize * 4;
        for y in 0..dh {
            let d = (((y + y0) * ow + x0) * 4) as usize;
            let s = (y * dw) as usize * 4;
            cv.buf[d..d + n].copy_from_slice(&src[s..s + n]);
        }

        let px = 12.0 * scale;
        let pad = (6.0 * scale).round() as i32;
        let (_, lh) = text::line_metrics(&fonts.ui, px);
        let mut y = BORDER + pad;
        let label = |cv: &mut Canvas, lines: &[&str], y: i32| -> i32 {
            let tw = lines.iter().map(|l| text::line_width(&fonts.ui, l, px)).fold(0.0, f32::max);
            let h = (lh * lines.len() as f32).ceil() as i32 + pad;
            let r = Rect::xywh(BORDER + pad, y, tw.ceil() as i32 + 2 * pad, h);
            cv.fill_rrect(r, 5.0 * scale, PANEL);
            for (i, l) in lines.iter().enumerate() {
                let ty = r.t as f32 + pad as f32 / 2.0 + lh * i as f32;
                cv.text((r.l + pad) as f32, ty, l, px, WHITE, &fonts.ui);
            }
            r.b + pad / 2
        };
        if let Some((text, _)) = &self.badge {
            y = label(&mut cv, &[text.as_str()], y);
        }
        if self.hint_until.is_some() {
            label(&mut cv, self.hint, y);
        }
        if self.hover.is_some() {
            self.draw_controls(&mut cv, ow, oh, fonts, scale);
        }
    }

    /// The toolbar and the resize grip, while the mouse is over the pin.
    fn draw_controls(&self, cv: &mut Canvas, ow: i32, oh: i32, fonts: &Fonts, scale: f32) {
        let buttons = self.buttons(ow, oh, scale);
        let hot = self.hover.and_then(|(x, y)| self.button_at(x, y, ow, oh, scale));
        if let Some(bar) = buttons.iter().map(|b| b.1).reduce(|a, b| a.union(&b)) {
            let pad = (3.0 * scale).round() as i32;
            cv.fill_rrect(bar.inflate(pad), 7.0 * scale, PANEL);
        }
        for (btn, r) in &buttons {
            let lit = hot == Some(*btn);
            if lit {
                let c = if *btn == PinBtn::Close { Rgba(232, 17, 35, 255) } else { Rgba(255, 255, 255, 45) };
                cv.fill_rrect(*r, 5.0 * scale, c);
            }
            let (cx, cy) = ((r.l + r.r) as f32 / 2.0, (r.t + r.b) as f32 / 2.0);
            let icon = 16.0 * scale;
            match btn {
                PinBtn::Zoom => {
                    let t = format!("{}%", (self.zoom * 100.0).round());
                    let fpx = 12.0 * scale;
                    let tw = text::line_width(&fonts.bold, &t, fpx);
                    let (_, lh) = text::line_metrics(&fonts.bold, fpx);
                    cv.text(cx - tw / 2.0, cy - lh / 2.0, &t, fpx, WHITE, &fonts.bold);
                }
                PinBtn::Opacity => {
                    let rad = 6.0 * scale;
                    cv.stroke_circle(cx, cy, rad, 1.6 * scale, WHITE);
                    cv.fill_half_circle(cx, cy, rad, WHITE);
                }
                PinBtn::Edit => cv.icon(Icon::Pen, cx, cy, icon, WHITE, fonts),
                PinBtn::Copy => cv.icon(Icon::Copy, cx, cy, icon, WHITE, fonts),
                PinBtn::Save => cv.icon(Icon::Save, cx, cy, icon, WHITE, fonts),
                PinBtn::Close => cv.icon(Icon::Close, cx, cy, icon, WHITE, fonts),
            }
        }
        // The resize grip in the bottom-right corner.
        let g = (14.0 * scale).round() as i32;
        let m = (3.0 * scale).round() as i32;
        if ow > 3 * g && oh > 3 * g {
            let r = Rect::xywh(ow - BORDER - m - g, oh - BORDER - m - g, g, g);
            cv.fill_rrect(r, 3.0 * scale, PANEL);
            let w = 1.4 * scale;
            for k in [0.3f32, 0.6] {
                let d = g as f32 * k;
                cv.line(r.r as f32 - 3.0 * scale - d, r.b as f32 - 3.0 * scale, r.r as f32 - 3.0 * scale, r.b as f32 - 3.0 * scale - d, w, WHITE);
            }
        }
    }
}

/// Zoom anchoring, zoom and opacity limits, resizing, the toolbar layout and
/// drawing (debug self-test).
#[cfg(debug_assertions)]
pub fn selftest(fonts: &Fonts, preview: &std::path::Path) -> Result<String, String> {
    let check = |ok: bool, what: &str| if ok { Ok(()) } else { Err(what.to_string()) };
    let (w, h) = (40, 30);
    let px: Vec<u8> = (0..w * h).flat_map(|i| [(i % 251) as u8, (i / 7 % 251) as u8, 100, 255]).collect();
    let at = |x: i32, y: i32| px[((y * w + x) * 4) as usize..((y * w + x) * 4) as usize + 4].to_vec();
    let mut pin = Pin::new(w, h, px.clone(), &["hint"], true);
    check(pin.size() == (w + 2 * BORDER, h + 2 * BORDER), "wrong window size at 100%")?;

    // Zooming keeps the image point under the cursor where it is on screen.
    let win = (500, 300);
    let (ax, ay) = (BORDER + 10, BORDER + 21);
    let before = (win.0 + ax, win.1 + ay);
    for steps in [1, 2, -3, -2] {
        let z0 = pin.zoom();
        let (dx, dy) = pin.zoom_by(steps, ax, ay).ok_or("zoom didn't change")?;
        let z1 = pin.zoom();
        let after = (
            win.0 + dx + BORDER + ((ax - BORDER) as f32 * z1 / z0).round() as i32,
            win.1 + dy + BORDER + ((ay - BORDER) as f32 * z1 / z0).round() as i32,
        );
        check((after.0 - before.0).abs() <= 1 && (after.1 - before.1).abs() <= 1, "zoom moved the point under the cursor")?;
        pin.actual_size(ax, ay);
    }
    while pin.zoom_by(1, 0, 0).is_some() {}
    check(pin.zoom() == 8.0, "zoom in doesn't stop at 800%")?;
    while pin.zoom_by(-1, 0, 0).is_some() {}
    check(pin.zoom() == 0.1, "zoom out doesn't stop at 10%")?;
    pin.actual_size(0, 0);
    check(pin.size() == (w + 2 * BORDER, h + 2 * BORDER), "100% didn't restore the size")?;

    while pin.opacity_by(-1) {}
    check((pin.opacity() - MIN_OPACITY).abs() < 0.01, "opacity doesn't stop at 10%")?;
    pin.opacity_by(20);
    check(pin.opacity() == 1.0, "opacity doesn't stop at 100%")?;
    let cycle: Vec<f32> = (0..4).map(|_| {
        pin.next_opacity();
        pin.opacity()
    }).collect();
    check(cycle == [0.8, 0.6, 0.4, 1.0], "the opacity button doesn't cycle 80, 60, 40, 100%")?;

    // Resizing by an edge: proportions kept, the zoom follows, and a wheel
    // step goes on from there.
    check(pin.keep_aspect(82, 999, true) == (82, 62), "keeping proportions by width")?;
    check(pin.keep_aspect(999, 47, false) == (62, 47), "keeping proportions by height")?;
    check(!pin.fit_to(pin.size().0, pin.size().1, None), "fitting to its own size changed the zoom")?;
    check(pin.fit_to(82, 62, None) && pin.zoom() == 2.0, "resizing to 200% didn't zoom to 200%")?;
    pin.actual_size(0, 0);
    check(pin.fit_to(82, 32, Some(true)) && pin.zoom() == 2.0, "dragging the right edge alone didn't zoom")?;
    pin.actual_size(0, 0);
    check(pin.fit_to(42, 62, Some(false)) && pin.zoom() == 2.0, "dragging the bottom edge alone didn't zoom")?;
    pin.zoom_by(1, 0, 0);
    check(pin.zoom() == 3.0, "the wheel doesn't continue from a resized zoom")?;
    pin.actual_size(0, 0);

    // Toolbar: hidden until hovered; drops buttons on small pins but keeps
    // close; edges and corners resize.
    let (ww, wh) = (400, 300);
    check(pin.button_at(380, 20, ww, wh, 1.0).is_none(), "toolbar buttons work while hidden")?;
    pin.set_hover(Some((200, 150)), ww, wh, 1.0);
    let names = |pin: &Pin, ww| pin.buttons(ww, wh, 1.0).into_iter().map(|b| b.0).collect::<Vec<_>>();
    check(names(&pin, ww) == [PinBtn::Close, PinBtn::Edit, PinBtn::Save, PinBtn::Copy, PinBtn::Opacity], "toolbar at 100%")?;
    pin.zoom_by(1, 0, 0);
    check(names(&pin, ww).contains(&PinBtn::Zoom), "no zoom button when zoomed")?;
    check(names(&pin, 60) == [PinBtn::Close], "a small pin should keep only the close button")?;
    let close = pin.buttons(ww, wh, 1.0)[0].1;
    check(pin.button_at(close.center().0, close.center().1, ww, wh, 1.0) == Some(PinBtn::Close), "close button hit")?;
    check(pin.edge_at(200, 1, ww, wh, 1.0) == Some(Edge::N) && pin.edge_at(398, 298, ww, wh, 1.0) == Some(Edge::SE), "edges")?;
    check(pin.edge_at(200, 150, ww, wh, 1.0).is_none(), "the middle isn't an edge")?;
    check(close.intersect(&Rect::new(ww - Pin::grab(1.0), 0, ww, wh)).is_empty(), "the toolbar overlaps the resize edge")?;
    pin.actual_size(0, 0);
    pin.set_hover(None, ww, wh, 1.0);

    // Editing: the layers come back, and a replaced picture keeps the zoom.
    check(pin.layers().0 == pin.image().2 && pin.layers().1.is_empty(), "a plain pin should be all picture")?;
    pin.zoom_by(3, 0, 0);
    pin.replace(w, h, px.clone(), Some((vec![0u8; px.len()], Vec::new())));
    check(pin.zoom() == 2.0 && pin.layers().0.iter().all(|&b| b == 0), "replacing lost the zoom or the layers")?;
    pin.set_layers(Some((vec![0u8; 4], Vec::new())));
    check(pin.layers().0 == pin.image().2, "layers of the wrong size were kept")?;
    pin.actual_size(0, 0);

    // Drawn 1:1 inside the frame, and 2× with each pixel doubled.
    pin.expire(Instant::now() + Duration::from_secs(60));
    let (ww, wh) = pin.size();
    let mut out = vec![0u8; (ww * wh * 4) as usize];
    pin.render(&mut out, ww, wh, fonts, 1.0);
    let get = |out: &[u8], ww: i32, x: i32, y: i32| out[((y * ww + x) * 4) as usize..((y * ww + x) * 4) as usize + 4].to_vec();
    check(get(&out, ww, 0, 0) == FRAME.bgra().to_le_bytes().to_vec(), "no frame around the pin")?;
    check(get(&out, ww, BORDER + 5, BORDER + 7) == at(5, 7), "1:1 drawing doesn't match the image")?;
    pin.zoom_by(3, 0, 0);
    check(pin.zoom() == 2.0, "expected 200%")?;
    pin.expire(Instant::now() + Duration::from_secs(60));
    let (ww, wh) = pin.size();
    let mut out = vec![0u8; (ww * wh * 4) as usize];
    pin.render(&mut out, ww, wh, fonts, 1.0);
    let same = [(0, 0), (1, 0), (0, 1), (1, 1)].iter().all(|(ox, oy)| get(&out, ww, BORDER + 10 + ox, BORDER + 14 + oy) == at(5, 7));
    check(same, "200% drawing doesn't double the pixels")?;

    // A preview with the toolbar (close button lit), grip and labels.
    let (w, h) = (360, 200);
    let px: Vec<u8> = (0..w * h).flat_map(|i| [(i % w * 255 / w) as u8, (i / w * 255 / h) as u8, 160, 255]).collect();
    let mut pin = Pin::new(w, h, px, &["Drag to move, drag the edges to resize", "Wheel: zoom · Ctrl+wheel: opacity"], true);
    pin.zoom_by(1, 0, 0);
    let (ww, wh) = pin.size();
    let close = pin.buttons(ww, wh, 1.0)[0].1.center();
    pin.set_hover(Some(close), ww, wh, 1.0);
    let mut out = vec![0u8; (ww * wh * 4) as usize];
    pin.render(&mut out, ww, wh, fonts, 1.0);
    let _ = util::encode_png(ww, wh, &out).map(|png| std::fs::write(preview, png));
    Ok("zoom 10–800% around the cursor, resize keeps proportions, opacity cycle, toolbar and edges, exact drawing".into())
}
