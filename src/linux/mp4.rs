//! The MP4 writer on Linux: OpenH264 (built into the app, since desktops don't
//! reliably ship an H.264 encoder) and a minimal MP4 muxer for its output:
//! `ftyp`, the frames in `mdat`, then the index (`moov`), which the shared
//! faststart moves to the front.

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, RateControlMode, UsageType,
    VuiConfig,
};
use openh264::formats::YUVSource;
use openh264::{OpenH264API, Timestamp};

use crate::mp4_enc::VideoWriter;

/// Sample times are in milliseconds, as the capture side measures them.
const TIMESCALE: u32 = 1000;

/// A borrowed I420 frame (Y, U and V planes, back to back).
struct I420<'a> {
    data: &'a [u8],
    w: usize,
    h: usize,
}

impl YUVSource for I420<'_> {
    fn dimensions(&self) -> (usize, usize) {
        (self.w, self.h)
    }

    fn strides(&self) -> (usize, usize, usize) {
        (self.w, self.w / 2, self.w / 2)
    }

    fn y(&self) -> &[u8] {
        &self.data[..self.w * self.h]
    }

    fn u(&self) -> &[u8] {
        let n = self.w * self.h;
        &self.data[n..n + n / 4]
    }

    fn v(&self) -> &[u8] {
        let n = self.w * self.h;
        &self.data[n + n / 4..n + n / 2]
    }
}

pub struct Writer {
    encoder: Encoder,
    file: BufWriter<File>,
    w: i32,
    h: i32,
    /// Parameter sets, from the first keyframe (they go in `avcC`).
    sps: Vec<u8>,
    pps: Vec<u8>,
    /// Where `mdat` starts and where the next frame goes.
    mdat_at: u64,
    pos: u64,
    /// One chunk per frame: its offset, size and duration; keyframe numbers
    /// (1-based).
    offsets: Vec<u64>,
    sizes: Vec<u32>,
    durations: Vec<u32>,
    keyframes: Vec<u32>,
}

impl VideoWriter for Writer {
    const NV12: bool = false;
    const MIN_SIZE: i32 = 16;
    /// What OpenH264 takes (as the `openh264` crate checks it).
    const MAX_SIZE: (i32, i32) = (3840, 2160);

    fn create(path: &Path, w: i32, h: i32, fps: u32) -> Result<Writer, String> {
        let config = EncoderConfig::new()
            .usage_type(UsageType::ScreenContentRealTime)
            .rate_control_mode(RateControlMode::Quality)
            .bitrate(BitRate::from_bps(bitrate(w, h, fps)))
            .max_frame_rate(FrameRate::from_hz(fps as f32))
            // A quarter faster than the default, for about the same size.
            .complexity(Complexity::Low)
            // Not available for screen content (OpenH264 says so otherwise).
            .adaptive_quantization(false)
            .background_detection(false)
            // Every frame is kept (skipping drops frames of busy content); a
            // static screen costs next to nothing anyway.
            .skip_frames(false)
            // A keyframe every two seconds or so, for seeking.
            .intra_frame_period(IntraFramePeriod::from_num_frames(fps * 2))
            // The colors as converted (BT.709, limited range).
            .vui(VuiConfig::bt709());
        let mut encoder = Encoder::with_api_config(OpenH264API::from_source(), config)
            .map_err(|e| format!("Starting the H.264 encoder failed: {e}"))?;
        // OpenH264 prints notes to stderr while it starts (the crate quiets it
        // only afterwards), e.g. that keeping every frame means the bitrate
        // isn't enforced, which is intended. Quiet it first:
        // ENCODER_OPTION_TRACE_LEVEL (25) = WELS_LOG_QUIET (0); allowed before
        // initialization.
        let mut quiet: u32 = 0;
        unsafe { encoder.raw_api().set_option(25, (&raw mut quiet).cast()) };
        let file = File::create(path).map_err(|e| format!("Cannot create {}: {e}", path.display()))?;
        let mut writer = Writer {
            encoder,
            file: BufWriter::new(file),
            w,
            h,
            sps: Vec::new(),
            pps: Vec::new(),
            mdat_at: 0,
            pos: 0,
            offsets: Vec::new(),
            sizes: Vec::new(),
            durations: Vec::new(),
            keyframes: Vec::new(),
        };
        let ftyp = mp4_box(b"ftyp", &[b"isom".as_slice(), &0x200u32.to_be_bytes(), b"isomiso2avc1mp41"].concat());
        writer.put(&ftyp)?;
        // mdat with a 64-bit size, filled in at the end.
        writer.mdat_at = writer.pos;
        writer.put(&[&1u32.to_be_bytes(), b"mdat".as_slice(), &0u64.to_be_bytes()].concat())?;
        Ok(writer)
    }

