//! Annotation shapes and their rasterization.
//!
//! All pixel buffers in this app are 32-bit BGRA, the native GDI DIB layout.
//! tiny-skia expects RGBA, so colors are handed to it with red and blue
//! swapped; blending is per-channel, so results land correctly in BGRA.

use std::sync::Arc;

use tiny_skia::{
    Color, FillRule, LineCap, LineJoin, Paint, Path, PathBuilder, PixmapMut, Rect as SkRect, Stroke, Transform,
};

use crate::ocr;
use crate::text::{self, Fonts};
use crate::util::Rect;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgba(pub u8, pub u8, pub u8, pub u8);

impl Rgba {
    /// Color for drawing into a BGRA buffer through tiny-skia.
    pub fn sk(self) -> Color {
        Color::from_rgba8(self.2, self.1, self.0, self.3)
    }

    pub fn alpha(self, a: u8) -> Rgba {
        Rgba(self.0, self.1, self.2, a)
    }

    /// Packed pixel as it sits in a BGRA buffer (little-endian u32).
    pub fn bgra(self) -> u32 {
        u32::from_le_bytes([self.2, self.1, self.0, self.3])
    }
}

pub type Pt = (f32, f32);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    Pen,
    Line,
    Arrow,
    Rect,
    Ellipse,
    Marker,
    Text,
    Step,
    Pixelate,
}

/// `outline` is the color of a halo drawn around the shape (see
/// [`auto_outline`]) so it stays visible on any background.
#[derive(Clone, Debug)]
pub enum Shape {
    Pen { pts: Vec<Pt>, color: Rgba, width: f32, outline: Option<Rgba> },
    Marker { pts: Vec<Pt>, color: Rgba, width: f32 },
    Line { a: Pt, b: Pt, color: Rgba, width: f32, outline: Option<Rgba> },
    Arrow { a: Pt, b: Pt, color: Rgba, width: f32, outline: Option<Rgba> },
    Rect { a: Pt, b: Pt, color: Rgba, width: f32, outline: Option<Rgba> },
    Ellipse { a: Pt, b: Pt, color: Rgba, width: f32, outline: Option<Rgba> },
    Text { pos: Pt, text: String, color: Rgba, size: f32, outline: Option<Rgba> },
    /// A numbered circle (1, 2, 3...) centered on `pos`; `size` is its diameter.
    Step { pos: Pt, n: u32, color: Rgba, size: f32, outline: Option<Rgba> },
    /// Pixelation. Plain pixelated text can be read back with known-font
    /// attacks (e.g. Depix), so before averaging, words found by OCR are
    /// replaced with random fake words of the same size and color: the result
    /// looks like pixelated text, but of text that was never there. `patch`
    /// holds that finished result, made once from the screenshot when the
    /// shape is drawn (see `make_patch`) and frozen with it; until then
    /// (while dragging) a plain preview is shown.
    Pixelate { a: Pt, b: Pt, block: f32, seed: u64, patch: Option<Patch> },
}

/// Finished pixels of a pixelate shape, positioned in shape coordinates.
#[derive(Clone)]
pub struct Patch {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub px: Arc<Vec<u8>>,
}

impl std::fmt::Debug for Patch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Patch({}x{} at {}, {})", self.w, self.h, self.x, self.y)
    }
}

impl Shape {
    pub fn translate(&mut self, dx: f32, dy: f32) {
        let mv = |p: &mut Pt| {
            p.0 += dx;
            p.1 += dy;
        };
        match self {
            Shape::Pen { pts, .. } | Shape::Marker { pts, .. } => pts.iter_mut().for_each(mv),
            Shape::Line { a, b, .. }
            | Shape::Arrow { a, b, .. }
            | Shape::Rect { a, b, .. }
            | Shape::Ellipse { a, b, .. } => {
                mv(a);
                mv(b);
            }
            Shape::Pixelate { a, b, patch, .. } => {
                mv(a);
                mv(b);
                if let Some(p) = patch {
                    p.x = (p.x as f32 + dx).round() as i32;
                    p.y = (p.y as f32 + dy).round() as i32;
                }
            }
            Shape::Text { pos, .. } | Shape::Step { pos, .. } => mv(pos),
        }
    }

