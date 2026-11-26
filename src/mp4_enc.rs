//! MP4 (H.264) encoding of a recording, with the same frames, annotations,
//! progress and cancel as the GIF encoder.
//!
//! This part is shared: it merges identical frames (a frame's duration lasts
//! until the next change), fits the size to what H.264 encoders take,
//! converts to YUV, and finally moves the file's index to the front. The
//! platform supplies a [`VideoWriter`]: Media Foundation on Windows, OpenH264
//! and a small MP4 writer on Linux.

use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::sync::mpsc::Receiver;

use rayon::prelude::*;

#[cfg(target_os = "linux")]
use crate::linux::mp4::Writer;
use crate::recording::{Format, Msg, RecInfo, CANCELLED};
use crate::util;
#[cfg(windows)]
use crate::win::mp4::Writer;

/// An H.264 encoder writing an MP4 file.
pub trait VideoWriter: Sized {
    /// Frames are NV12 (Y plane, then interleaved U/V); otherwise I420 (Y,
    /// U and V planes).
    const NV12: bool;
    /// The smallest width and height the encoder takes (even); smaller
    /// areas are enlarged.
    const MIN_SIZE: i32;
    /// The largest frame the encoder takes; bigger areas are scaled down.
    const MAX_SIZE: (i32, i32);

    /// Starts a file of `w` × `h` (both even) frames, at about `fps`.
    fn create(path: &Path, w: i32, h: i32, fps: u32) -> Result<Self, String>;
    /// Adds a frame shown from `time_ms` for `duration_ms`.
    fn write(&mut self, yuv: &[u8], time_ms: u64, duration_ms: u64) -> Result<(), String>;
    /// Completes the file (its index may come last; [`faststart`] moves it).
    fn finish(self) -> Result<(), String>;
}

/// Encodes frames from `rx` until `Msg::End` (or the sender hangs up).
/// Called through [`crate::recording::encode`].
#[allow(clippy::too_many_arguments)]
pub fn encode(
    rx: Receiver<Msg>,
    w: i32,
    h: i32,
    fps: u32,
    mut overlay: impl FnMut(&mut [u32]),
    out: &Path,
    cancel: &AtomicBool,
    processed: &AtomicUsize,
) -> Result<RecInfo, String> {
    let part = out.with_extension("mp4.part");
    let (ow, oh) = stored_size(w, h, Writer::MIN_SIZE, Writer::MAX_SIZE);
    let result = encode_to::<Writer>(&rx, (w, h), (ow, oh), fps, &mut overlay, &part, cancel, processed).and_then(|r| {
        if let Err(e) = faststart(&part, out) {
            let _ = fs::remove_file(out); // half copied
            return Err(format!("Cannot save {}: {e}", out.display()));
        }
        let _ = fs::remove_file(&part);
        Ok(r)
    });
    match result {
        Ok((frames, duration_ms)) => Ok(RecInfo {
            path: out.to_path_buf(),
            format: Format::Mp4,
            bytes: fs::metadata(out).map(|m| m.len()).unwrap_or(0),
            frames,
            duration_ms,
            w: ow,
            h: oh,
        }),
        Err(e) => {
            let _ = fs::remove_file(&part);
            Err(e)
        }
    }
}

/// A top-level MP4 box: its type, where it starts and its total size.
struct BoxPos {
    kind: [u8; 4],
    at: u64,
    size: u64,
}

fn top_boxes(f: &mut File) -> io::Result<Vec<BoxPos>> {
    let len = f.metadata()?.len();
    let mut boxes = Vec::new();
    let mut at = 0;
    while at + 8 <= len {
        f.seek(SeekFrom::Start(at))?;
        let mut head = [0u8; 16];
        f.read_exact(&mut head[..8])?;
        let mut size = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as u64;
        if size == 1 {
            f.read_exact(&mut head[8..])?;
            size = u64::from_be_bytes([head[8], head[9], head[10], head[11], head[12], head[13], head[14], head[15]]);
        } else if size == 0 {
            size = len - at;
        }
        if size < 8 || at + size > len {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "damaged MP4 box"));
        }
        boxes.push(BoxPos { kind: [head[4], head[5], head[6], head[7]], at, size });
        at += size;
    }
    Ok(boxes)
}

fn read_u32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// Adds `shift` to every chunk offset (`stco`/`co64`) in the boxes of `data`
/// (a `moov` payload), going into the boxes that lead to them.
fn shift_offsets(data: &mut [u8], shift: u64) -> Option<()> {
    let mut at = 0;
    while at + 8 <= data.len() {
        let size = read_u32(data, at)? as usize;
        if size < 8 || at + size > data.len() {
            return None;
        }
        let kind: [u8; 4] = data[at + 4..at + 8].try_into().ok()?;
        let body = &mut data[at + 8..at + size];
        match &kind {
            b"trak" | b"mdia" | b"minf" | b"stbl" => shift_offsets(body, shift)?,
            b"stco" | b"co64" => {
                let step = if &kind == b"co64" { 8 } else { 4 };
                let n = read_u32(body, 4)? as usize;
                for e in body.get_mut(8..8 + n * step)?.chunks_exact_mut(step) {
                    if step == 8 {
                        let v = u64::from_be_bytes(e.try_into().ok()?) + shift;
                        e.copy_from_slice(&v.to_be_bytes());
                    } else {
                        // Past 4 GB it would need co64; leave the file as it is.
                        let v = u32::try_from(u32::from_be_bytes(e.try_into().ok()?) as u64 + shift).ok()?;
                        e.copy_from_slice(&v.to_be_bytes());
                    }
                }
            }
            _ => {}
        }
        at += size;
    }
    Some(())
}

