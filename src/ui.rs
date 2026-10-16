//! Drawing helpers for UI chrome (toolbars, labels) and the toolbar icons.

use fontdue::Font;
use tiny_skia::{FillRule, LineCap, LineJoin, Paint, Path, PathBuilder, PixmapMut, Rect as SkRect, Stroke, StrokeDash, Transform};

use crate::annotate::Rgba;
use crate::text::{self, Fonts};
use crate::util::Rect;

/// A BGRA buffer to draw into.
pub struct Canvas<'a> {
    pub buf: &'a mut [u8],
    pub w: i32,
    pub h: i32,
}

impl<'a> Canvas<'a> {
    pub fn new(buf: &'a mut [u8], w: i32, h: i32) -> Self {
        Canvas { buf, w, h }
    }

    fn pm(&mut self) -> Option<PixmapMut<'_>> {
        PixmapMut::from_bytes(self.buf, self.w as u32, self.h as u32)
    }

    pub fn fill_path(&mut self, path: Option<Path>, c: Rgba, tf: Transform) {
        let (Some(path), Some(mut pm)) = (path, self.pm()) else { return };
        let mut paint = Paint::default();
        paint.set_color(c.sk());
        paint.anti_alias = true;
        pm.fill_path(&path, &paint, FillRule::Winding, tf, None);
    }

    pub fn stroke_path(&mut self, path: Option<Path>, c: Rgba, width: f32, tf: Transform) {
        self.stroke_styled(path, c, width, None, tf);
    }

    fn stroke_styled(&mut self, path: Option<Path>, c: Rgba, width: f32, dash: Option<StrokeDash>, tf: Transform) {
        let (Some(path), Some(mut pm)) = (path, self.pm()) else { return };
        let mut paint = Paint::default();
        paint.set_color(c.sk());
        paint.anti_alias = true;
        let st = Stroke {
            width,
            line_cap: LineCap::Round,
            line_join: LineJoin::Round,
            dash,
            ..Stroke::default()
        };
        pm.stroke_path(&path, &paint, &st, tf, None);
    }

    pub fn fill_rrect(&mut self, r: Rect, rad: f32, c: Rgba) {
        let p = rrect_path(r.l as f32, r.t as f32, r.w() as f32, r.h() as f32, rad);
        self.fill_path(p, c, Transform::identity());
    }

    pub fn fill_circle(&mut self, cx: f32, cy: f32, r: f32, c: Rgba) {
        self.fill_path(PathBuilder::from_circle(cx, cy, r), c, Transform::identity());
    }

    pub fn stroke_circle(&mut self, cx: f32, cy: f32, r: f32, width: f32, c: Rgba) {
        self.stroke_path(PathBuilder::from_circle(cx, cy, r), c, width, Transform::identity());
    }

    /// Fills the right half of a circle.
    pub fn fill_half_circle(&mut self, cx: f32, cy: f32, r: f32, c: Rgba) {
        let k = 0.552_284_8 * r;
        let mut pb = PathBuilder::new();
        pb.move_to(cx, cy - r);
        pb.cubic_to(cx + k, cy - r, cx + r, cy - k, cx + r, cy);
        pb.cubic_to(cx + r, cy + k, cx + k, cy + r, cx, cy + r);
        pb.close();
        self.fill_path(pb.finish(), c, Transform::identity());
    }

    pub fn line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, width: f32, c: Rgba) {
        self.stroke_path(poly(&[(x0, y0), (x1, y1)], false), c, width, Transform::identity());
    }

    pub fn dashed_rect(&mut self, r: Rect, c: Rgba) {
        let Some(sr) = SkRect::from_ltrb(r.l as f32 + 0.5, r.t as f32 + 0.5, r.r as f32 - 0.5, r.b as f32 - 0.5) else {
            return;
        };
        let dash = StrokeDash::new(vec![4.0, 3.0], 0.0);
        self.stroke_styled(Some(PathBuilder::from_rect(sr)), c, 1.0, dash, Transform::identity());
    }

    /// Pixel-exact fill, alpha-blended, clipped to the buffer.
    pub fn fill_px(&mut self, r: Rect, c: Rgba) {
        let r = r.intersect(&Rect::new(0, 0, self.w, self.h));
        if r.is_empty() {
            return;
        }
        let a = c.3 as u32;
        let ia = 255 - a;
        let src = [c.2 as u32 * a, c.1 as u32 * a, c.0 as u32 * a];
        for y in r.t..r.b {
            let row = ((y * self.w + r.l) * 4) as usize;
            let line = &mut self.buf[row..row + r.w() as usize * 4];
            if a == 255 {
                let px = c.bgra().to_le_bytes();
                for p in line.chunks_exact_mut(4) {
                    p.copy_from_slice(&px);
                }
            } else {
                for p in line.chunks_exact_mut(4) {
                    p[0] = ((src[0] + p[0] as u32 * ia) / 255) as u8;
                    p[1] = ((src[1] + p[1] as u32 * ia) / 255) as u8;
                    p[2] = ((src[2] + p[2] as u32 * ia) / 255) as u8;
                    p[3] = 255;
                }
            }
        }
    }

    pub fn text(&mut self, x: f32, y: f32, s: &str, px: f32, c: Rgba, font: &Font) {
        text::draw(self.buf, self.w, self.h, x, y, s, px, c, font);
    }

    /// Draws a toolbar icon centered at `(cx, cy)`, `size` pixels square.
    /// Icons are designed on a 24x24 grid.
    pub fn icon(&mut self, icon: Icon, cx: f32, cy: f32, size: f32, c: Rgba, fonts: &Fonts) {
        let k = size / 24.0;
        let tf = Transform::from_row(k, 0.0, 0.0, k, cx - 12.0 * k, cy - 12.0 * k);
        let sw = 1.9;
        match icon {
            Icon::Pen => {
                let body = poly(&[(4.5, 19.5), (5.5, 15.0), (15.5, 5.0), (19.0, 8.5), (9.0, 18.5)], true);
                self.stroke_path(body, c, sw, tf);
                self.stroke_path(poly(&[(13.0, 7.5), (16.5, 11.0)], false), c, sw, tf);
            }
            Icon::Line => self.stroke_path(poly(&[(5.0, 19.0), (19.0, 5.0)], false), c, 2.2, tf),
            Icon::Arrow => {
                self.stroke_path(poly(&[(5.0, 19.0), (18.0, 6.0)], false), c, 2.2, tf);
                self.stroke_path(poly(&[(10.0, 5.5), (18.5, 5.5), (18.5, 14.0)], false), c, 2.2, tf);
            }
            Icon::Rect => self.stroke_path(rrect_path(4.0, 6.0, 16.0, 12.0, 1.5), c, sw, tf),
            Icon::Ellipse => {
                let oval = SkRect::from_xywh(3.5, 5.5, 17.0, 13.0).and_then(PathBuilder::from_oval);
                self.stroke_path(oval, c, sw, tf);
            }
            Icon::Marker => self.stroke_path(poly(&[(6.0, 17.5), (18.0, 6.5)], false), c.alpha(150), 6.5, tf),
            Icon::Text => {
                self.stroke_path(poly(&[(6.0, 6.0), (18.0, 6.0)], false), c, 2.4, tf);
                self.stroke_path(poly(&[(12.0, 6.0), (12.0, 19.0)], false), c, 2.4, tf);
            }
            Icon::Step => {
                self.stroke_path(PathBuilder::from_circle(12.0, 12.0, 8.8), c, sw, tf);
                let px = size * 0.46;
                let tw = text::line_width(&fonts.bold, "1", px);
                let (ascent, _) = text::line_metrics(&fonts.bold, px);
                self.text(cx - tw / 2.0, cy + px * 0.36 - ascent, "1", px, c, &fonts.bold);
            }
            Icon::Pixelate => {
                for i in 0..4 {
                    for j in 0..4 {
                        let col = if (i + j) % 2 == 0 { c } else { c.alpha(c.3 / 3) };
                        let cell = SkRect::from_xywh(4.0 + 4.0 * i as f32, 4.0 + 4.0 * j as f32, 4.0, 4.0)
                            .map(PathBuilder::from_rect);
                        self.fill_path(cell, col, tf);
                    }
                }
            }
            Icon::Undo | Icon::Redo => {
                let m = |x: f32| if icon == Icon::Redo { 24.0 - x } else { x };
                self.stroke_path(poly(&[(m(9.0), 5.5), (m(5.0), 9.5), (m(9.0), 13.5)], false), c, sw, tf);
                let mut pb = PathBuilder::new();
                pb.move_to(m(5.5), 9.5);
                pb.line_to(m(14.0), 9.5);
                pb.cubic_to(m(17.3), 9.5, m(19.5), 11.7, m(19.5), 14.3);
                pb.cubic_to(m(19.5), 17.0, m(17.3), 19.0, m(14.0), 19.0);
                pb.line_to(m(10.0), 19.0);
                self.stroke_path(pb.finish(), c, sw, tf);
            }
            Icon::Gif => {
                let px = size * 0.5;
                let tw = text::line_width(&fonts.bold, "GIF", px);
                let (ascent, _) = text::line_metrics(&fonts.bold, px);
                // Center on cap height (~0.7em) rather than the full line box.
                let top = cy + px * 0.35 - ascent;
                self.text(cx - tw / 2.0, top, "GIF", px, c, &fonts.bold);
            }
            Icon::Copy => {
                self.stroke_path(rrect_path(8.5, 8.5, 11.0, 11.0, 2.0), c, sw, tf);
                let mut pb = PathBuilder::new();
                pb.move_to(5.0, 15.5);
                pb.line_to(5.0, 7.0);
                pb.quad_to(5.0, 5.0, 7.0, 5.0);
                pb.line_to(15.5, 5.0);
                self.stroke_path(pb.finish(), c, sw, tf);
            }
            Icon::Save => {
                self.stroke_path(poly(&[(12.0, 4.0), (12.0, 15.0)], false), c, sw, tf);
                self.stroke_path(poly(&[(7.5, 10.5), (12.0, 15.0), (16.5, 10.5)], false), c, sw, tf);
                self.stroke_path(poly(&[(5.0, 19.5), (19.0, 19.5)], false), c, sw, tf);
            }
            Icon::Close => {
                self.stroke_path(poly(&[(6.5, 6.5), (17.5, 17.5)], false), c, 2.2, tf);
                self.stroke_path(poly(&[(17.5, 6.5), (6.5, 17.5)], false), c, 2.2, tf);
            }
            Icon::Plus => {
                self.stroke_path(poly(&[(12.0, 6.0), (12.0, 18.0)], false), c, 2.2, tf);
                self.stroke_path(poly(&[(6.0, 12.0), (18.0, 12.0)], false), c, 2.2, tf);
            }
            Icon::Stop => self.fill_path(rrect_path(6.5, 6.5, 11.0, 11.0, 2.0), c, tf),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Icon {
    Pen,
    Line,
    Arrow,
    Rect,
    Ellipse,
    Marker,
    Text,
    Step,
    Pixelate,
    Undo,
    Redo,
    Gif,
    Copy,
    Save,
    Close,
    Plus,
    Stop,
}

fn poly(pts: &[(f32, f32)], close: bool) -> Option<Path> {
    let (first, rest) = pts.split_first()?;
    let mut pb = PathBuilder::new();
    pb.move_to(first.0, first.1);
    for p in rest {
        pb.line_to(p.0, p.1);
    }
    if close {
        pb.close();
    }
    pb.finish()
}

pub fn rrect_path(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<Path> {
    let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
    if r <= 0.0 {
        return SkRect::from_xywh(x, y, w, h).map(PathBuilder::from_rect);
    }
    let k = 0.552_284_8 * r;
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.cubic_to(x + w - r + k, y, x + w, y + r - k, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.cubic_to(x + w, y + h - r + k, x + w - r + k, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.cubic_to(x + r - k, y + h, x, y + h - r + k, x, y + h - r);
    pb.line_to(x, y + r);
    pb.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
    pb.close();
    pb.finish()
}