    /// Corners of the area the shape's points span (ignoring stroke width),
    /// for sampling the background behind it. `None` for text, whose extent
    /// depends on layout.
    pub fn extent(&self) -> Option<(Pt, Pt)> {
        let span = |pts: &mut dyn Iterator<Item = Pt>| {
            pts.fold(None, |acc: Option<(Pt, Pt)>, p| match acc {
                None => Some((p, p)),
                Some((lo, hi)) => Some(((lo.0.min(p.0), lo.1.min(p.1)), (hi.0.max(p.0), hi.1.max(p.1)))),
            })
        };
        match self {
            Shape::Pen { pts, .. } | Shape::Marker { pts, .. } => span(&mut pts.iter().copied()),
            Shape::Line { a, b, .. }
            | Shape::Arrow { a, b, .. }
            | Shape::Rect { a, b, .. }
            | Shape::Ellipse { a, b, .. }
            | Shape::Pixelate { a, b, .. } => span(&mut [*a, *b].into_iter()),
            Shape::Step { pos, size, .. } => {
                let r = size / 2.0;
                Some(((pos.0 - r, pos.1 - r), (pos.0 + r, pos.1 + r)))
            }
            Shape::Text { .. } => None,
        }
    }

    /// Sets the outline of shapes that can have one.
    pub fn set_outline(&mut self, o: Option<Rgba>) {
        match self {
            Shape::Pen { outline, .. }
            | Shape::Line { outline, .. }
            | Shape::Arrow { outline, .. }
            | Shape::Rect { outline, .. }
            | Shape::Ellipse { outline, .. }
            | Shape::Text { outline, .. }
            | Shape::Step { outline, .. } => *outline = o,
            Shape::Marker { .. } | Shape::Pixelate { .. } => {}
        }
    }

    pub fn color(&self) -> Option<Rgba> {
        match self {
            Shape::Pen { color, .. }
            | Shape::Marker { color, .. }
            | Shape::Line { color, .. }
            | Shape::Arrow { color, .. }
            | Shape::Rect { color, .. }
            | Shape::Ellipse { color, .. }
            | Shape::Text { color, .. }
            | Shape::Step { color, .. } => Some(*color),
            Shape::Pixelate { .. } => None,
        }
    }

    /// Too small to keep, e.g. a plain click with the rectangle tool.
    pub fn is_degenerate(&self) -> bool {
        match self {
            Shape::Pen { pts, .. } | Shape::Marker { pts, .. } => pts.is_empty(),
            Shape::Line { a, b, .. } | Shape::Arrow { a, b, .. } => dist(*a, *b) < 3.0,
            Shape::Rect { a, b, .. } | Shape::Ellipse { a, b, .. } | Shape::Pixelate { a, b, .. } => {
                (a.0 - b.0).abs() < 2.0 || (a.1 - b.1).abs() < 2.0
            }
            Shape::Text { text, .. } => text.trim().is_empty(),
            Shape::Step { .. } => false,
        }
    }
}

impl Shape {
    /// Whether point `p` is on the shape (within `tol` pixels of its stroke;
    /// anywhere inside for text, steps and pixelate areas).
    pub fn hit(&self, p: Pt, tol: f32, fonts: &Fonts) -> bool {
        let inside = |l: f32, t: f32, r: f32, b: f32| p.0 >= l && p.0 <= r && p.1 >= t && p.1 <= b;
        match self {
            Shape::Pen { pts, width, .. } | Shape::Marker { pts, width, .. } => {
                let d = width / 2.0 + tol;
                match pts.len() {
                    0 => false,
                    1 => dist(pts[0], p) <= d,
                    _ => pts.windows(2).any(|w| segment_dist(p, w[0], w[1]) <= d),
                }
            }
            Shape::Line { a, b, width, .. } | Shape::Arrow { a, b, width, .. } => {
                segment_dist(p, *a, *b) <= width / 2.0 + tol
            }
            Shape::Rect { a, b, width, .. } => {
                // Only the border: the inside stays free for other clicks.
                let d = width / 2.0 + tol;
                let (l, t, r, bt) = (a.0.min(b.0), a.1.min(b.1), a.0.max(b.0), a.1.max(b.1));
                inside(l - d, t - d, r + d, bt + d) && !(p.0 > l + d && p.0 < r - d && p.1 > t + d && p.1 < bt - d)
            }
            Shape::Ellipse { a, b, width, .. } => {
                let (cx, cy) = ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0);
                let (rx, ry) = ((a.0 - b.0).abs() / 2.0, (a.1 - b.1).abs() / 2.0);
                if rx < 1.0 || ry < 1.0 {
                    return segment_dist(p, *a, *b) <= width / 2.0 + tol;
                }
                let (nx, ny) = ((p.0 - cx) / rx, (p.1 - cy) / ry);
                ((nx * nx + ny * ny).sqrt() - 1.0).abs() * rx.min(ry) <= width / 2.0 + tol
            }
            Shape::Text { pos, text, size, .. } => {
                let (w, h) = text::measure(&fonts.bold, text, *size);
                inside(pos.0 - tol, pos.1 - tol, pos.0 + w + tol, pos.1 + h + tol)
            }
            Shape::Step { pos, size, .. } => dist(*pos, p) <= size / 2.0 + tol,
            Shape::Pixelate { a, b, .. } => inside(a.0.min(b.0), a.1.min(b.1), a.0.max(b.0), a.1.max(b.1)),
        }
    }
}