/// Copies the finished `src` to `dst` with its index (`moov`) in front of the
/// media data, so players and websites can start playing before the whole
/// file has arrived. A file that has it there already (or that can't be
/// rearranged) is copied as it is.
pub fn faststart(src: &Path, dst: &Path) -> io::Result<()> {
    let mut f = File::open(src)?;
    let boxes = top_boxes(&mut f)?;
    let mdat = boxes.iter().position(|b| &b.kind == b"mdat");
    let moov = boxes.iter().position(|b| &b.kind == b"moov");
    let (Some(mdat), Some(moov)) = (mdat, moov) else {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "incomplete MP4 file"));
    };
    let mut index = Vec::new();
    if moov > mdat {
        let m = &boxes[moov];
        f.seek(SeekFrom::Start(m.at))?;
        index = vec![0u8; m.size as usize];
        f.read_exact(&mut index)?;
        // Everything from the first mdat on moves along by the index's size.
        let plain_header = read_u32(&index, 0) == Some(m.size as u32);
        if !plain_header || shift_offsets(&mut index[8..], m.size).is_none() {
            index.clear();
        }
    }
    let mut out = BufWriter::new(File::create(dst)?);
    for (i, b) in boxes.iter().enumerate() {
        if !index.is_empty() {
            if i == mdat {
                out.write_all(&index)?;
            }
            if i == moov {
                continue;
            }
        }
        f.seek(SeekFrom::Start(b.at))?;
        io::copy(&mut (&mut f).take(b.size), &mut out)?;
    }
    out.into_inner().map_err(|e| e.into_error())?.sync_all()
}

/// The size frames are stored at: within the encoder's limits (`max` is the
/// landscape size, turned for portrait areas), keeping the aspect ratio, and
/// even, as 4:2:0 video needs. Tiny areas are enlarged by a whole factor
/// (pixels stay crisp), huge ones scaled down. (Only a sliver too thin for
/// both limits gets stretched to the minimum.)
pub fn stored_size(w: i32, h: i32, min: i32, max: (i32, i32)) -> (i32, i32) {
    let (w, h) = (w.max(1), h.max(1));
    let max = if h > w { (max.1, max.0) } else { max };
    let k = (min + w.min(h) - 1) / w.min(h);
    let (w, h) = util::fit(w * k, h * k, max.0, max.1);
    ((w & !1).max(min), (h & !1).max(min))
}

/// A frame waiting for the next change, which ends it.
struct Pending {
    px: Vec<u32>,
    time: u64,
    /// Captured frames it stands for (for progress).
    raw: usize,
}

#[allow(clippy::too_many_arguments)]
fn encode_to<W: VideoWriter>(
    rx: &Receiver<Msg>,
    (w, h): (i32, i32),
    (ow, oh): (i32, i32),
    fps: u32,
    overlay: &mut impl FnMut(&mut [u32]),
    part: &Path,
    cancel: &AtomicBool,
    processed: &AtomicUsize,
) -> Result<(usize, u64), String> {
    if w <= 0 || h <= 0 {
        return Err(format!("Unsupported video size {w}x{h}"));
    }
    if let Some(dir) = part.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let fps = fps.clamp(1, 60);
    let mut writer = W::create(part, ow, oh, fps)?;
    let mut pending: Option<Pending> = None;
    let mut start: Option<u64> = None;
    let mut written = 0usize;
    let mut end = None;

    let mut emit = |writer: &mut W, p: Pending, until: u64, start: u64| -> Result<(), String> {
        let yuv = to_yuv(&p.px, w, h, ow, oh, W::NV12);
        writer.write(&yuv, p.time - start, until.saturating_sub(p.time).max(1))?;
        processed.fetch_add(p.raw, Relaxed);
        written += 1;
        Ok(())
    };

    for msg in rx.iter() {
        if cancel.load(Relaxed) {
            return Err(CANCELLED.into());
        }
        match msg {
            Msg::Frame(mut px, t) => {
                if px.len() != w as usize * h as usize {
                    continue;
                }
                overlay(&mut px);
                let t0 = *start.get_or_insert(t);
                let t = t.max(t0);
                match pending.as_mut() {
                    Some(p) if p.px == px => p.raw += 1,
                    _ => {
                        if let Some(p) = pending.take() {
                            emit(&mut writer, p, t, t0)?;
                        }
                        pending = Some(Pending { px, time: t, raw: 1 });
                    }
                }
            }
            Msg::End(t) => {
                end = Some(t);
                break;
            }
        }
    }
    if cancel.load(Relaxed) {
        return Err(CANCELLED.into());
    }
    let t0 = start.unwrap_or(0);
    if let Some(p) = pending.take() {
        // The last frame lasts until the recording stopped (or one frame
        // interval, if the capture side hung up without saying when).
        let until = end.filter(|&e| e > p.time).unwrap_or(p.time + 1000 / fps as u64);
        emit(&mut writer, p, until, t0)?;
        end = Some(until);
    }
    if written == 0 {
        return Err("No frames were captured.".into());
    }
    writer.finish()?;
    Ok((written, end.unwrap_or(0).saturating_sub(t0)))
}

