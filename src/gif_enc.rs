//! Animated GIF encoding tuned for screen recordings.
//!
//! - Frames identical to the previous one are merged (their time is added to
//!   the previous frame's delay).
//! - Each frame only stores the bounding box of the pixels that changed;
//!   unchanged pixels inside it are transparent so the previous frame shows
//!   through. Typical UI recordings shrink dramatically this way.
//! - Every sub-frame gets its own palette: exact if it has at most 255 colors,
//!   NeuQuant otherwise. Quantization and LZW compression run in parallel.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::sync::mpsc::Receiver;

use color_quant::NeuQuant;
use rayon::prelude::*;

pub const CANCELLED: &str = "cancelled";

pub enum Msg {
    /// Pixels as little-endian BGRA words with alpha 255, and a timestamp in ms.
    Frame(Vec<u32>, u64),
    /// Recording stopped at this timestamp (ms).
    End(u64),
}

#[derive(Debug)]
pub struct GifInfo {
    pub path: PathBuf,
    pub bytes: u64,
    pub frames: usize,
    pub duration_ms: u64,
    pub w: i32,
    pub h: i32,
}

struct Job {
    left: u16,
    top: u16,
    w: u16,
    h: u16,
    /// BGRA words; 0 (alpha 0) marks "unchanged, keep previous frame".
    px: Vec<u32>,
    transparent: bool,
    delay: u16,
    /// Number of captured frames this job represents (for progress).
    raw: usize,
}

/// Encodes frames from `rx` until `Msg::End` (or the sender hangs up).
/// `overlay` is applied to every frame before encoding (annotations).
pub fn encode(
    rx: Receiver<Msg>,
    w: i32,
    h: i32,
    mut overlay: impl FnMut(&mut [u32]),
    out: &Path,
    cancel: &AtomicBool,
    processed: &AtomicUsize,
) -> Result<GifInfo, String> {
    let part = out.with_extension("gif.part");
    let result = encode_to(&rx, w, h, &mut overlay, &part, cancel, processed)
        .and_then(|r| fs::rename(&part, out).map(|_| r).map_err(|e| format!("Cannot save {}: {e}", out.display())));
    match result {
        Ok((frames, duration_ms)) => Ok(GifInfo {
            path: out.to_path_buf(),
            bytes: fs::metadata(out).map(|m| m.len()).unwrap_or(0),
            frames,
            duration_ms,
            w,
            h,
        }),
        Err(e) => {
            let _ = fs::remove_file(&part);
            Err(e)
        }
    }
}

fn encode_to(
    rx: &Receiver<Msg>,
    w: i32,
    h: i32,
    overlay: &mut impl FnMut(&mut [u32]),
    part: &Path,
    cancel: &AtomicBool,
    processed: &AtomicUsize,
) -> Result<(usize, u64), String> {
    if w <= 0 || h <= 0 || w > u16::MAX as i32 || h > u16::MAX as i32 {
        return Err(format!("Unsupported GIF size {w}x{h}"));
    }
    if let Some(dir) = part.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let file = File::create(part).map_err(|e| format!("Cannot create {}: {e}", part.display()))?;
    let mut enc = gif::Encoder::new(BufWriter::new(file), w as u16, h as u16, &[]).map_err(|e| e.to_string())?;
    enc.set_repeat(gif::Repeat::Infinite).map_err(|e| e.to_string())?;

    let batch_size = (rayon::current_num_threads() * 2).max(4);
    let mut prev: Option<Vec<u32>> = None;
    let mut pending: Option<(Job, u64)> = None; // job + start time in centiseconds
    let mut batch = Vec::new();
    let mut written = 0usize;
    let mut end_ms = 0u64;

    for msg in rx.iter() {
        if cancel.load(Relaxed) {
            return Err(CANCELLED.into());
        }
        match msg {
            Msg::Frame(mut px, t) => {
                overlay(&mut px);
                let cs = (t + 5) / 10;
                let job = match &prev {
                    None => Some(full_job(&px, w as u16, h as u16)),
                    Some(p) => diff_job(p, &px, w as usize, h as usize),
                };
                match job {
                    Some(job) => {
                        if let Some((mut pj, start)) = pending.take() {
                            pj.delay = delay_cs(start, cs);
                            batch.push(pj);
                        }
                        pending = Some((job, cs));
                    }
                    None => {
                        if let Some((pj, _)) = pending.as_mut() {
                            pj.raw += 1;
                        }
                    }
                }
                prev = Some(px);
                if batch.len() >= batch_size {
                    written += flush(&mut enc, &mut batch, cancel, processed)?;
                }
            }
            Msg::End(t) => {
                end_ms = t;
                break;
            }
        }
    }
    if cancel.load(Relaxed) {
        return Err(CANCELLED.into());
    }
    if let Some((mut pj, start)) = pending.take() {
        pj.delay = delay_cs(start, (end_ms + 5) / 10);
        batch.push(pj);
    }
    written += flush(&mut enc, &mut batch, cancel, processed)?;
    if written == 0 {
        return Err("No frames were captured.".into());
    }
    let mut writer = enc.into_inner().map_err(|e| e.to_string())?;
    writer.flush().map_err(|e| e.to_string())?;
    Ok((written, end_ms))
}

fn delay_cs(start: u64, end: u64) -> u16 {
    // Browsers treat delays below 2cs as 10cs, so never go below 2.
    end.saturating_sub(start).clamp(2, u16::MAX as u64) as u16
}

fn full_job(px: &[u32], w: u16, h: u16) -> Job {
    Job { left: 0, top: 0, w, h, px: px.to_vec(), transparent: false, delay: 0, raw: 1 }
}