/// Distance from `p` to the segment a-b.
fn segment_dist(p: Pt, a: Pt, b: Pt) -> f32 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len2 = dx * dx + dy * dy;
    if len2 < 1e-6 {
        return dist(a, p);
    }
    let t = (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len2).clamp(0.0, 1.0);
    dist((a.0 + t * dx, a.1 + t * dy), p)
}

pub fn dist(a: Pt, b: Pt) -> f32 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
}

/// Constrains the segment a→b to multiples of 45°.
pub fn snap_angle(a: Pt, b: Pt) -> Pt {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len = (dx * dx + dy * dy).sqrt();
    let step = std::f32::consts::FRAC_PI_4;
    let ang = (dy.atan2(dx) / step).round() * step;
    (a.0 + len * ang.cos(), a.1 + len * ang.sin())
}

/// Constrains the box a→b to a square.
pub fn snap_square(a: Pt, b: Pt) -> Pt {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let m = dx.abs().max(dy.abs());
    (a.0 + m * dx.signum(), a.1 + m * dy.signum())
}

/// Draws `shapes` into a `w`x`h` BGRA buffer whose top-left pixel corresponds
/// to `(ox, oy)` in shape coordinates.
pub fn render(buf: &mut [u8], w: i32, h: i32, shapes: &[Shape], ox: f32, oy: f32, fonts: &Fonts) {
    for s in shapes {
        render_one(buf, w, h, s, ox, oy, fonts);
    }
}

pub const OUTLINE_WHITE: Rgba = Rgba(255, 255, 255, 240);
pub const OUTLINE_BLACK: Rgba = Rgba(16, 16, 18, 235);

/// Perceived brightness, 0–255.
pub fn luma(c: Rgba) -> u8 {
    ((c.0 as u32 * 299 + c.1 as u32 * 587 + c.2 as u32 * 114) / 1000) as u8
}

/// The automatic outline for a drawing color over a background of the given
/// brightness. It must contrast with the color; mid-tone colors (red, blue,
/// green…) contrast with both black and white, so for those it picks the one
/// that also stands out from the background — then the outline is always
/// visible, not just where it's needed.
pub fn auto_outline(c: Rgba, background: u8) -> Rgba {
    let black = match luma(c) {
        l if l > 175 => true,
        l if l < 80 => false,
        _ => background > 128,
    };
    if black {
        OUTLINE_BLACK
    } else {
        OUTLINE_WHITE
    }
}

/// How far the halo extends past a stroke of the given width.
fn halo(width: f32) -> f32 {
    (1.0 + width * 0.3).clamp(2.0, 6.0)
}

/// Halo radius for text of the given size.
fn text_halo(size: f32) -> f32 {
    (size * 0.09).clamp(2.0, 5.0)
}

