//! `screen-capture --selftest <dir>` (debug builds only): exercises screen
//! capture, overlay rendering and GIF/MP4 encoding without user interaction and
//! writes the results to `<dir>` for inspection.

use std::fs::{self, File};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use crate::annotate::{self, Rgba, Shape};
use crate::editor;
#[cfg(target_os = "linux")]
use crate::linux::{capture::Grabber, screen_for_test};
use crate::recording::{self, Format, Msg};
use crate::text::{self, Fonts};
use crate::util::{encode_png, Rect};
#[cfg(windows)]
use crate::win::{capture::Grabber, overlay::screen_for_test};

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
    let mut screen = None;
    let mut check = |name: &str, r: Result<String, String>| match r {
        Ok(msg) => println!("ok   {name}: {msg}"),
        Err(e) => {
            failures += 1;
            println!("FAIL {name}: {e}");
        }
    };

    check("capture", {
        let t = Instant::now();
        screen_for_test().ok_or_else(|| "capture returned nothing".to_string()).and_then(|sc| {
            let elapsed = t.elapsed();
            let png = encode_png(sc.w, sc.h, &sc.pixels)?;
            fs::write(dir.join("screen.png"), png).map_err(|e| e.to_string())?;
            Ok(format!("{}x{} at {:?}, {} monitor(s), in {elapsed:?}", sc.w, sc.h, sc.origin, sc.monitors.len()))
        })
    });

    check("pixelate whole screen", {
        screen_for_test().ok_or_else(|| "capture failed".to_string()).map(|sc| {
            let (px, w, h) = (sc.pixels, sc.w, sc.h);
            let t = Instant::now();
            let lines = crate::ocr::find_text(&px, w, h).map_or(0, |l| l.len());
            let ocr = t.elapsed();
            let t = Instant::now();
            let _ = annotate::make_patch(&px, w, h, (0.0, 0.0), (w as f32, h as f32), 10.0, 1, &fonts);
            format!("OCR found {lines} text lines in {ocr:?}; full pixelate {:?}", t.elapsed())
        })
    });

    check("overlay", {
        let t = Instant::now();
        screen_for_test()
            .ok_or_else(|| "capture failed".to_string())
            .and_then(|screen| editor::render_preview(fonts.clone(), screen, &dir.join("overlay.png")))
            .map(|_| format!("rendered in {:?}", t.elapsed()))
    });
    // The record button names the format; MP4 is the wider label.
    check("overlay mp4", {
        let format = editor::record_format();
        editor::set_record_format(Format::Mp4);
        let r = screen_for_test()
            .ok_or_else(|| "capture failed".to_string())
            .and_then(|screen| editor::render_preview(fonts.clone(), screen, &dir.join("overlay_mp4.png")))
            .map(|_| "rendered with the MP4 record button".to_string());
        editor::set_record_format(format);
        r
    });

    check("text editing", editor::text_editing_selftest(fonts.clone()));
    check("drawing", editor::drawing_selftest(fonts.clone()));
    check("pin mode", editor::pin_mode_selftest(fonts.clone()));
    check("pin view", crate::pin::selftest(&fonts, &dir.join("pin.png")));
    #[cfg(windows)]
    check("pin window", crate::win::pin::selftest(fonts.clone()));
    check("history", crate::history::selftest());

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
            for (i, &c) in editor::PALETTE.iter().enumerate() {
                let x = 15.0 + i as f32 * 55.0;
                let outline = Some(annotate::auto_outline(c, annotate::luma(bg)));
                shapes.push(Shape::Arrow { a: (x, y0 as f32 + 115.0), b: (x + 35.0, y0 as f32 + 60.0), color: c, width: 3.0, outline });
                shapes.push(Shape::Text { pos: (x, y0 as f32 + 15.0), text: "Ab".into(), color: c, size: 22.0, outline });
            }
        }
        annotate::render(&mut img, w, h, &shapes, 0.0, 0.0, &fonts);
        let _ = encode_png(w, h, &img).map(|png| fs::write(dir.join("outlines.png"), png));
        let red = editor::PALETTE[0];
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
        let _ = encode_png(w, h * 12, &sheet).map(|png| fs::write(dir.join("pixelate_attack.png"), png));
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
                let _ = encode_png(w, h * 4, &sheet).map(|png| fs::write(dir.join(format!("{name}_result.png")), png));
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

    for format in Format::ALL {
        let ext = format.extension();
        check(&format!("synthetic {ext}"), {
            let (w, h) = (320, 200);
            let (tx, rx) = mpsc::channel();
            for (px, t) in synthetic_frames(w, h) {
                tx.send(Msg::Frame(px, t)).unwrap();
            }
            tx.send(Msg::End(40 * 66)).unwrap();
            let shapes = vec![
                Shape::Arrow { a: (250.0, 40.0), b: (200.0, 90.0), color: Rgba(255, 59, 48, 255), width: 4.0, outline: Some(annotate::OUTLINE_WHITE) },
                Shape::Text { pos: (10.0, 10.0), text: format!("{} test", format.label()), color: Rgba(255, 255, 255, 255), size: 20.0, outline: Some(annotate::OUTLINE_WHITE) },
            ];
            let out = dir.join(format!("synthetic.{ext}"));
            let t = Instant::now();
            let processed = AtomicUsize::new(0);
            recording::encode(
                format,
                rx,
                w,
                h,
                15,
                |px| annotate::render(bytemuck::cast_slice_mut(px), w, h, &shapes, 0.0, 0.0, &fonts),
                &out,
                &AtomicBool::new(false),
                &processed,
            )
            .and_then(|info| {
                let frames = verify(format, &out, w, h)?;
                if frames != 40 || info.frames != 40 {
                    return Err(format!("expected 40 frames after merging duplicates, stored {} and decoded {frames}", info.frames));
                }
                if processed.load(Relaxed) != 44 {
                    return Err(format!("progress counted {} of 44 captured frames", processed.load(Relaxed)));
                }
                if format == Format::Mp4 && info.duration_ms != 40 * 66 {
                    return Err(format!("duration {} ms, expected {}", info.duration_ms, 40 * 66));
                }
                Ok(format!("{} frames, {} bytes in {:?}", info.frames, info.bytes, t.elapsed()))
            })
        });
    }

    check("screen frames", {
        let (origin, w, h) = screen_for_test().map_or(((0, 0), 800, 450), |sc| (sc.origin, sc.w, sc.h));
        let region = Rect::xywh(origin.0, origin.1, 800.min(w), 450.min(h));
        let frames = thread::spawn(move || {
            let mut g = Grabber::new(region.w(), region.h())?;
            let start = Instant::now();
            let mut frames = Vec::new();
            for _ in 0..30 {
                let t = start.elapsed().as_millis() as u64;
                frames.push((g.grab(region.l, region.t, true), t));
                thread::sleep(Duration::from_millis(66));
            }
            Some((frames, start.elapsed().as_millis() as u64))
        })
        .join()
        .ok()
        .flatten();
        match frames {
            Some((frames, end)) => {
                let n = frames.len();
                screen = Some((region, frames, end));
                Ok(format!("{n} frames of {}x{}", region.w(), region.h()))
            }
            None => Err("couldn't grab the screen".into()),
        }
    });
    if let Some((region, frames, end)) = &screen {
        for format in Format::ALL {
            let ext = format.extension();
            check(&format!("screen recording {ext}"), {
                let (tx, rx) = mpsc::channel();
                for (px, t) in frames {
                    tx.send(Msg::Frame(px.clone(), *t)).unwrap();
                }
                tx.send(Msg::End(*end)).unwrap();
                let out = dir.join(format!("recording.{ext}"));
                let t = Instant::now();
                recording::encode(format, rx, region.w(), region.h(), 15, |_| {}, &out, &AtomicBool::new(false), &AtomicUsize::new(0))
                    .and_then(|info| {
                        let frames = verify(format, &out, info.w, info.h)?;
                        Ok(format!(
                            "{}x{}, {frames} frames, {} bytes, {:.1}s, encoded in {:?}",
                            info.w,
                            info.h,
                            info.bytes,
                            info.duration_ms as f64 / 1000.0,
                            t.elapsed()
                        ))
                    })
            });
        }
    }

    // H.264 needs even sizes and has limits: odd, tiny and huge areas must
    // each give a video (trimmed or scaled) or a clear error.
    check("mp4 sizes", {
        let mut report = Vec::new();
        let mut problems = Vec::new();
        for (w, h) in [(2, 2), (3, 3), (31, 17), (17, 640), (4000, 10), (1366, 768), (4096, 2304), (5120, 1440), (1440, 2560), (2160, 3840)] {
            let (tx, rx) = mpsc::channel();
            for i in 0..3u64 {
                let px: Vec<u32> = (0..w * h).map(|p| 0xFF00_0000 | ((p as u32).wrapping_mul(2654435761) >> 8) ^ (i as u32 * 0x30_3030)).collect();
                tx.send(Msg::Frame(px, i * 100)).unwrap();
            }
            tx.send(Msg::End(300)).unwrap();
            let out = dir.join(format!("size_{w}x{h}.mp4"));
            match recording::encode(Format::Mp4, rx, w, h, 10, |_| {}, &out, &AtomicBool::new(false), &AtomicUsize::new(0)) {
                Ok(info) => match verify(Format::Mp4, &out, info.w, info.h) {
                    Ok(3) => {
                        report.push(format!("{w}x{h}→{}x{}", info.w, info.h));
                        let _ = fs::remove_file(&out);
                    }
                    Ok(n) => problems.push(format!("{w}x{h}: {n} frames")),
                    Err(e) => problems.push(format!("{w}x{h}: {e}")),
                },
                Err(e) => {
                    if out.with_extension("mp4.part").exists() {
                        problems.push(format!("{w}x{h}: left a .part file"));
                    }
                    report.push(format!("{w}x{h}: error \"{e}\""));
                }
            }
        }
        if problems.is_empty() {
            Ok(report.join(", "))
        } else {
            Err(problems.join("; "))
        }
    });

    // Full HD where every frame changes (the worst case): how fast each
    // format encodes compared with real time. Frames left over are encoded
    // after Stop, with progress.
    for format in Format::ALL {
        let ext = format.extension();
        check(&format!("{ext} speed"), {
            let (w, h, n) = (1920, 1080, 30u64);
            let (tx, rx) = mpsc::channel();
            for i in 0..n {
                let px: Vec<u32> = (0..w * h)
                    .map(|p| {
                        let (x, y) = ((p % w) as u32, (p / w) as u32);
                        0xFF00_0000 | ((x + i as u32 * 8) & 0xFF) << 16 | ((y + i as u32 * 4) & 0xFF) << 8 | ((x ^ y) & 0xFF)
                    })
                    .collect();
                tx.send(Msg::Frame(px, i * 33)).unwrap();
            }
            tx.send(Msg::End(n * 33)).unwrap();
            let out = dir.join(format!("speed.{ext}"));
            let t = Instant::now();
            recording::encode(format, rx, w, h, 30, |_| {}, &out, &AtomicBool::new(false), &AtomicUsize::new(0)).map(|info| {
                let secs = t.elapsed().as_secs_f64();
                let _ = fs::remove_file(&out);
                format!("{w}x{h}: {} frames in {secs:.2}s ({:.0} fps), {} bytes", info.frames, info.frames as f64 / secs, info.bytes)
            })
        });
    }

    for format in Format::ALL {
        let ext = format.extension();
        check(&format!("cancel {ext}"), {
            let (tx, rx) = mpsc::channel();
            tx.send(Msg::Frame(vec![0xFF00_0000; 100], 0)).unwrap();
            tx.send(Msg::Frame(vec![0xFFFF_FFFF; 100], 50)).unwrap();
            let out = dir.join(format!("cancelled.{ext}"));
            let cancel = AtomicBool::new(false);
            // Cancelled once the encoder is under way (the file is open).
            let r = recording::encode(format, rx, 10, 10, 15, |_| cancel.store(true, Relaxed), &out, &cancel, &AtomicUsize::new(0));
            let part = out.with_extension(format!("{ext}.part"));
            match r {
                Err(e) if e == recording::CANCELLED && !out.exists() && !part.exists() => Ok("cancelled cleanly".into()),
                other => Err(format!("unexpected: {:?}, part left: {}", other.map(|i| i.path), part.exists())),
            }
        });
    }

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

