//! Finds words with the OCR engine built into Windows (Windows.Media.Ocr).
//! Only word positions are used: pixelate replaces the text with random fake
//! words before pixelating, so the result can't be reversed.

use windows::core::HSTRING;
use windows::Globalization::Language;
use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};
use windows::Media::Ocr::OcrEngine;
use windows::Storage::Streams::DataWriter;
use windows::Win32::System::WinRT::{RoInitialize, RoUninitialize, RO_INIT_MULTITHREADED};

use crate::util::Rect;

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

/// Lines of text in a BGRA image, or `None` if OCR isn't available.
///
/// To catch as much text as possible, detection runs on the image enlarged
/// (small UI text is found more reliably) and again with colors inverted
/// (light text on dark backgrounds), and overlapping results are merged.
pub fn find_text(bgra: &[u8], w: i32, h: i32) -> Option<Vec<Line>> {
    let data = bgra.to_vec();
    // WinRT needs a multithreaded apartment; the UI thread must stay
    // single-threaded for the common dialogs, so run on a short-lived thread.
    std::thread::spawn(move || unsafe {
        let init = RoInitialize(RO_INIT_MULTITHREADED).is_ok();
        let result = detect(&data, w, h).ok();
        if init {
            RoUninitialize();
        }
        result
    })
    .join()
    .ok()
    .flatten()
}

fn engine() -> windows::core::Result<OcrEngine> {
    OcrEngine::TryCreateFromUserProfileLanguages()
        .or_else(|_| OcrEngine::TryCreateFromLanguage(&Language::CreateLanguage(&HSTRING::from("en-US"))?))
}

fn detect(bgra: &[u8], w: i32, h: i32) -> windows::core::Result<Vec<Line>> {
    let engine = engine()?;
    let max = OcrEngine::MaxImageDimension()? as i32;
    let scale = if w.max(h) * 2 <= max { 2 } else { 1 };
    if w.max(h) * scale > max {
        return Err(windows::core::Error::empty());
    }
    let s = scale as f32;
    let mut lines: Vec<Line> = Vec::new();
    for invert in [false, true] {
        let img = prepare(bgra, w, h, scale, invert);
        let writer = DataWriter::new()?;
        writer.WriteBytes(&img)?;
        let bitmap =
            SoftwareBitmap::CreateCopyFromBuffer(&writer.DetachBuffer()?, BitmapPixelFormat::Bgra8, w * scale, h * scale)?;
        let result = engine.RecognizeAsync(&bitmap)?.get()?;
        for line in result.Lines()? {
            let mut found = Line::default();
            for word in line.Words()? {
                let r = word.BoundingRect()?;
                add_word(
                    &mut found.words,
                    Rect::new(
                        (r.X / s).floor() as i32,
                        (r.Y / s).floor() as i32,
                        ((r.X + r.Width) / s).ceil() as i32,
                        ((r.Y + r.Height) / s).ceil() as i32,
                    ),
                );
            }
            add_line(&mut lines, found);
        }
    }
    Ok(lines)
}

fn mostly_overlap(a: &Rect, b: &Rect) -> bool {
    a.intersect(b).area() * 2 >= a.area().min(b.area())
}

/// Adds a word box, merging it with one it mostly overlaps (the same word
/// found in both passes).
fn add_word(words: &mut Vec<Rect>, r: Rect) {
    if r.is_empty() {
        return;
    }
    match words.iter_mut().find(|w| mostly_overlap(w, &r)) {
        Some(w) => *w = w.union(&r),
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