// BT.709 limited range ("TV" levels, what players assume for HD video), in
// 16.16 fixed point. Each chroma row sums to zero so grays stay neutral.
const Y_R: i32 = 11966;
const Y_G: i32 = 40254;
const Y_B: i32 = 4064;
const U_R: i32 = -6596;
const U_G: i32 = -22189;
const U_B: i32 = 28785;
const V_R: i32 = 28784;
const V_G: i32 = -26145;
const V_B: i32 = -2639;

fn rgb(p: u32) -> (i32, i32, i32) {
    (((p >> 16) & 0xFF) as i32, ((p >> 8) & 0xFF) as i32, (p & 0xFF) as i32)
}

fn luma(p: u32) -> u8 {
    let (r, g, b) = rgb(p);
    (((Y_R * r + Y_G * g + Y_B * b + 0x8000) >> 16) + 16) as u8
}

/// Chroma of a 2×2 block (its four pixels).
fn chroma(q: [u32; 4]) -> (u8, u8) {
    let (mut r, mut g, mut b) = (0, 0, 0);
    for p in q {
        let c = rgb(p);
        r += c.0;
        g += c.1;
        b += c.2;
    }
    // Sums of four pixels: shift by two more bits.
    let u = ((U_R * r + U_G * g + U_B * b + 0x2_0000) >> 18) + 128;
    let v = ((V_R * r + V_G * g + V_B * b + 0x2_0000) >> 18) + 128;
    (u.clamp(0, 255) as u8, v.clamp(0, 255) as u8)
}

/// Converts source rows `2j` and `2j + 1` (of width `sw`, `ow` used) into
/// two rows of luma in `yrows` and a row of chroma, stored by `put(i, u, v)`
/// for block `i`.
fn row_pair(src: &[u32], sw: usize, ow: usize, j: usize, yrows: &mut [u8], mut put: impl FnMut(usize, u8, u8)) {
    let (r0, r1) = (&src[2 * j * sw..][..ow], &src[(2 * j + 1) * sw..][..ow]);
    let (y0, y1) = yrows.split_at_mut(ow);
    for x in 0..ow {
        y0[x] = luma(r0[x]);
        y1[x] = luma(r1[x]);
    }
    for i in 0..ow / 2 {
        let (u, v) = chroma([r0[2 * i], r0[2 * i + 1], r1[2 * i], r1[2 * i + 1]]);
        put(i, u, v);
    }
}

/// Converts BGRA words (`w` × `h`) to 4:2:0 YUV of `ow` × `oh` (even): the
/// top-left part if that's all that differs (dropping an odd row or
/// column), scaled otherwise.
pub fn to_yuv(px: &[u32], w: i32, h: i32, ow: i32, oh: i32, nv12: bool) -> Vec<u8> {
    let scaled: Vec<u32>;
    let (src, sw) = if (ow, oh) != (w & !1, h & !1) {
        // Back to words by value: a byte buffer isn't guaranteed to be aligned
        // for casting.
        let bytes = util::resize(bytemuck::cast_slice(px), w, h, ow, oh);
        scaled = bytes.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
        (scaled.as_slice(), ow as usize)
    } else {
        (px, w as usize)
    };
    let (ow, oh) = (ow as usize, oh as usize);
    let mut out = vec![0u8; ow * oh * 3 / 2];
    let (ys, cs) = out.split_at_mut(ow * oh);
    if nv12 {
        ys.par_chunks_mut(2 * ow).zip(cs.par_chunks_mut(ow)).enumerate().for_each(|(j, (yrows, uv))| {
            row_pair(src, sw, ow, j, yrows, |i, u, v| {
                uv[2 * i] = u;
                uv[2 * i + 1] = v;
            })
        });
    } else {
        let (us, vs) = cs.split_at_mut(ow / 2 * (oh / 2));
        ys.par_chunks_mut(2 * ow).zip(us.par_chunks_mut(ow / 2)).zip(vs.par_chunks_mut(ow / 2)).enumerate().for_each(
            |(j, ((yrows, u_row), v_row))| {
                row_pair(src, sw, ow, j, yrows, |i, u, v| {
                    u_row[i] = u;
                    v_row[i] = v;
                })
            },
        );
    }
    out
}