pub fn render_one(buf: &mut [u8], w: i32, h: i32, s: &Shape, ox: f32, oy: f32, fonts: &Fonts) {
    match s {
        Shape::Text { pos, text, color, size, outline } => {
            let halo = outline.map(|oc| (oc, text_halo(*size)));
            text::draw_outlined(buf, w, h, pos.0 - ox, pos.1 - oy, text, *size, *color, halo, &fonts.bold);
        }
        Shape::Step { pos, n, color, size, outline } => {
            let (cx, cy, r) = (pos.0 - ox, pos.1 - oy, size / 2.0);
            if let Some(mut pm) = PixmapMut::from_bytes(buf, w as u32, h as u32) {
                if let Some(oc) = outline {
                    fill(&mut pm, PathBuilder::from_circle(cx, cy, r + halo(size * 0.1)), *oc, Transform::identity());
                }
                fill(&mut pm, PathBuilder::from_circle(cx, cy, r), *color, Transform::identity());
            }
            let label = n.to_string();
            let px = size * if label.len() > 1 { 0.5 } else { 0.6 };
            let fg = if luma(*color) > 160 { Rgba(20, 20, 22, 255) } else { Rgba(255, 255, 255, 255) };
            let tw = text::line_width(&fonts.bold, &label, px);
            let (ascent, _) = text::line_metrics(&fonts.bold, px);
            // Center on the digits' height (~0.7 em), not the whole line box.
            text::draw(buf, w, h, cx - tw / 2.0, cy + px * 0.36 - ascent, &label, px, fg, &fonts.bold);
        }
        Shape::Pixelate { a, b, block, patch, .. } => match patch {
            Some(p) => blit_patch(buf, w, h, p, ox, oy),
            None => pixelate(buf, w, h, pixel_box(*a, *b, ox, oy), block_px(*block)),
        },
        _ => {
            let Some(mut pm) = PixmapMut::from_bytes(buf, w as u32, h as u32) else {
                return;
            };
            let tf = Transform::from_translate(-ox, -oy);
            let round = (LineCap::Round, LineJoin::Round);
            let sharp = (LineCap::Square, LineJoin::Miter);
            match s {
                Shape::Pen { pts, color, width, outline } => {
                    outlined(&mut pm, smooth_path(pts), *color, *width, *outline, round, tf)
                }
                Shape::Marker { pts, color, width } => {
                    stroke(&mut pm, smooth_path(pts), color.alpha(110), *width, round, tf)
                }
                Shape::Line { a, b, color, width, outline } => {
                    outlined(&mut pm, line_path(*a, *b), *color, *width, *outline, round, tf)
                }
                Shape::Arrow { a, b, color, width, outline } => arrow(&mut pm, *a, *b, *color, *width, *outline, tf),
                Shape::Rect { a, b, color, width, outline } => {
                    outlined(&mut pm, rect_path(*a, *b), *color, *width, *outline, sharp, tf)
                }
                Shape::Ellipse { a, b, color, width, outline } => {
                    outlined(&mut pm, oval_path(*a, *b), *color, *width, *outline, round, tf)
                }
                Shape::Text { .. } | Shape::Step { .. } | Shape::Pixelate { .. } => {}
            }
        }
    }
}

fn stroke(pm: &mut PixmapMut, path: Option<Path>, c: Rgba, width: f32, style: (LineCap, LineJoin), tf: Transform) {
    let Some(path) = path else { return };
    let mut paint = Paint::default();
    paint.set_color(c.sk());
    paint.anti_alias = true;
    let st = Stroke {
        width,
        line_cap: style.0,
        line_join: style.1,
        ..Stroke::default()
    };
    pm.stroke_path(&path, &paint, &st, tf, None);
}

/// A stroke, optionally with a halo: the same path stroked wider in the
/// outline color underneath.
fn outlined(
    pm: &mut PixmapMut,
    path: Option<Path>,
    c: Rgba,
    width: f32,
    outline: Option<Rgba>,
    style: (LineCap, LineJoin),
    tf: Transform,
) {
    if let Some(oc) = outline {
        stroke(pm, path.clone(), oc, width + 2.0 * halo(width), style, tf);
    }
    stroke(pm, path, c, width, style, tf);
}

fn fill(pm: &mut PixmapMut, path: Option<Path>, c: Rgba, tf: Transform) {
    let Some(path) = path else { return };
    let mut paint = Paint::default();
    paint.set_color(c.sk());
    paint.anti_alias = true;
    pm.fill_path(&path, &paint, FillRule::Winding, tf, None);
}

