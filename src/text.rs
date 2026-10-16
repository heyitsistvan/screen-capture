//! Text rasterization with fontdue, using the system's fonts (Segoe UI on
//! Windows, the fontconfig sans-serif font on Linux).

use std::path::PathBuf;

use fontdue::{Font, FontSettings};

use crate::annotate::Rgba;

pub struct Fonts {
    /// Regular weight, for UI labels.
    pub ui: Font,
    /// Bold weight, for text annotations.
    pub bold: Font,
}

impl Fonts {
    #[cfg(windows)]
    pub fn load() -> Result<Fonts, String> {
        let dir = std::env::var_os("WINDIR").map(PathBuf::from).unwrap_or_else(|| "C:\\Windows".into()).join("Fonts");
        let paths = |names: &[&str]| names.iter().map(|n| dir.join(n)).collect::<Vec<_>>();
        let regular = paths(&["segoeui.ttf", "arial.ttf", "tahoma.ttf", "verdana.ttf"]);
        let bold = paths(&["segoeuib.ttf", "arialbd.ttf", "tahomabd.ttf"]);
        Self::from_candidates(&regular, &bold).ok_or_else(|| format!("No usable font found in {}", dir.display()))
    }

    #[cfg(target_os = "linux")]
    pub fn load() -> Result<Fonts, String> {
        let known = |names: &[&str]| -> Vec<PathBuf> {
            let dirs = [
                "/usr/share/fonts/truetype/dejavu",
                "/usr/share/fonts/truetype/noto",
                "/usr/share/fonts/noto",
                "/usr/share/fonts/google-noto",
                "/usr/share/fonts/TTF",
                "/usr/share/fonts/dejavu",
                "/usr/share/fonts/truetype/liberation",
                "/usr/share/fonts/liberation-sans",
            ];
            dirs.iter().flat_map(|d| names.iter().map(move |n| std::path::Path::new(d).join(n))).collect()
        };
        let mut regular: Vec<PathBuf> = fc_match("sans-serif").into_iter().collect();
        regular.extend(known(&["NotoSans-Regular.ttf", "DejaVuSans.ttf", "LiberationSans-Regular.ttf"]));
        let mut bold: Vec<PathBuf> = fc_match("sans-serif:bold").into_iter().collect();
        bold.extend(known(&["NotoSans-Bold.ttf", "DejaVuSans-Bold.ttf", "LiberationSans-Bold.ttf"]));
        Self::from_candidates(&regular, &bold)
            .ok_or_else(|| "No usable font found; install a font such as fonts-dejavu or fonts-noto".into())
    }

    fn from_candidates(regular: &[PathBuf], bold: &[PathBuf]) -> Option<Fonts> {
        let ui = load_first(regular)?;
        let bold = load_first(bold).or_else(|| load_first(regular))?;
        Some(Fonts { ui, bold })
    }
}

/// The font file fontconfig picks for a pattern.
#[cfg(target_os = "linux")]
fn fc_match(pattern: &str) -> Option<PathBuf> {
    let out = std::process::Command::new("fc-match").args(["-f", "%{file}", pattern]).output().ok()?;
    let path = PathBuf::from(String::from_utf8(out.stdout).ok()?.trim());
    path.is_file().then_some(path)
}

fn load_first(paths: &[PathBuf]) -> Option<Font> {
    paths.iter().find_map(|p| {
        let data = std::fs::read(p).ok()?;
        Font::from_bytes(data, FontSettings::default()).ok()
    })
}

/// Returns `(ascent, line_height)` for a font size in pixels.
pub fn line_metrics(font: &Font, px: f32) -> (f32, f32) {
    match font.horizontal_line_metrics(px) {
        Some(m) => (m.ascent, m.new_line_size),
        None => (px * 0.8, px * 1.25),
    }
}

pub fn line_width(font: &Font, line: &str, px: f32) -> f32 {
    let mut x = 0.0;
    let mut prev = None;
    for ch in line.chars() {
        if let Some(p) = prev {
            x += font.horizontal_kern(p, ch, px).unwrap_or(0.0);
        }
        x += font.metrics(ch, px).advance_width;
        prev = Some(ch);
    }
    x
}

/// Size of a (possibly multi-line) string: `(width, height)`.
pub fn measure(font: &Font, s: &str, px: f32) -> (f32, f32) {
    let (_, lh) = line_metrics(font, px);
    let mut w: f32 = 0.0;
    let mut lines = 0;
    for line in s.split('\n') {
        w = w.max(line_width(font, line, px));
        lines += 1;
    }
    (w, lh * lines as f32)
}

/// A rasterized glyph placed in buffer coordinates.
struct Glyph {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    coverage: Vec<u8>,
}

