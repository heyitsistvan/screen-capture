//! Word boxes from Tesseract (the `tesseract` command, if installed).

use std::collections::BTreeMap;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use crate::ocr;
use crate::util::Rect;

/// Tesseract has no hard limit; this keeps a pass to a few seconds.
const MAX_DIMENSION: i32 = 8000;
/// One- and two-letter "words" below this confidence are mostly noise found
/// in images. Longer words are kept whatever their confidence: passwords and
/// keys aren't dictionary words, so Tesseract is never sure of them.
const SHORT_WORD_CONFIDENCE: f32 = 50.0;

fn available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| Command::new("tesseract").arg("--version").output().is_ok_and(|o| o.status.success()))
}

/// Lines of word boxes found in a BGRA image (all passes), or `None` if
/// Tesseract isn't installed.
pub fn recognize(bgra: &[u8], w: i32, h: i32) -> Option<Vec<Vec<Rect>>> {
    if !available() {
        return None;
    }
    let (scale, passes) = ocr::passes(bgra, w, h, MAX_DIMENSION)?;
    let (sw, sh) = (w * scale, h * scale);
    // Tesseract works on one image at a time; run both passes in parallel.
    let runs: Vec<_> = passes.into_iter().map(|img| std::thread::spawn(move || run(&img, sw, sh))).collect();
    let mut lines = Vec::new();
    for run in runs {
        for words in run.join().ok()?? {
            lines.push(words.into_iter().map(|(x, y, w, h)| ocr::unscale(x, y, w, h, scale)).collect());
        }
    }
    Some(lines)
}

type Word = (f32, f32, f32, f32);

fn run(bgra: &[u8], w: i32, h: i32) -> Option<Vec<Vec<Word>>> {
    let png = crate::util::encode_png(w, h, bgra).ok()?;
    let mut child = Command::new("tesseract")
        // Automatic layout skips photos instead of reading "words" in them;
        // Sauvola thresholding finds text on any background (the default
        // single threshold loses e.g. green text on a dark panel). It reads
        // dark-on-light only, so the inverted pass finds the rest.
        .args(["stdin", "stdout", "--psm", "3", "-c", "thresholding_method=2", "tsv"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdin = child.stdin.take()?;
    let writer = std::thread::spawn(move || stdin.write_all(&png));
    let out = child.wait_with_output().ok()?;
    writer.join().ok()?.ok()?;
    Some(parse_tsv(&String::from_utf8_lossy(&out.stdout)))
}

/// Lines of word boxes from Tesseract's TSV output: level page block par
/// line word left top width height conf text.
fn parse_tsv(tsv: &str) -> Vec<Vec<Word>> {
    let mut lines: BTreeMap<(&str, &str, &str, &str), Vec<Word>> = BTreeMap::new();
    for row in tsv.lines().skip(1) {
        let c: Vec<&str> = row.split('\t').collect();
        if c.len() < 12 || c[0] != "5" {
            continue;
        }
        let conf: f32 = c[10].parse().unwrap_or(-1.0);
        let num = |i: usize| c[i].parse::<f32>().unwrap_or(0.0);
        let (w, h) = (num(8), num(9));
        let letters = c[11].chars().filter(|ch| ch.is_alphanumeric()).count();
        if letters > 0 && (letters >= 3 || conf >= SHORT_WORD_CONFIDENCE) && w >= 2.0 && h >= 4.0 {
            lines.entry((c[1], c[2], c[3], c[4])).or_default().push((num(6), num(7), w, h));
        }
    }
    lines.into_values().collect()
}