/// Freehand stroke smoothed with quadratic curves through segment midpoints.
fn smooth_path(pts: &[Pt]) -> Option<Path> {
    let first = *pts.first()?;
    let mut pb = PathBuilder::new();
    pb.move_to(first.0, first.1);
    match pts.len() {
        1 => pb.line_to(first.0 + 0.01, first.1),
        2 => pb.line_to(pts[1].0, pts[1].1),
        n => {
            for i in 1..n - 1 {
                let (p, q) = (pts[i], pts[i + 1]);
                pb.quad_to(p.0, p.1, (p.0 + q.0) / 2.0, (p.1 + q.1) / 2.0);
            }
            pb.line_to(pts[n - 1].0, pts[n - 1].1);
        }
    }
    pb.finish()
}

fn line_path(a: Pt, b: Pt) -> Option<Path> {
    let mut pb = PathBuilder::new();
    pb.move_to(a.0, a.1);
    pb.line_to(b.0, b.1);
    pb.finish()
}

fn rect_path(a: Pt, b: Pt) -> Option<Path> {
    let r = SkRect::from_ltrb(a.0.min(b.0), a.1.min(b.1), a.0.max(b.0), a.1.max(b.1))?;
    Some(PathBuilder::from_rect(r))
}

fn oval_path(a: Pt, b: Pt) -> Option<Path> {
    let r = SkRect::from_ltrb(a.0.min(b.0), a.1.min(b.1), a.0.max(b.0), a.1.max(b.1))?;
    PathBuilder::from_oval(r)
}

fn arrow(pm: &mut PixmapMut, a: Pt, b: Pt, c: Rgba, width: f32, outline: Option<Rgba>, tf: Transform) {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len = (dx * dx + dy * dy).sqrt();
    if len < 1.0 {
        return;
    }
    let (ux, uy) = (dx / len, dy / len);
    let head_len = (width * 3.0 + 9.0).min(len * 0.8);
    let half = head_len * 0.55;
    let base = (b.0 - ux * head_len, b.1 - uy * head_len);
    // The shaft ends inside the head so its cap is hidden.
    let shaft_end = (b.0 - ux * head_len * 0.6, b.1 - uy * head_len * 0.6);
    let shaft = line_path(a, shaft_end);

    let notch = (base.0 + ux * head_len * 0.18, base.1 + uy * head_len * 0.18);
    let mut pb = PathBuilder::new();
    pb.move_to(b.0, b.1);
    pb.line_to(base.0 - uy * half, base.1 + ux * half);
    pb.line_to(notch.0, notch.1);
    pb.line_to(base.0 + uy * half, base.1 - ux * half);
    pb.close();
    let head = pb.finish();

    let round = (LineCap::Round, LineJoin::Round);
    if let Some(oc) = outline {
        // Both halos go down first so neither covers the other's color.
        let hw = halo(width);
        stroke(pm, shaft.clone(), oc, width + 2.0 * hw, round, tf);
        stroke(pm, head.clone(), oc, 2.0 * hw, round, tf);
    }
    stroke(pm, shaft, c, width, round, tf);
    fill(pm, head, c, tf);
}

/// A fresh random seed for a pixelate shape.
pub fn new_seed() -> u64 {
    use std::hash::{BuildHasher, RandomState};
    RandomState::new().hash_one(std::time::SystemTime::now())
}

type PixelBox = (i32, i32, i32, i32);

const GRAY: Rgba = Rgba(128, 128, 128, 255);

fn block_px(block: f32) -> i32 {
    block.round().max(2.0) as i32
}

fn pixel_box(a: Pt, b: Pt, ox: f32, oy: f32) -> PixelBox {
    (
        (a.0.min(b.0) - ox).floor() as i32,
        (a.1.min(b.1) - oy).floor() as i32,
        (a.0.max(b.0) - ox).ceil() as i32,
        (a.1.max(b.1) - oy).ceil() as i32,
    )
}

fn clip((l, t, r, b): PixelBox, w: i32, h: i32) -> Option<PixelBox> {
    let c = (l.max(0), t.max(0), r.min(w), b.min(h));
    (c.0 < c.2 && c.1 < c.3).then_some(c)
}

/// The visible blocks of a box. The grid is aligned to the box origin, so it
/// doesn't shift when part of the box is clipped.
fn blocks(area: PixelBox, w: i32, h: i32, block: i32) -> Vec<Rect> {
    let (l, t, _, _) = area;
    let Some((cl, ct, cr, cb)) = clip(area, w, h) else { return Vec::new() };
    let mut v = Vec::new();
    let mut cy = t + (ct - t) / block * block;
    while cy < cb {
        let mut cx = l + (cl - l) / block * block;
        while cx < cr {
            v.push(Rect::new(cx.max(cl), cy.max(ct), (cx + block).min(cr), (cy + block).min(cb)));
            cx += block;
        }
        cy += block;
    }
    v
}

