//! What a screen recording is, whatever it's saved as: the frames the capture
//! side sends, the formats it can be saved in, and the encoder for each. Both
//! formats get the same frames, annotations, progress and cancel; only the
//! file differs.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::mpsc::Receiver;

use crate::{gif_enc, mp4_enc};

/// The error an encoder returns when the recording was discarded.
pub const CANCELLED: &str = "cancelled";

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Format {
    #[default]
    Gif,
    Mp4,
}

impl Format {
    pub const ALL: [Format; 2] = [Format::Gif, Format::Mp4];

    /// For menus and messages.
    pub fn label(self) -> &'static str {
        match self {
            Format::Gif => "GIF",
            Format::Mp4 => "MP4",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Format::Gif => "gif",
            Format::Mp4 => "mp4",
        }
    }

    /// The settings value.
    pub fn key(self) -> &'static str {
        self.extension()
    }

    pub fn from_key(s: &str) -> Option<Format> {
        Format::ALL.into_iter().find(|f| f.key().eq_ignore_ascii_case(s.trim()))
    }

    pub fn other(self) -> Format {
        match self {
            Format::Gif => Format::Mp4,
            Format::Mp4 => Format::Gif,
        }
    }
}

pub enum Msg {
    /// Pixels as little-endian BGRA words with alpha 255, and a timestamp in ms.
    Frame(Vec<u32>, u64),
    /// Recording stopped at this timestamp (ms).
    End(u64),
}

/// A finished recording.
#[derive(Debug)]
pub struct RecInfo {
    pub path: PathBuf,
    pub format: Format,
    pub bytes: u64,
    /// Frames stored (identical frames are merged).
    pub frames: usize,
    pub duration_ms: u64,
    /// The stored size (MP4 needs even dimensions, so it can be a pixel
    /// smaller than the area).
    pub w: i32,
    pub h: i32,
}

/// Encodes frames from `rx` until `Msg::End` (or the sender hangs up) into
/// `out`, by way of a `.part` file that's removed on failure or cancel.
/// `overlay` is applied to every frame first (annotations); `processed`
/// counts captured frames handled, for the progress bar.
#[allow(clippy::too_many_arguments)]
pub fn encode(
    format: Format,
    rx: Receiver<Msg>,
    w: i32,
    h: i32,
    fps: u32,
    overlay: impl FnMut(&mut [u32]),
    out: &Path,
    cancel: &AtomicBool,
    processed: &AtomicUsize,
) -> Result<RecInfo, String> {
    match format {
        Format::Gif => gif_enc::encode(rx, w, h, overlay, out, cancel, processed),
        Format::Mp4 => mp4_enc::encode(rx, w, h, fps, overlay, out, cancel, processed),
    }
}

/// A recording's file name in the captures folder.
pub fn file_name(format: Format, timestamp: &str) -> String {
    format!("Recording_{timestamp}.{}", format.extension())
}