/// 40 frames of a gradient (>255 colors, exercises NeuQuant) with a moving
/// box, plus 4 duplicates that must be merged away; 66 ms apart.
fn synthetic_frames(w: i32, h: i32) -> Vec<(Vec<u32>, u64)> {
    let mut frames = Vec::new();
    for i in 0..40u64 {
        let mut px = vec![0u32; (w * h) as usize];
        for y in 0..h {
            for x in 0..w {
                let (r, g, b) = ((x * 255 / w) as u32, (y * 255 / h) as u32, 128u32);
                let inside = x >= 20 + i as i32 * 6 && x < 60 + i as i32 * 6 && (80..120).contains(&y);
                let c = if inside { 0x00FF_FFFF } else { r << 16 | g << 8 | b };
                px[(y * w + x) as usize] = c | 0xFF00_0000;
            }
        }
        frames.push((px.clone(), i * 66));
        if i % 10 == 9 {
            frames.push((px, i * 66 + 33));
        }
    }
    frames
}

/// Checks a recording's size and returns its frame count.
fn verify(format: Format, path: &Path, w: i32, h: i32) -> Result<usize, String> {
    match format {
        Format::Gif => verify_gif(path, w as u16, h as u16),
        Format::Mp4 => verify_mp4(path, w, h),
    }
}