fn fill_box(buf: &mut [u8], w: i32, r: Rect, px: [u8; 4]) {
    for y in r.t..r.b {
        let i = ((y * w + r.l) * 4) as usize;
        for p in buf[i..i + r.w() as usize * 4].chunks_exact_mut(4) {
            p.copy_from_slice(&px);
        }
    }
}

/// Plain pixelation: every block becomes the average of its pixels.
fn pixelate(buf: &mut [u8], w: i32, h: i32, area: PixelBox, block: i32) {
    for c in blocks(area, w, h, block) {
        let (mut s, mut n) = ([0u32; 3], 0u32);
        for y in c.t..c.b {
            let i = ((y * w + c.l) * 4) as usize;
            for p in buf[i..i + c.w() as usize * 4].chunks_exact(4) {
                s[0] += p[0] as u32;
                s[1] += p[1] as u32;
                s[2] += p[2] as u32;
                n += 1;
            }
        }
        fill_box(buf, w, c, [(s[0] / n) as u8, (s[1] / n) as u8, (s[2] / n) as u8, 255]);
    }
}

fn blit_patch(buf: &mut [u8], w: i32, h: i32, p: &Patch, ox: f32, oy: f32) {
    let (dx, dy) = ((p.x as f32 - ox).round() as i32, (p.y as f32 - oy).round() as i32);
    let Some((l, t, r, b)) = clip((dx, dy, dx + p.w, dy + p.h), w, h) else { return };
    let n = (r - l) as usize * 4;
    for y in t..b {
        let src = (((y - dy) * p.w + (l - dx)) * 4) as usize;
        let dst = ((y * w + l) * 4) as usize;
        buf[dst..dst + n].copy_from_slice(&p.px[src..src + n]);
    }
}

/// Makes the finished pixelation of box `a`→`b` over a BGRA image whose
/// (0, 0) is at shape coordinate (0, 0). Words found by OCR are replaced
/// with random ones, then the area is pixelated normally, so images look
/// exactly like pixelated images and text like pixelated text — just not the
/// real text. Without OCR the area becomes a mosaic of its colors instead, so
/// it never falls back to something reversible.
#[allow(clippy::too_many_arguments)]
pub fn make_patch(buf: &[u8], w: i32, h: i32, a: Pt, b: Pt, block: f32, seed: u64, fonts: &Fonts) -> Option<Patch> {
    let area = pixel_box(a, b, 0.0, 0.0);
    let (l, t, r, bt) = clip(area, w, h)?;
    let (pw, ph) = (r - l, bt - t);
    let mut px = Vec::with_capacity(pw as usize * ph as usize * 4);
    for y in t..bt {
        let i = ((y * w + l) * 4) as usize;
        px.extend_from_slice(&buf[i..i + pw as usize * 4]);
    }
    let local = (area.0 - l, area.1 - t, area.2 - l, area.3 - t);
    let block = block_px(block);
    let mut rng = Rng(seed);
    match ocr::find_text(&px, pw, ph) {
        Some(lines) => {
            scramble(&mut px, pw, ph, &lines, &mut rng, fonts);
            pixelate(&mut px, pw, ph, local, block);
        }
        None => color_mosaic(&mut px, pw, ph, local, block, &mut rng),
    }
    Some(Patch { x: l, y: t, w: pw, h: ph, px: Arc::new(px) })
}

/// The text-scrambling step alone, without pixelation (self-test only).
#[cfg(debug_assertions)]
pub fn scramble_preview(buf: &[u8], w: i32, h: i32, seed: u64, fonts: &Fonts) -> (Vec<u8>, usize) {
    let mut px = buf.to_vec();
    let lines = ocr::find_text(&px, w, h).unwrap_or_default();
    scramble(&mut px, w, h, &lines, &mut Rng(seed), fonts);
    (px, lines.len())
}

const FAKE_CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzABCDEFGHJKLMNPRSTUVWXYZ0123456789";

/// How to redraw one line of text.
struct LinePlan {
    words: Vec<Rect>,
    erase: Vec<Rect>,
    bg: Rgba,
    ink: Rgba,
    size: f32,
    baseline: f32,
}

