//! The application icon, drawn in code.
//!
//! Depends only on tiny-skia so that `build.rs` can include this file to
//! generate the `.ico` embedded in the executable, while the app itself uses
//! it at runtime for the tray icon (including the red "recording" variant).

use tiny_skia::{Color, FillRule, LineCap, LineJoin, Paint, Path, PathBuilder, Pixmap, Stroke, Transform};

/// Renders the icon as straight (non-premultiplied) RGBA, `size` x `size`.
pub fn render_rgba(size: u32, recording: bool) -> Vec<u8> {
    let mut pm = Pixmap::new(size, size).expect("icon size");
    let k = size as f32 / 32.0;
    let tf = Transform::from_scale(k, k);

    let mut paint = Paint::default();
    paint.anti_alias = true;
    paint.set_color(if recording {
        Color::from_rgba8(229, 57, 53, 255)
    } else {
        Color::from_rgba8(37, 118, 240, 255)
    });
    if let Some(bg) = rounded_rect(1.0, 1.0, 30.0, 30.0, 7.5) {
        pm.fill_path(&bg, &paint, FillRule::Winding, tf, None);
    }

    // Viewfinder corner brackets.
    paint.set_color(Color::WHITE);
    let stroke = Stroke {
        width: 2.6,
        line_cap: LineCap::Round,
        line_join: LineJoin::Round,
        ..Stroke::default()
    };
    let brackets: [[(f32, f32); 3]; 4] = [
        [(8.0, 13.0), (8.0, 8.0), (13.0, 8.0)],
        [(19.0, 8.0), (24.0, 8.0), (24.0, 13.0)],
        [(8.0, 19.0), (8.0, 24.0), (13.0, 24.0)],
        [(24.0, 19.0), (24.0, 24.0), (19.0, 24.0)],
    ];
    for b in brackets {
        let mut pb = PathBuilder::new();
        pb.move_to(b[0].0, b[0].1);
        pb.line_to(b[1].0, b[1].1);
        pb.line_to(b[2].0, b[2].1);
        if let Some(p) = pb.finish() {
            pm.stroke_path(&p, &paint, &stroke, tf, None);
        }
    }
    if let Some(dot) = PathBuilder::from_circle(16.0, 16.0, 3.2) {
        pm.fill_path(&dot, &paint, FillRule::Winding, tf, None);
    }

    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for p in pm.pixels() {
        let c = p.demultiply();
        out.extend_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
    }
    out
}

fn rounded_rect(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<Path> {
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