    fn write(&mut self, yuv: &[u8], time_ms: u64, duration_ms: u64) -> Result<(), String> {
        let (w, h) = (self.w as usize, self.h as usize);
        if yuv.len() < w * h * 3 / 2 {
            return Err("Bad frame size".into());
        }
        let frame = I420 { data: yuv, w, h };
        let bits = self
            .encoder
            .encode_at(&frame, Timestamp::from_millis(time_ms))
            .map_err(|e| format!("Encoding a frame failed: {e}"))?;
        let key = matches!(bits.frame_type(), FrameType::IDR | FrameType::I);
        // Annex B (start codes) to MP4 (length-prefixed), with the parameter
        // sets kept aside.
        let mut sample = Vec::new();
        for l in 0..bits.num_layers() {
            let Some(layer) = bits.layer(l) else { continue };
            for n in 0..layer.nal_count() {
                let Some(nal) = layer.nal_unit(n).map(strip_start_code) else { continue };
                let Some(&head) = nal.first() else { continue };
                match head & 0x1F {
                    7 if self.sps.is_empty() => self.sps = nal.to_vec(),
                    8 if self.pps.is_empty() => self.pps = nal.to_vec(),
                    7 | 8 | 9 => {} // repeated parameter sets, access unit delimiters
                    _ => {
                        sample.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                        sample.extend_from_slice(nal);
                    }
                }
            }
        }
        let duration = duration_ms.clamp(1, u32::MAX as u64) as u32;
        if sample.is_empty() {
            // A skipped frame: the previous one stays up longer.
            if let Some(d) = self.durations.last_mut() {
                *d = d.saturating_add(duration);
            }
            return Ok(());
        }
        self.offsets.push(self.pos);
        self.put(&sample)?;
        self.sizes.push(sample.len() as u32);
        self.durations.push(duration);
        if key {
            self.keyframes.push(self.sizes.len() as u32);
        }
        Ok(())
    }

    fn finish(mut self) -> Result<(), String> {
        if self.sps.is_empty() || self.pps.is_empty() || self.sizes.is_empty() {
            return Err("The H.264 encoder produced no video".into());
        }
        let moov = self.moov();
        let mdat_size = self.pos - self.mdat_at;
        self.put(&moov)?;
        let io = |e: std::io::Error| format!("Writing the MP4 file failed: {e}");
        self.file.seek(SeekFrom::Start(self.mdat_at + 8)).map_err(io)?;
        self.file.write_all(&mdat_size.to_be_bytes()).map_err(io)?;
        let file = self.file.into_inner().map_err(|e| io(e.into_error()))?;
        file.sync_all().map_err(io)
    }
}

impl Writer {
    fn put(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.file.write_all(bytes).map_err(|e| format!("Writing the MP4 file failed: {e}"))?;
        self.pos += bytes.len() as u64;
        Ok(())
    }

    /// The index: one H.264 video track.
    fn moov(&self) -> Vec<u8> {
        let duration: u64 = self.durations.iter().map(|&d| d as u64).sum();
        let duration = duration.min(u32::MAX as u64) as u32;
        let (w, h) = (self.w as u32, self.h as u32);
        const MATRIX: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];
        let matrix: Vec<u8> = MATRIX.iter().flat_map(|v| v.to_be_bytes()).collect();