/// Replaces every word with a random one of about the same size, width and
/// color. All lines are measured first, then erased, then redrawn, so one
/// line's cleanup can't clip the fake text of its neighbor.
fn scramble(buf: &mut [u8], w: i32, h: i32, lines: &[ocr::Line], rng: &mut Rng, fonts: &Fonts) {
    let plans: Vec<LinePlan> = lines.iter().filter_map(|l| plan_line(buf, w, h, l)).collect();
    for p in &plans {
        for r in &p.erase {
            fill_box(buf, w, *r, [p.bg.2, p.bg.1, p.bg.0, 255]);
        }
    }
    for p in &plans {
        let (ascent, _) = text::line_metrics(&fonts.ui, p.size);
        let mut words = p.words.clone();
        words.sort_by_key(|r| r.l);
        for (i, word) in words.iter().enumerate() {
            // Roughly the real width (randomized so it doesn't give away the
            // exact length), but never running into the next word.
            let target = word.w() as f32 * (0.8 + 0.25 * rng.unit());
            let room = words.get(i + 1).map_or(f32::MAX, |n| (n.l - word.l) as f32 - p.size * 0.25);
            let mut fake = String::new();
            while fake.len() < 80 {
                fake.push(FAKE_CHARS[(rng.next() % FAKE_CHARS.len() as u64) as usize] as char);
                let width = text::line_width(&fonts.ui, &fake, p.size);
                if width > room && fake.len() > 1 {
                    fake.pop();
                    break;
                }
                if width >= target {
                    break;
                }
            }
            text::draw(buf, w, h, word.l as f32, p.baseline - ascent, &fake, p.size, p.ink, &fonts.ui);
        }
    }
}

/// Measures a line: its background and text colors, and a font size and
/// baseline that match the real text.
fn plan_line(buf: &[u8], w: i32, h: i32, line: &ocr::Line) -> Option<LinePlan> {
    let bounds = Rect::new(0, 0, w, h);
    let lb = line.bounds().intersect(&bounds);
    if lb.is_empty() {
        return None;
    }
    let lh = lb.h();
    // Generous margin so no stray bits of the real glyphs survive.
    let pad = lh / 4 + 1;
    let px = |x: i32, y: i32| {
        let i = ((y * w + x) * 4) as usize;
        u32::from_le_bytes([buf[i], buf[i + 1], buf[i + 2], 0])
    };

    // Background: the most common color around the line.
    let around = lb.inflate(pad).intersect(&bounds);
    let mut ring: Vec<u32> = (around.l..around.r).flat_map(|x| [px(x, around.t), px(x, around.b - 1)]).collect();
    ring.extend((around.t..around.b).flat_map(|y| [px(around.l, y), px(around.r - 1, y)]));
    let bg = dominant(&mut ring).unwrap_or(GRAY);

    // Ink color, the ink's vertical extent, and where each column's ink ends.
    let mut inked = Vec::new();
    let mut bottoms = vec![0usize; lh as usize + 1];
    let (mut top, mut bottom) = (lh, 0);
    for x in lb.l..lb.r {
        let mut lowest = None;
        for y in lb.t..lb.b {
            let c = px(x, y);
            if far(unpack(c), bg) {
                inked.push(c);
                lowest = Some(y - lb.t);
                top = top.min(y - lb.t);
            }
        }
        if let Some(y) = lowest {
            bottoms[(y + 1) as usize] += 1;
            bottom = bottom.max(y + 1);
        }
    }
    if bottom <= top {
        return None; // nothing that looks like text
    }
    // The letters' solid color, not their anti-aliased edges: only consider
    // pixels with close to the strongest contrast against the background.
    let contrast_of = |c: u32| {
        let c = unpack(c);
        let d = |x: u8, y: u8| (x as i32 - y as i32).abs();
        d(c.0, bg.0).max(d(c.1, bg.1)).max(d(c.2, bg.2))
    };
    let strongest = inked.iter().map(|&c| contrast_of(c)).max().unwrap_or(0);
    inked.retain(|&c| contrast_of(c) * 4 >= strongest * 3);
    let ink = dominant(&mut inked).unwrap_or_else(|| contrast(bg));

    // Most letters sit on the baseline, so it's where columns most often end
    // (descenders are the few that reach lower). OCR boxes can be loose, so
    // the size comes from the ink: baseline to the tallest letters is about
    // 0.72 em, and the whole ink height at most about 0.97 em.
    let baseline = (1..bottoms.len()).max_by_key(|&i| (bottoms[i], i)).unwrap_or(bottom as usize) as i32;
    let size = ((baseline - top) as f32 / 0.72).max((bottom - top) as f32 / 0.97).max(6.0);

    Some(LinePlan {
        words: line.words.clone(),
        erase: line.words.iter().map(|r| r.inflate(pad).intersect(&bounds)).filter(|r| !r.is_empty()).collect(),
        bg,
        ink,
        size,
        baseline: (lb.t + baseline) as f32,
    })
}