/// Lays out and rasterizes text with its top-left corner at `(x, y)`,
/// skipping lines that start below `max_y`.
fn layout(x: f32, y: f32, s: &str, px: f32, font: &Font, max_y: f32) -> Vec<Glyph> {
    let (ascent, lh) = line_metrics(font, px);
    let mut glyphs = Vec::new();
    for (i, line) in s.split('\n').enumerate() {
        let baseline = y + ascent + lh * i as f32;
        if baseline - ascent > max_y {
            break;
        }
        let mut pen = x;
        let mut prev = None;
        for ch in line.chars() {
            if let Some(p) = prev {
                pen += font.horizontal_kern(p, ch, px).unwrap_or(0.0);
            }
            let (m, coverage) = font.rasterize(ch, px);
            glyphs.push(Glyph {
                x: (pen + m.xmin as f32).round() as i32,
                y: (baseline - m.ymin as f32 - m.height as f32).round() as i32,
                w: m.width as i32,
                h: m.height as i32,
                coverage,
            });
            pen += m.advance_width;
            prev = Some(ch);
        }
    }
    glyphs
}

/// Draws text with its top-left corner at `(x, y)` into a BGRA buffer.
#[allow(clippy::too_many_arguments)]
pub fn draw(buf: &mut [u8], bw: i32, bh: i32, x: f32, y: f32, s: &str, px: f32, c: Rgba, font: &Font) {
    for g in layout(x, y, s, px, font, bh as f32) {
        blend_coverage(buf, bw, bh, g.x, g.y, g.w, g.h, &g.coverage, c);
    }
}

/// Like [`draw`], optionally with a halo of `(color, radius)` around the
/// letters: their combined shapes grown by the radius, with soft edges.
#[allow(clippy::too_many_arguments)]
pub fn draw_outlined(
    buf: &mut [u8],
    bw: i32,
    bh: i32,
    x: f32,
    y: f32,
    s: &str,
    px: f32,
    c: Rgba,
    halo: Option<(Rgba, f32)>,
    font: &Font,
) {
    let glyphs = layout(x, y, s, px, font, bh as f32);
    if let Some((hc, radius)) = halo {
        let m = radius.ceil() as i32 + 1;
        let l = glyphs.iter().map(|g| g.x).min().unwrap_or(0) - m;
        let t = glyphs.iter().map(|g| g.y).min().unwrap_or(0) - m;
        let r = glyphs.iter().map(|g| g.x + g.w).max().unwrap_or(0) + m;
        let b = glyphs.iter().map(|g| g.y + g.h).max().unwrap_or(0) + m;
        let (mw, mh) = (r - l, b - t);
        if mw > 0 && mh > 0 {
            let mut mask = vec![0u8; (mw * mh) as usize];
            for g in &glyphs {
                for row in 0..g.h {
                    for col in 0..g.w {
                        let i = ((g.y - t + row) * mw + (g.x - l + col)) as usize;
                        mask[i] = mask[i].max(g.coverage[(row * g.w + col) as usize]);
                    }
                }
            }
            let grown = dilate(&mask, mw, mh, radius);
            blend_coverage(buf, bw, bh, l, t, mw, mh, &grown, hc);
        }
    }
    for g in &glyphs {
        blend_coverage(buf, bw, bh, g.x, g.y, g.w, g.h, &g.coverage, c);
    }
}

/// Grows a coverage mask by `radius` pixels (a soft-edged disc max filter).
fn dilate(mask: &[u8], w: i32, h: i32, radius: f32) -> Vec<u8> {
    let ri = radius.ceil() as i32;
    let mut kernel = Vec::new();
    for dy in -ri..=ri {
        for dx in -ri..=ri {
            let weight = (radius + 0.5 - ((dx * dx + dy * dy) as f32).sqrt()).clamp(0.0, 1.0);
            if weight > 0.0 {
                kernel.push((dx, dy, (weight * 256.0) as u32));
            }
        }
    }
    let mut out = vec![0u8; mask.len()];
    for y in 0..h {
        for x in 0..w {
            let mut best = 0u32;
            for &(dx, dy, wt) in &kernel {
                let (sx, sy) = (x + dx, y + dy);
                if sx >= 0 && sy >= 0 && sx < w && sy < h {
                    best = best.max(mask[(sy * w + sx) as usize] as u32 * wt >> 8);
                }
            }
            out[(y * w + x) as usize] = best.min(255) as u8;
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn blend_coverage(buf: &mut [u8], bw: i32, bh: i32, gx: i32, gy: i32, gw: i32, gh: i32, cov: &[u8], c: Rgba) {
    for row in 0..gh {
        let y = gy + row;
        if y < 0 || y >= bh {
            continue;
        }
        for col in 0..gw {
            let x = gx + col;
            if x < 0 || x >= bw {
                continue;
            }
            let a = cov[(row * gw + col) as usize] as u32 * c.3 as u32 / 255;
            if a == 0 {
                continue;
            }
            let i = ((y * bw + x) * 4) as usize;
            let ia = 255 - a;
            buf[i] = ((c.2 as u32 * a + buf[i] as u32 * ia) / 255) as u8;
            buf[i + 1] = ((c.1 as u32 * a + buf[i + 1] as u32 * ia) / 255) as u8;
            buf[i + 2] = ((c.0 as u32 * a + buf[i + 2] as u32 * ia) / 255) as u8;
            buf[i + 3] = (a + buf[i + 3] as u32 * ia / 255) as u8;
        }
    }
}