/// The payload of the first box of type `name` in `data` (top level only).
fn mp4_box<'a>(data: &'a [u8], name: &[u8; 4]) -> Option<&'a [u8]> {
    let mut rest = data;
    while rest.len() >= 8 {
        let size = u32::from_be_bytes(rest[0..4].try_into().ok()?) as u64;
        let (size, header) = match size {
            0 => (rest.len() as u64, 8),
            1 => (u64::from_be_bytes(rest.get(8..16)?.try_into().ok()?), 16),
            s => (s, 8),
        };
        if size < header as u64 || size > rest.len() as u64 {
            return None;
        }
        if &rest[4..8] == name {
            return Some(&rest[header..size as usize]);
        }
        rest = &rest[size as usize..];
    }
    None
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// Walks an MP4's boxes: one H.264 track of `w` × `h`, its index before the
/// data (so it streams), and the sample count.
fn verify_mp4(path: &Path, w: i32, h: i32) -> Result<usize, String> {
    let data = fs::read(path).map_err(|e| e.to_string())?;
    if data.get(4..8) != Some(b"ftyp") {
        return Err("doesn't start with ftyp".into());
    }
    let pos = |name: &[u8]| data.windows(4).position(|w| w == name);
    if pos(b"moov") > pos(b"mdat") {
        return Err("moov is after mdat (not streamable)".into());
    }
    let moov = mp4_box(&data, b"moov").ok_or("no moov")?;
    let trak = mp4_box(moov, b"trak").ok_or("no trak")?;
    let tkhd = mp4_box(trak, b"tkhd").ok_or("no tkhd")?;
    let n = tkhd.len();
    let (tw, th) = (be32(tkhd, n - 8).ok_or("short tkhd")? >> 16, be32(tkhd, n - 4).ok_or("short tkhd")? >> 16);
    if (tw as i32, th as i32) != (w, h) {
        return Err(format!("track is {tw}x{th}, expected {w}x{h}"));
    }
    let stbl = mp4_box(trak, b"mdia").and_then(|m| mp4_box(m, b"minf")).and_then(|m| mp4_box(m, b"stbl")).ok_or("no stbl")?;
    let stsd = mp4_box(stbl, b"stsd").ok_or("no stsd")?;
    if stsd.get(12..16) != Some(b"avc1") {
        return Err("not H.264 (avc1)".into());
    }
    let stsz = mp4_box(stbl, b"stsz").ok_or("no stsz")?;
    Ok(be32(stsz, 8).ok_or("short stsz")? as usize)
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
