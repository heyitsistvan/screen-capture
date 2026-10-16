//! Finding words on screen, so pixelate can replace them with fake ones before
//! pixelating. The recognizer is the OCR engine built into Windows, or
//! Tesseract on Linux; only word positions are used.

use crate::util::Rect;

#[cfg(target_os = "linux")]
use crate::linux::ocr as backend;
#[cfg(windows)]
use crate::win::ocr as backend;

/// A line of text: its word boxes.
#[derive(Debug, Default)]
pub struct Line {
    pub words: Vec<Rect>,
}

impl Line {
    pub fn bounds(&self) -> Rect {
        self.words.iter().fold(Rect::default(), |acc, w| acc.union(w))
    }
}

/// Lines of text in a BGRA image, or `None` if no OCR engine is available.
pub fn find_text(bgra: &[u8], w: i32, h: i32) -> Option<Vec<Line>> {
    let found = backend::recognize(bgra, w, h)?;
    let mut lines: Vec<Line> = Vec::new();
    for words in found {
        let mut line = Line::default();
        for r in words {
            add_word(&mut line.words, r);
        }
        add_line(&mut lines, line);
    }
    Some(lines)
}

/// The images to run OCR on, to catch as much text as possible: enlarged 2x
/// when that fits under `max` (small UI text is found more reliably), once
/// as-is and once inverted (light text on dark backgrounds). Returns the
/// scale and the two passes, or `None` if the image is too large.
pub(crate) fn passes(bgra: &[u8], w: i32, h: i32, max: i32) -> Option<(i32, [Vec<u8>; 2])> {
    let scale = if w.max(h) * 2 <= max { 2 } else { 1 };
    if w.max(h) * scale > max {
        return None;
    }
    Some((scale, [prepare(bgra, w, h, scale, false), prepare(bgra, w, h, scale, true)]))
}

/// A box found in a pass image, back in original image coordinates.
pub(crate) fn unscale(x: f32, y: f32, w: f32, h: f32, scale: i32) -> Rect {
    let s = scale as f32;
    Rect::new((x / s).floor() as i32, (y / s).floor() as i32, ((x + w) / s).ceil() as i32, ((y + h) / s).ceil() as i32)
}

fn mostly_overlap(a: &Rect, b: &Rect) -> bool {
    a.intersect(b).area() * 2 >= a.area().min(b.area())
}

/// Adds a word box, merging it with one it mostly overlaps (the same word
/// found in both passes). The merged box spans both horizontally, so no
/// letters are left out, but takes the shorter one's height: a pass that
/// barely made the word out can report a box reaching into the next line.
fn add_word(words: &mut Vec<Rect>, r: Rect) {
    if r.is_empty() {
        return;
    }
    match words.iter_mut().find(|w| mostly_overlap(w, &r)) {
        Some(w) => {
            let tight = if r.b - r.t < w.b - w.t { r } else { *w };
            *w = Rect::new(w.l.min(r.l), tight.t, w.r.max(r.r), tight.b);
        }
        None => words.push(r),
    }
}

/// Adds a line, merging it into one it mostly overlaps.
fn add_line(lines: &mut Vec<Line>, line: Line) {
    if line.words.is_empty() {
        return;
    }
    let b = line.bounds();
    match lines.iter_mut().find(|l| mostly_overlap(&l.bounds(), &b)) {
        Some(existing) => line.words.into_iter().for_each(|w| add_word(&mut existing.words, w)),
        None => lines.push(line),
    }
}

/// Integer upscale (nearest neighbor), optionally inverted, alpha opaque.
fn prepare(bgra: &[u8], w: i32, h: i32, scale: i32, invert: bool) -> Vec<u8> {
    let (w, h, s) = (w as usize, h as usize, scale as usize);
    let mut out = Vec::with_capacity(w * h * s * s * 4);
    let mut line = Vec::with_capacity(w * s * 4);
    for y in 0..h {
        line.clear();
        for x in 0..w {
            let i = (y * w + x) * 4;
            let p = if invert {
                [255 - bgra[i], 255 - bgra[i + 1], 255 - bgra[i + 2], 255]
            } else {
                [bgra[i], bgra[i + 1], bgra[i + 2], 255]
            };
            for _ in 0..s {
                line.extend_from_slice(&p);
            }
        }
        for _ in 0..s {
            out.extend_from_slice(&line);
        }
    }
    out
}
