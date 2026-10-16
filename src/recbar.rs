//! The floating bar shown while recording a GIF: a blinking dot, elapsed
//! time, frame count and Stop/discard buttons, then encoding progress. Drawn
//! the same on every platform; each platform's window supplies state, clicks
//! and presentation.

use crate::annotate::Rgba;
use crate::text::{self, Fonts};
use crate::ui::{Canvas, Icon};
use crate::util::Rect;

pub const RED: Rgba = Rgba(229, 57, 53, 255);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BarBtn {
    Stop,
    Cancel,
}

pub struct BarState {
    /// Recording has stopped and the remaining frames are being encoded.
    pub encoding: bool,
    pub elapsed_ms: u64,
    pub frames: usize,
    pub processed: usize,
}

fn px(v: f32, scale: f32) -> i32 {
    (v * scale).round() as i32
}

/// The bar's size at a UI scale: wide enough for the longest labels in the
/// UI fonts (which differ between systems).
pub fn size(scale: f32, fonts: &Fonts) -> (i32, i32) {
    let s = scale;
    let rec = text::line_width(&fonts.bold, "REC 00:00", 13.0 * s);
    let frames = text::line_width(&fonts.ui, "00000 frames", 12.0 * s);
    let labels = (14.0 + 18.0 + 10.0 + 10.0) * s + rec + frames;
    let buttons = (72.0 + 6.0 + 28.0 + 6.0) * s;
    ((labels + buttons).ceil() as i32, px(40.0, s))
}

pub fn buttons(w: i32, h: i32, scale: f32, encoding: bool) -> Vec<(BarBtn, Rect)> {
    let b = px(28.0, scale);
    let pad = (h - b) / 2;
    let cancel = Rect::xywh(w - pad - b, pad, b, b);
    if encoding {
        vec![(BarBtn::Cancel, cancel)]
    } else {
        let sw = px(72.0, scale);
        vec![(BarBtn::Stop, Rect::xywh(cancel.l - px(6.0, scale) - sw, pad, sw, b)), (BarBtn::Cancel, cancel)]
    }
}

pub fn hit(w: i32, h: i32, scale: f32, encoding: bool, x: i32, y: i32) -> Option<BarBtn> {
    buttons(w, h, scale, encoding).into_iter().find(|(_, r)| r.contains(x, y)).map(|(b, _)| b)
}

/// Draws the whole bar into `cv` (which is exactly the bar's size).
pub fn draw(cv: &mut Canvas, scale: f32, st: &BarState, hover: Option<BarBtn>, fonts: &Fonts) {
    let (w, h, s) = (cv.w, cv.h, scale);
    cv.fill_px(Rect::new(0, 0, w, h), Rgba(28, 29, 32, 255));
    let fpx = 13.0 * s;
    let (_, lh) = text::line_metrics(&fonts.ui, fpx);
    let cy = h as f32 / 2.0;
    let ty = cy - lh / 2.0;
    let mut x = 14.0 * s;
    if !st.encoding {
        let on = (st.elapsed_ms / 500).is_multiple_of(2);
        cv.fill_circle(x + 5.0 * s, cy, 5.0 * s, if on { RED } else { RED.alpha(70) });
        x += 18.0 * s;
        let secs = st.elapsed_ms / 1000;
        let label = format!("REC {:02}:{:02}", secs / 60, secs % 60);
        cv.text(x, ty, &label, fpx, Rgba(255, 255, 255, 255), &fonts.bold);
        x += text::line_width(&fonts.bold, &label, fpx) + 10.0 * s;
        let small = 12.0 * s;
        let (_, slh) = text::line_metrics(&fonts.ui, small);
        cv.text(x, cy - slh / 2.0, &format!("{} frames", st.frames), small, Rgba(150, 152, 158, 255), &fonts.ui);
    } else {
        let total = st.frames.max(1);
        let pct = st.processed.min(total) * 100 / total;
        cv.text(x, ty, &format!("Encoding GIF…  {pct}%"), fpx, Rgba(255, 255, 255, 255), &fonts.ui);
        let track = Rect::new(px(14.0, s), h - px(6.0, s), w - px(54.0, s), h - px(3.0, s));
        cv.fill_rrect(track, 1.5 * s, Rgba(255, 255, 255, 35));
        let fill = Rect::new(track.l, track.t, track.l + (track.w() as usize * pct / 100) as i32, track.b);
        cv.fill_rrect(fill, 1.5 * s, Rgba(10, 132, 255, 255));
    }

    for (b, r) in buttons(w, h, s, st.encoding) {
        let hovered = hover == Some(b);
        match b {
            BarBtn::Stop => {
                cv.fill_rrect(r, 6.0 * s, if hovered { Rgba(244, 81, 72, 255) } else { RED });
                let ic = r.l as f32 + 16.0 * s;
                let mid = (r.t + r.b) as f32 / 2.0;
                cv.icon(Icon::Stop, ic, mid, 16.0 * s, Rgba(255, 255, 255, 255), fonts);
                cv.text(ic + 11.0 * s, mid - lh / 2.0, "Stop", fpx, Rgba(255, 255, 255, 255), &fonts.bold);
            }
            BarBtn::Cancel => {
                if hovered {
                    cv.fill_rrect(r, 6.0 * s, Rgba(255, 255, 255, 30));
                }
                let (cx, cy) = ((r.l + r.r) as f32 / 2.0, (r.t + r.b) as f32 / 2.0);
                cv.icon(Icon::Close, cx, cy, 16.0 * s, Rgba(200, 202, 208, 255), fonts);
            }
        }
    }
}
