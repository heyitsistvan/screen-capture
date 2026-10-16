//! `screen-capture --selftest <dir>` (debug builds only): exercises screen
//! capture, overlay rendering and GIF encoding without user interaction and
//! writes the results to `<dir>` for inspection.

use std::fs::{self, File};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use crate::annotate::{self, Rgba, Shape};
use crate::capture::{self, Grabber};
use crate::gif_enc::{self, Msg};
use crate::overlay;
use crate::text::{self, Fonts};
use crate::util::Rect;

pub fn run(dir: &Path, photos: Option<&Path>) -> i32 {
    let _ = fs::create_dir_all(dir);
    let fonts = match Fonts::load() {
        Ok(f) => Arc::new(f),
        Err(e) => {
            println!("FAIL fonts: {e}");
            return 1;
        }
    };
    let mut failures = 0;
    let mut check = |name: &str, r: Result<String, String>| match r {
        Ok(msg) => println!("ok   {name}: {msg}"),
        Err(e) => {
            failures += 1;
            println!("FAIL {name}: {e}");
        }
    };

    check("capture", {
        let t = Instant::now();
        let vs = capture::virtual_screen();
        capture::capture(vs)
            .ok_or_else(|| "capture returned nothing".to_string())
            .and_then(|px| {
                let elapsed = t.elapsed();
                let png = capture::encode_png(vs.w(), vs.h(), &px)?;
                fs::write(dir.join("screen.png"), png).map_err(|e| e.to_string())?;
                Ok(format!("{}x{} at ({}, {}) in {elapsed:?}", vs.w(), vs.h(), vs.l, vs.t))
            })
    });

    check("pixelate whole screen", {
        let vs = capture::virtual_screen();
        capture::capture(vs).ok_or_else(|| "capture failed".to_string()).map(|px| {
            let t = Instant::now();
            let lines = crate::ocr::find_text(&px, vs.w(), vs.h()).map_or(0, |l| l.len());
            let ocr = t.elapsed();
            let t = Instant::now();
            let _ = annotate::make_patch(&px, vs.w(), vs.h(), (0.0, 0.0), (vs.w() as f32, vs.h() as f32), 10.0, 1, &fonts);
            format!("OCR found {lines} text lines in {ocr:?}; full pixelate {:?}", t.elapsed())
        })
    });

    check("overlay", {
        let t = Instant::now();
        overlay::render_preview(fonts.clone(), &dir.join("overlay.png")).map(|_| format!("rendered in {:?}", t.elapsed()))
    });

    check("text editing", overlay::text_editing_selftest(fonts.clone()));
    check("drawing", overlay::drawing_selftest(fonts.clone()));

    check("outlines", {
        // Every palette color as arrow + text, on a light and a dark panel,
        // with the automatic outline for that panel.
        let (w, h) = (620, 300);
        let mut img = vec![0u8; (w * h * 4) as usize];
        let panels = [(Rgba(255, 255, 255, 255), 0), (Rgba(30, 30, 30, 255), 150)];
        let mut shapes = Vec::new();
        for (bg, y0) in panels {
            for y in y0..y0 + 150 {
                for x in 0..w {
                    let i = ((y * w + x) * 4) as usize;
                    img[i..i + 4].copy_from_slice(&[bg.2, bg.1, bg.0, 255]);
                }
            }
            for (i, &c) in crate::overlay::PALETTE.iter().enumerate() {
                let x = 15.0 + i as f32 * 55.0;
                let outline = Some(annotate::auto_outline(c, annotate::luma(bg)));
                shapes.push(Shape::Arrow { a: (x, y0 as f32 + 115.0), b: (x + 35.0, y0 as f32 + 60.0), color: c, width: 3.0, outline });
                shapes.push(Shape::Text { pos: (x, y0 as f32 + 15.0), text: "Ab".into(), color: c, size: 22.0, outline });
            }
        }
        annotate::render(&mut img, w, h, &shapes, 0.0, 0.0, &fonts);
        let _ = capture::encode_png(w, h, &img).map(|png| fs::write(dir.join("outlines.png"), png));
        let red = crate::overlay::PALETTE[0];
        match (annotate::auto_outline(red, 255), annotate::auto_outline(red, 30)) {
            (annotate::OUTLINE_BLACK, annotate::OUTLINE_WHITE) => Ok("red gets black on light, white on dark".into()),
            other => Err(format!("unexpected automatic outlines for red: {other:?}")),
        }
    });

    check("pixelate attack", {
        // The known-font attack: render every candidate password the same way,
        // pixelate it plainly, and pick the candidate closest to what we see.
        // It must beat plain pixelation (or it proves nothing) and fail
        // against ours.
        let (w, h, size, block) = (220, 40, 18.0, 8.0);
        let render = |s: &str| {
            let mut buf = vec![255u8; (w * h * 4) as usize];
            text::draw(&mut buf, w, h, 10.0, 8.0, s, size, Rgba(0, 0, 0, 255), &fonts.ui);
            buf
        };
        let whole = |patch| Shape::Pixelate { a: (0.0, 0.0), b: (w as f32, h as f32), block, seed: 0, patch };
        let plain = |img: &[u8]| {
            let mut out = img.to_vec();
            annotate::render(&mut out, w, h, &[whole(None)], 0.0, 0.0, &fonts);
            out
        };
        let alphabet = b"abcdefghijklmnopqrstuvwxyz0123456789";
        let mut seed = 0x5EED_u64;
        let mut rand = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) as usize
        };
        let candidates: Vec<String> =
            (0..300).map(|_| (0..8).map(|_| alphabet[rand() % alphabet.len()] as char).collect()).collect();
        let reference: Vec<Vec<u8>> = candidates.iter().map(|c| plain(&render(c))).collect();
        let guess = |seen: &[u8]| {
            let dist = |r: &Vec<u8>| r.iter().zip(seen).map(|(a, b)| (*a as i64 - *b as i64).pow(2)).sum::<i64>();
            (0..reference.len()).min_by_key(|&i| dist(&reference[i])).unwrap()
        };

        let trials = 15;
        let (mut plain_hits, mut safe_hits, mut found) = (0, 0, 0);
        let mut ocr_time = Duration::ZERO;
        let mut sheet = Vec::new();
        for i in 0..trials {
            let secret = (i * 37) % candidates.len();
            let img = render(&candidates[secret]);
            let t = Instant::now();
            let patch = annotate::make_patch(&img, w, h, (0.0, 0.0), (w as f32, h as f32), block, i as u64, &fonts);
            ocr_time += t.elapsed();
            let mut safe = img.clone();
            annotate::render(&mut safe, w, h, &[whole(patch)], 0.0, 0.0, &fonts);
            plain_hits += (guess(&plain(&img)) == secret) as usize;
            safe_hits += (guess(&safe) == secret) as usize;
            let (scrambled, words) = annotate::scramble_preview(&img, w, h, i as u64, &fonts);
            found += (words > 0) as usize;
            if i < 3 {
                for row in [&img, &scrambled, &plain(&img), &safe] {
                    sheet.extend_from_slice(row);
                }
            }
        }
        let _ = capture::encode_png(w, h * 12, &sheet).map(|png| fs::write(dir.join("pixelate_attack.png"), png));
        let summary = format!(
            "attack finds the password in plain pixelation {plain_hits}/{trials}, in ours {safe_hits}/{trials} \
             (guessing: 0.3%); OCR found the text {found}/{trials}; {:?} per pixelate",
            ocr_time / trials as u32
        );
        if plain_hits * 10 < trials * 9 {
            Err(format!("attack too weak to prove anything: {summary}"))
        } else if found < trials {
            Err(format!("OCR missed text, so this didn't test scrambling: {summary}"))
        } else if safe_hits > 1 {
            Err(format!("text leaked: {summary}"))
        } else {
            Ok(summary)
        }
    });

    if let Some(pdir) = photos {
        check("pixelate photos + text", {
            // Left half (x < 320) is a real photo: it must come out exactly
            // like plain pixelation (no phantom "text" found in it). Right
            // half is text: it must be scrambled.
            let mut files: Vec<_> = fs::read_dir(pdir)
                .map(|d| d.flatten().map(|e| e.path()).collect())
                .unwrap_or_default();
            files.retain(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("photo_")));
            files.sort();
            let mut report = Vec::new();
            let mut problems = Vec::new();
            for path in &files {
                let Some((w, h, img)) = load_png(path) else {
                    problems.push(format!("can't read {}", path.display()));
                    continue;
                };
                let whole = |patch| Shape::Pixelate { a: (0.0, 0.0), b: (w as f32, h as f32), block: 8.0, seed: 1, patch };
                let patch = annotate::make_patch(&img, w, h, (0.0, 0.0), (w as f32, h as f32), 8.0, 1, &fonts);
                let (mut plain, mut ours) = (img.clone(), img.clone());
                annotate::render(&mut plain, w, h, &[whole(None)], 0.0, 0.0, &fonts);
                annotate::render(&mut ours, w, h, &[whole(patch)], 0.0, 0.0, &fonts);
                let changed = |range: std::ops::Range<i32>| {
                    (0..h)
                        .flat_map(|y| range.clone().map(move |x| ((y * w + x) * 4) as usize))
                        .filter(|&i| plain[i..i + 4] != ours[i..i + 4])
                        .count()
                };
                let (photo, text) = (changed(0..320), changed(320..w));
                let name = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
                if photo > 0 {
                    problems.push(format!("{name}: {photo} photo pixels changed"));
                }
                if text == 0 {
                    problems.push(format!("{name}: text not scrambled"));
                }
                report.push(format!("{name} photo {photo}/text {text}"));
                let (scrambled, _) = annotate::scramble_preview(&img, w, h, 1, &fonts);
                let sheet: Vec<u8> = [&img, &scrambled, &plain, &ours].iter().flat_map(|v| v.iter().copied()).collect();
                let _ = capture::encode_png(w, h * 4, &sheet).map(|png| fs::write(dir.join(format!("{name}_result.png")), png));
            }
            if files.is_empty() {
                Err("no photo_*.png inputs".into())
            } else if problems.is_empty() {
                Ok(format!("changed pixels: {}", report.join(", ")))
            } else {
                Err(problems.join("; "))
            }
        });
    }

    check("synthetic gif", {
        let (w, h) = (320, 200);
        let (tx, rx) = mpsc::channel();
        for i in 0..40u64 {
            let mut px = vec![0u32; (w * h) as usize];
            for y in 0..h {
                for x in 0..w {
                    // A gradient (>255 colors, exercises NeuQuant) with a moving box.
                    let (r, g, b) = ((x * 255 / w) as u32, (y * 255 / h) as u32, 128u32);
                    let inside = x >= 20 + i as i32 * 6 && x < 60 + i as i32 * 6 && (80..120).contains(&y);
                    let c = if inside { 0x00FF_FFFF } else { r << 16 | g << 8 | b };
                    px[(y * w + x) as usize] = c | 0xFF00_0000;
                }
            }
            tx.send(Msg::Frame(px.clone(), i * 66)).unwrap();
            if i % 10 == 9 {
                // An identical frame, which must be merged away.
                tx.send(Msg::Frame(px, i * 66 + 33)).unwrap();
            }
        }
        tx.send(Msg::End(40 * 66)).unwrap();
        let shapes = vec![
            Shape::Arrow { a: (250.0, 40.0), b: (200.0, 90.0), color: Rgba(255, 59, 48, 255), width: 4.0, outline: Some(annotate::OUTLINE_WHITE) },
            Shape::Text { pos: (10.0, 10.0), text: "GIF test".into(), color: Rgba(255, 255, 255, 255), size: 20.0, outline: Some(annotate::OUTLINE_WHITE) },
        ];
        let out = dir.join("synthetic.gif");
        let t = Instant::now();
        gif_enc::encode(
            rx,
            w,
            h,
            |px| annotate::render(bytemuck::cast_slice_mut(px), w, h, &shapes, 0.0, 0.0, &fonts),
            &out,
            &AtomicBool::new(false),
            &AtomicUsize::new(0),
        )
        .and_then(|info| {
            let frames = verify_gif(&out, w as u16, h as u16)?;
            if frames != 40 {
                return Err(format!("expected 40 frames after merging duplicates, decoded {frames}"));
            }
            Ok(format!("{} frames, {} bytes in {:?}", info.frames, info.bytes, t.elapsed()))
        })
    });

    check("screen recording gif", {
        let vs = capture::virtual_screen();
        let region = Rect::xywh(vs.l, vs.t, 800.min(vs.w()), 450.min(vs.h()));
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let Some(mut g) = Grabber::new(region.w(), region.h()) else {
                let _ = tx.send(Msg::End(0));
                return;
            };
            let start = Instant::now();
            for _ in 0..30 {
                let t = start.elapsed().as_millis() as u64;
                let _ = tx.send(Msg::Frame(g.grab(region.l, region.t, true), t));
                thread::sleep(Duration::from_millis(66));
            }
            let _ = tx.send(Msg::End(start.elapsed().as_millis() as u64));
        });
        let out = dir.join("recording.gif");
        let t = Instant::now();
        gif_enc::encode(rx, region.w(), region.h(), |_| {}, &out, &AtomicBool::new(false), &AtomicUsize::new(0))
            .and_then(|info| {
                let frames = verify_gif(&out, region.w() as u16, region.h() as u16)?;
                Ok(format!(
                    "{}x{}, {frames} frames, {} bytes, {:.1}s, done {:?} after start",
                    info.w,
                    info.h,
                    info.bytes,
                    info.duration_ms as f64 / 1000.0,
                    t.elapsed()
                ))
            })
    });

    check("cancel", {
        let (tx, rx) = mpsc::channel();
        tx.send(Msg::Frame(vec![0xFF00_0000; 100], 0)).unwrap();
        let out = dir.join("cancelled.gif");
        let r = gif_enc::encode(rx, 10, 10, |_| {}, &out, &AtomicBool::new(true), &AtomicUsize::new(0));
        match r {
            Err(e) if e == gif_enc::CANCELLED && !out.exists() && !out.with_extension("gif.part").exists() => {
                Ok("cancelled cleanly".into())
            }
            other => Err(format!("unexpected: {:?}", other.map(|i| i.path))),
        }
    });

    if failures == 0 {
        println!("all checks passed; output in {}", dir.display());
        0
    } else {
        1
    }
}

/// Reads an 8-bit RGB/RGBA PNG as opaque BGRA.
fn load_png(path: &Path) -> Option<(i32, i32, Vec<u8>)> {
    let mut reader = png::Decoder::new(File::open(path).ok()?).read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    let channels = match info.color_type {
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        _ => return None,
    };
    let bgra = buf[..info.buffer_size()].chunks_exact(channels).flat_map(|p| [p[2], p[1], p[0], 255]).collect();
    Some((info.width as i32, info.height as i32, bgra))
}

/// Decodes a GIF fully and returns its frame count.
fn verify_gif(path: &Path, w: u16, h: u16) -> Result<usize, String> {
    let mut opts = gif::DecodeOptions::new();
    opts.set_color_output(gif::ColorOutput::RGBA);
    let mut dec = opts.read_info(File::open(path).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    if dec.width() != w || dec.height() != h {
        return Err(format!("size {}x{} != {w}x{h}", dec.width(), dec.height()));
    }
    let mut n = 0;
    while dec.read_next_frame().map_err(|e| e.to_string())?.is_some() {
        n += 1;
    }
    Ok(n)
}