        let mut mvhd = Vec::new();
        mvhd.extend_from_slice(&[0u8; 8]); // created, modified
        mvhd.extend_from_slice(&TIMESCALE.to_be_bytes());
        mvhd.extend_from_slice(&duration.to_be_bytes());
        mvhd.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // rate 1.0
        mvhd.extend_from_slice(&0x0100u16.to_be_bytes()); // volume 1.0
        mvhd.extend_from_slice(&[0u8; 10]);
        mvhd.extend_from_slice(&matrix);
        mvhd.extend_from_slice(&[0u8; 24]);
        mvhd.extend_from_slice(&2u32.to_be_bytes()); // next track id

        let mut tkhd = Vec::new();
        tkhd.extend_from_slice(&[0u8; 8]);
        tkhd.extend_from_slice(&1u32.to_be_bytes()); // track id
        tkhd.extend_from_slice(&[0u8; 4]);
        tkhd.extend_from_slice(&duration.to_be_bytes());
        tkhd.extend_from_slice(&[0u8; 16]); // reserved, layer, group, volume, reserved
        tkhd.extend_from_slice(&matrix);
        tkhd.extend_from_slice(&(w << 16).to_be_bytes());
        tkhd.extend_from_slice(&(h << 16).to_be_bytes());

        let mut mdhd = Vec::new();
        mdhd.extend_from_slice(&[0u8; 8]);
        mdhd.extend_from_slice(&TIMESCALE.to_be_bytes());
        mdhd.extend_from_slice(&duration.to_be_bytes());
        mdhd.extend_from_slice(&0x55C4u16.to_be_bytes()); // language "und"
        mdhd.extend_from_slice(&[0u8; 2]);

        let mut hdlr = vec![0u8; 4];
        hdlr.extend_from_slice(b"vide");
        hdlr.extend_from_slice(&[0u8; 12]);
        hdlr.extend_from_slice(b"VideoHandler\0");

        let dref = full_box(b"dref", 0, 0, &[&1u32.to_be_bytes(), full_box(b"url ", 0, 1, &[]).as_slice()].concat());

        let stbl = [
            full_box(b"stsd", 0, 0, &[&1u32.to_be_bytes(), self.avc1().as_slice()].concat()),
            full_box(b"stts", 0, 0, &stts(&self.durations)),
            full_box(b"stss", 0, 0, &table(&self.keyframes)),
            // One entry: from chunk 1 on, one frame per chunk, description 1.
            full_box(b"stsc", 0, 0, &be(&[1, 1, 1, 1])),
            full_box(b"stsz", 0, 0, &[&0u32.to_be_bytes(), table(&self.sizes).as_slice()].concat()),
            self.chunk_offsets(),
        ]
        .concat();