/// Fallback when OCR isn't available: blocks randomly mix the area's two
/// main colors, so nothing about its content can be read back.
fn color_mosaic(buf: &mut [u8], w: i32, h: i32, area: PixelBox, block: i32, rng: &mut Rng) {
    let mut all: Vec<u32> = buf.chunks_exact(4).map(|p| u32::from_le_bytes([p[0], p[1], p[2], 0])).collect();
    let bg = dominant(&mut all).unwrap_or(GRAY);
    let mut inked: Vec<u32> = all.into_iter().filter(|&c| far(unpack(c), bg)).collect();
    let ink = dominant(&mut inked).unwrap_or_else(|| contrast(bg));
    for c in blocks(area, w, h, block) {
        let u = rng.unit();
        let f = u * u * 0.6;
        let mix = |b: u8, i: u8| (b as f32 + (i as f32 - b as f32) * f) as u8;
        fill_box(buf, w, c, [mix(bg.2, ink.2), mix(bg.1, ink.1), mix(bg.0, ink.0), 255]);
    }
}

/// Clearly different colors (any channel differs by 48 or more).
fn far(a: Rgba, b: Rgba) -> bool {
    let d = |x: u8, y: u8| (x as i32 - y as i32).abs();
    d(a.0, b.0).max(d(a.1, b.1)).max(d(a.2, b.2)) >= 48
}

fn unpack(c: u32) -> Rgba {
    Rgba((c >> 16) as u8, (c >> 8) as u8, c as u8, 255)
}

/// Start and length of the longest run of equal keys in a sorted slice
/// (ties go to the larger key, to stay deterministic).
fn longest_run(v: &[u32], key: impl Fn(u32) -> u32) -> Option<(usize, usize)> {
    let mut best: Option<(usize, u32, usize)> = None; // (len, key, start)
    let mut i = 0;
    while i < v.len() {
        let k = key(v[i]);
        let n = v[i..].iter().take_while(|&&c| key(c) == k).count();
        if best.is_none_or(|(bn, bk, _)| (n, k) > (bn, bk)) {
            best = Some((n, k, i));
        }
        i += n;
    }
    best.map(|(n, _, start)| (start, n))
}

/// The most common color. Colors are grouped coarsely (16 levels per channel)
/// to find the dominant group, then that group's exact most frequent color is
/// used, so anti-aliased edges don't shift the result.
fn dominant(colors: &mut [u32]) -> Option<Rgba> {
    let group = |c: u32| c & 0x00F0_F0F0;
    colors.sort_unstable_by_key(|&c| (group(c), c));
    let (start, n) = longest_run(colors, group)?;
    let in_group = &colors[start..start + n];
    let (s, _) = longest_run(in_group, |c| c)?;
    Some(unpack(in_group[s]))
}

/// A visibly different shade, for when there's no second color.
fn contrast(c: Rgba) -> Rgba {
    let luma = (c.0 as u32 * 3 + c.1 as u32 * 6 + c.2 as u32) / 10;
    let target = if luma > 127 { 0.0 } else { 255.0 };
    let mix = |v: u8| (v as f32 + (target - v as f32) * 0.35) as u8;
    Rgba(mix(c.0), mix(c.1), mix(c.2), 255)
}

fn splitmix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// A small deterministic random stream (the fake words only need to be
/// unrelated to the real ones, not unpredictable).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(1);
        splitmix(self.0)
    }

    /// Uniform in [0, 1).
    fn unit(&mut self) -> f32 {
        (self.next() >> 40) as f32 / (1u64 << 24) as f32
    }
}