/// Bounding box of changed pixels, with unchanged ones marked transparent.
/// `None` if the frames are identical.
fn diff_job(prev: &[u32], cur: &[u32], w: usize, h: usize) -> Option<Job> {
    fn row(buf: &[u32], w: usize, y: usize) -> &[u32] {
        &buf[y * w..(y + 1) * w]
    }
    let y0 = (0..h).find(|&y| row(prev, w, y) != row(cur, w, y))?;
    let y1 = (y0..h).rev().find(|&y| row(prev, w, y) != row(cur, w, y)).unwrap_or(y0);

    let mut x0 = w;
    let mut x1: Option<usize> = None;
    for y in y0..=y1 {
        let (p, c) = (row(prev, w, y), row(cur, w, y));
        if let Some(x) = (0..x0).find(|&x| p[x] != c[x]) {
            x0 = x;
        }
        let lo = x1.map_or(0, |v| v + 1);
        if let Some(x) = (lo..w).rev().find(|&x| p[x] != c[x]) {
            x1 = Some(x);
        }
    }
    let x1 = x1.unwrap_or(x0).max(x0);

    let (bw, bh) = (x1 - x0 + 1, y1 - y0 + 1);
    let mut px = Vec::with_capacity(bw * bh);
    let mut transparent = false;
    for y in y0..=y1 {
        let (p, c) = (&row(prev, w, y)[x0..=x1], &row(cur, w, y)[x0..=x1]);
        for (a, b) in p.iter().zip(c) {
            if a == b {
                px.push(0);
                transparent = true;
            } else {
                px.push(*b | 0xFF00_0000);
            }
        }
    }
    Some(Job {
        left: x0 as u16,
        top: y0 as u16,
        w: bw as u16,
        h: bh as u16,
        px,
        transparent,
        delay: 0,
        raw: 1,
    })
}

fn flush<W: Write>(
    enc: &mut gif::Encoder<W>,
    batch: &mut Vec<Job>,
    cancel: &AtomicBool,
    processed: &AtomicUsize,
) -> Result<usize, String> {
    if batch.is_empty() {
        return Ok(0);
    }
    if cancel.load(Relaxed) {
        return Err(CANCELLED.into());
    }
    let jobs = std::mem::take(batch);
    let raw: usize = jobs.iter().map(|j| j.raw).sum();
    let frames: Vec<gif::Frame<'static>> = jobs.into_par_iter().map(quantize).collect();
    for f in &frames {
        enc.write_lzw_pre_encoded_frame(f).map_err(|e| e.to_string())?;
    }
    processed.fetch_add(raw, Relaxed);
    Ok(frames.len())
}

fn rgb(p: u32) -> [u8; 3] {
    [(p >> 16) as u8, (p >> 8) as u8, p as u8]
}

fn quantize(job: Job) -> gif::Frame<'static> {
    let mut idx = Vec::with_capacity(job.px.len());
    let mut last: Option<(u32, u8)> = None;
    let (palette, transparent_index) = match exact_palette(&job.px) {
        Some(map) => {
            let mut pal = vec![0u8; (map.len() + 1) * 3];
            for (&c, &i) in &map {
                pal[i as usize * 3..i as usize * 3 + 3].copy_from_slice(&rgb(c));
            }
            let ti = map.len() as u8;
            for &p in &job.px {
                if p >> 24 == 0 {
                    idx.push(ti);
                    continue;
                }
                let i = match last {
                    Some((lp, li)) if lp == p => li,
                    _ => map[&p],
                };
                last = Some((p, i));
                idx.push(i);
            }
            (pal, ti)
        }
        None => {
            let mut rgba = Vec::with_capacity(job.px.len() * 4);
            for &p in &job.px {
                if p >> 24 != 0 {
                    let [r, g, b] = rgb(p);
                    rgba.extend_from_slice(&[r, g, b, 255]);
                }
            }
            // Large frames don't need every pixel to train a good palette.
            let samplefac = (rgba.len() / 4 / 60_000).clamp(1, 30) as i32;
            let nq = NeuQuant::new(samplefac, 255, &rgba);
            let mut pal = nq.color_map_rgb();
            pal.extend_from_slice(&[0, 0, 0]);
            for &p in &job.px {
                if p >> 24 == 0 {
                    idx.push(255);
                    continue;
                }
                let i = match last {
                    Some((lp, li)) if lp == p => li,
                    _ => {
                        let [r, g, b] = rgb(p);
                        nq.index_of(&[r, g, b, 255]) as u8
                    }
                };
                last = Some((p, i));
                idx.push(i);
            }
            (pal, 255)
        }
    };

    let mut f = gif::Frame::default();
    f.left = job.left;
    f.top = job.top;
    f.width = job.w;
    f.height = job.h;
    f.delay = job.delay;
    f.dispose = gif::DisposalMethod::Keep;
    f.transparent = job.transparent.then_some(transparent_index);
    f.palette = Some(palette);
    f.buffer = Cow::Owned(idx);
    f.make_lzw_pre_encoded();
    f
}

/// Maps each opaque color to a palette index if there are at most 255.
fn exact_palette(px: &[u32]) -> Option<HashMap<u32, u8>> {
    let mut map = HashMap::with_capacity(256);
    let mut last = None;
    for &p in px {
        if p >> 24 == 0 || Some(p) == last {
            continue;
        }
        last = Some(p);
        if !map.contains_key(&p) {
            if map.len() == 255 {
                return None;
            }
            let i = map.len() as u8;
            map.insert(p, i);
        }
    }
    Some(map)
}