        let minf = [
            full_box(b"vmhd", 0, 1, &[0u8; 8]),
            mp4_box(b"dinf", &dref),
            mp4_box(b"stbl", &stbl),
        ]
        .concat();
        let mdia = [full_box(b"mdhd", 0, 0, &mdhd), full_box(b"hdlr", 0, 0, &hdlr), mp4_box(b"minf", &minf)].concat();
        let trak = [full_box(b"tkhd", 0, 3, &tkhd), mp4_box(b"mdia", &mdia)].concat();
        mp4_box(b"moov", &[full_box(b"mvhd", 0, 0, &mvhd), mp4_box(b"trak", &trak)].concat())
    }

    /// The sample description: H.264 with its parameter sets and colors.
    fn avc1(&self) -> Vec<u8> {
        let (sps, pps) = (&self.sps, &self.pps);
        let profile = sps.get(1).copied().unwrap_or(66);
        let mut avcc = vec![1, profile, sps.get(2).copied().unwrap_or(0), sps.get(3).copied().unwrap_or(30), 0xFF, 0xE1];
        avcc.extend_from_slice(&(sps.len() as u16).to_be_bytes());
        avcc.extend_from_slice(sps);
        avcc.push(1);
        avcc.extend_from_slice(&(pps.len() as u16).to_be_bytes());
        avcc.extend_from_slice(pps);
        if matches!(profile, 100 | 110 | 122 | 144) {
            // 4:2:0, 8 bits, no SPS extensions.
            avcc.extend_from_slice(&[0xFD, 0xF8, 0xF8, 0]);
        }
        // BT.709 primaries, transfer and matrix; limited range.
        let colr = mp4_box(b"colr", &[b"nclx".as_slice(), &[0, 1, 0, 1, 0, 1, 0]].concat());

        let mut entry = vec![0u8; 6];
        entry.extend_from_slice(&1u16.to_be_bytes()); // data reference
        entry.extend_from_slice(&[0u8; 16]);
        entry.extend_from_slice(&(self.w as u16).to_be_bytes());
        entry.extend_from_slice(&(self.h as u16).to_be_bytes());
        entry.extend_from_slice(&0x0048_0000u32.to_be_bytes()); // 72 dpi
        entry.extend_from_slice(&0x0048_0000u32.to_be_bytes());
        entry.extend_from_slice(&[0u8; 4]);
        entry.extend_from_slice(&1u16.to_be_bytes()); // frames per sample
        entry.extend_from_slice(&[0u8; 32]); // compressor name
        entry.extend_from_slice(&0x0018u16.to_be_bytes()); // depth
        entry.extend_from_slice(&(-1i16).to_be_bytes());
        entry.extend_from_slice(&mp4_box(b"avcC", &avcc));
        entry.extend_from_slice(&colr);
        mp4_box(b"avc1", &entry)
    }

    /// `stco`, or `co64` past 4 GB.
    fn chunk_offsets(&self) -> Vec<u8> {
        if self.pos <= u32::MAX as u64 {
            let offsets: Vec<u32> = self.offsets.iter().map(|&o| o as u32).collect();
            full_box(b"stco", 0, 0, &table(&offsets))
        } else {
            let wide: Vec<u8> = self.offsets.iter().flat_map(|o| o.to_be_bytes()).collect();
            full_box(b"co64", 0, 0, &[&(self.offsets.len() as u32).to_be_bytes(), wide.as_slice()].concat())
        }
    }
}

/// A NAL unit without its Annex B start code.
fn strip_start_code(nal: &[u8]) -> &[u8] {
    if nal.starts_with(&[0, 0, 0, 1]) {
        &nal[4..]
    } else if nal.starts_with(&[0, 0, 1]) {
        &nal[3..]
    } else {
        nal
    }
}

fn mp4_box(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(8 + body.len());
    b.extend_from_slice(&(8 + body.len() as u32).to_be_bytes());
    b.extend_from_slice(kind);
    b.extend_from_slice(body);
    b
}

/// A box with a version and flags.
fn full_box(kind: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
    let head = (version as u32) << 24 | (flags & 0xFF_FFFF);
    mp4_box(kind, &[&head.to_be_bytes(), body].concat())
}

fn be(values: &[u32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_be_bytes()).collect()
}

/// An entry count followed by the entries.
fn table(entries: &[u32]) -> Vec<u8> {
    [&(entries.len() as u32).to_be_bytes(), be(entries).as_slice()].concat()
}

/// Durations as runs of (count, duration).
fn stts(durations: &[u32]) -> Vec<u8> {
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for &d in durations {
        match runs.last_mut() {
            Some((n, last)) if *last == d => *n += 1,
            _ => runs.push((1, d)),
        }
    }
    let flat: Vec<u32> = runs.iter().flat_map(|&(n, d)| [n, d]).collect();
    [&(runs.len() as u32).to_be_bytes(), be(&flat).as_slice()].concat()
}

/// The bitrate OpenH264 aims for in its constant-quality mode: enough for
/// sharp text in motion.
fn bitrate(w: i32, h: i32, fps: u32) -> u32 {
    let bits = w as f64 * h as f64 * fps as f64 * 0.15;
    bits.clamp(500_000.0, 40_000_000.0) as u32
}
