//! Shared helpers: an integer rectangle, PNG encoding, folders and
//! timestamps; plus Win32 string, message box and DPI helpers on Windows.

use std::path::PathBuf;

pub const APP_NAME: &str = "Screen Capture";
/// For the About menu (from Cargo.toml).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const MAKER: &str = "Pixobit Solutions";
pub const HOMEPAGE: &str = env!("CARGO_PKG_HOMEPAGE");
pub const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");

/// Where recorded GIFs (and quick saves) go.
pub fn captures_dir() -> PathBuf {
    pictures_dir().join(APP_NAME)
}

pub fn human_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{} KB", bytes.div_ceil(1024))
    }
}

/// Encodes opaque BGRA pixels as an RGB PNG.
pub fn encode_png(w: i32, h: i32, bgra: &[u8]) -> Result<Vec<u8>, String> {
    let mut rgb = Vec::with_capacity(w as usize * h as usize * 3);
    for c in bgra.chunks_exact(4) {
        rgb.extend_from_slice(&[c[2], c[1], c[0]]);
    }
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, w as u32, h as u32);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Fast);
        let mut wr = enc.write_header().map_err(|e| e.to_string())?;
        wr.write_image_data(&rgb).map_err(|e| e.to_string())?;
    }
    Ok(out)
}

/// Decodes a PNG into opaque BGRA `(w, h, pixels)`.
pub fn decode_png(data: &[u8]) -> Result<(i32, i32, Vec<u8>), String> {
    let mut dec = png::Decoder::new(data);
    dec.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = dec.read_info().map_err(|e| e.to_string())?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
    let channels = match info.color_type {
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Indexed => return Err("unexpected palette PNG".into()),
    };
    let px = buf[..info.buffer_size()]
        .chunks_exact(channels)
        .flat_map(|p| if channels < 3 { [p[0], p[0], p[0], 255] } else { [p[2], p[1], p[0], 255] })
        .collect();
    Ok((info.width as i32, info.height as i32, px))
}

/// Resizes BGRA pixels to `dw` × `dh`: averages the covered pixels when
/// shrinking, repeats them (crisp, no smoothing) when enlarging.
pub fn resize(src: &[u8], sw: i32, sh: i32, dw: i32, dh: i32) -> Vec<u8> {
    let (sw, sh, dw, dh) = (sw.max(1) as usize, sh.max(1) as usize, dw.max(1) as usize, dh.max(1) as usize);
    let mut out = vec![255u8; dw * dh * 4];
    for y in 0..dh {
        let (y0, y1) = (y * sh / dh, ((y + 1) * sh).div_ceil(dh).max(y * sh / dh + 1).min(sh));
        for x in 0..dw {
            let (x0, x1) = (x * sw / dw, ((x + 1) * sw).div_ceil(dw).max(x * sw / dw + 1).min(sw));
            let mut sum = [0u32; 3];
            for sy in y0..y1 {
                for sx in x0..x1 {
                    let i = (sy * sw + sx) * 4;
                    sum[0] += src[i] as u32;
                    sum[1] += src[i + 1] as u32;
                    sum[2] += src[i + 2] as u32;
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as u32;
            let o = (y * dw + x) * 4;
            out[o..o + 3].copy_from_slice(&[(sum[0] / n) as u8, (sum[1] / n) as u8, (sum[2] / n) as u8]);
        }
    }
    out
}

/// The largest size with the same aspect ratio as `w` × `h` that fits in
/// `max_w` × `max_h` (never enlarged, at least 1 × 1).
pub fn fit(w: i32, h: i32, max_w: i32, max_h: i32) -> (i32, i32) {
    let s = (max_w as f32 / w.max(1) as f32).min(max_h as f32 / h.max(1) as f32).min(1.0);
    (((w as f32 * s).round() as i32).max(1), ((h as f32 * s).round() as i32).max(1))
}

/// `HH:MM:SS` in local time.
pub fn clock() -> String {
    // timestamp() is `YYYY-MM-DD_HH-MM-SS`.
    timestamp()[11..].replace('-', ":")
}

/// Integer rectangle with exclusive right/bottom edges.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub l: i32,
    pub t: i32,
    pub r: i32,
    pub b: i32,
}

impl Rect {
    pub const fn new(l: i32, t: i32, r: i32, b: i32) -> Rect {
        Rect { l, t, r, b }
    }

    pub const fn xywh(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect { l: x, t: y, r: x + w, b: y + h }
    }

    /// Normalized rectangle spanning two corner points.
    pub fn from_points(ax: i32, ay: i32, bx: i32, by: i32) -> Rect {
        Rect::new(ax.min(bx), ay.min(by), ax.max(bx), ay.max(by))
    }

    pub fn w(&self) -> i32 {
        self.r - self.l
    }

    pub fn h(&self) -> i32 {
        self.b - self.t
    }

    pub fn is_empty(&self) -> bool {
        self.r <= self.l || self.b <= self.t
    }

    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.l && x < self.r && y >= self.t && y < self.b
    }

    pub fn intersect(&self, o: &Rect) -> Rect {
        Rect::new(self.l.max(o.l), self.t.max(o.t), self.r.min(o.r), self.b.min(o.b))
    }

    pub fn union(&self, o: &Rect) -> Rect {
        if self.is_empty() {
            return *o;
        }
        if o.is_empty() {
            return *self;
        }
        Rect::new(self.l.min(o.l), self.t.min(o.t), self.r.max(o.r), self.b.max(o.b))
    }

    pub fn inflate(&self, d: i32) -> Rect {
        Rect::new(self.l - d, self.t - d, self.r + d, self.b + d)
    }

    pub fn offset(&self, dx: i32, dy: i32) -> Rect {
        Rect::new(self.l + dx, self.t + dy, self.r + dx, self.b + dy)
    }

    pub fn center(&self) -> (i32, i32) {
        ((self.l + self.r) / 2, (self.t + self.b) / 2)
    }

    pub fn area(&self) -> i64 {
        if self.is_empty() {
            0
        } else {
            self.w() as i64 * self.h() as i64
        }
    }
}

// ----- Linux ---------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

/// `YYYY-MM-DD_HH-MM-SS` in local time, for file names.
#[cfg(target_os = "linux")]
pub fn timestamp() -> String {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let now = unsafe { libc::time(std::ptr::null_mut()) };
    unsafe { libc::localtime_r(&now, &mut tm) };
    format!(
        "{:04}-{:02}-{:02}_{:02}-{:02}-{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

/// The XDG pictures folder (from `user-dirs.dirs`), or `~/Pictures`.
#[cfg(target_os = "linux")]
pub fn pictures_dir() -> PathBuf {
    let config = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".config"));
    if let Ok(text) = std::fs::read_to_string(config.join("user-dirs.dirs")) {
        for line in text.lines() {
            if let Some(v) = line.trim().strip_prefix("XDG_PICTURES_DIR=") {
                let v = v.trim().trim_matches('"');
                let path = match v.strip_prefix("$HOME") {
                    Some(rest) => home().join(rest.trim_start_matches('/')),
                    None => PathBuf::from(v),
                };
                if path != home() {
                    return path;
                }
            }
        }
    }
    home().join("Pictures")
}

#[cfg(target_os = "linux")]
pub fn app_data_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"))
        .join("screen-capture")
}

// ----- Windows -------------------------------------------------------------------

#[cfg(windows)]
pub use win_helpers::*;

#[cfg(windows)]
mod win_helpers {
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::path::{Path, PathBuf};
    use std::ptr::null_mut;

    use windows_sys::Win32::Foundation::{HWND, POINT, RECT, SYSTEMTIME};
    use windows_sys::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST};
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;
    use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
    use windows_sys::Win32::UI::Shell::{FOLDERID_Pictures, SHGetKnownFolderPath};
    use windows_sys::Win32::UI::WindowsAndMessaging::MessageBoxW;

    use super::{Rect, APP_NAME};

    impl Rect {
        pub fn from_win(r: &RECT) -> Rect {
            Rect::new(r.left, r.top, r.right, r.bottom)
        }

        pub fn to_win(self) -> RECT {
            RECT { left: self.l, top: self.t, right: self.r, bottom: self.b }
        }
    }

    pub fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn wide_path(p: &Path) -> Vec<u16> {
        p.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
    }

    /// Converts a NUL-terminated UTF-16 buffer into a path.
    pub fn path_from_wide(buf: &[u16]) -> PathBuf {
        let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        PathBuf::from(OsString::from_wide(&buf[..n]))
    }

    /// Copies `s` into a fixed-size UTF-16 field, truncating and NUL-terminating.
    pub fn copy_wstr(dst: &mut [u16], s: &str) {
        let mut n = 0;
        for c in s.encode_utf16().take(dst.len().saturating_sub(1)) {
            dst[n] = c;
            n += 1;
        }
        dst[n] = 0;
    }

    pub fn message_box(hwnd: HWND, text: &str, flags: u32) {
        let t = wide(text);
        let c = wide(APP_NAME);
        unsafe {
            MessageBoxW(hwnd, t.as_ptr(), c.as_ptr(), flags);
        }
    }

    /// `YYYY-MM-DD_HH-MM-SS` in local time, for file names.
    pub fn timestamp() -> String {
        let mut st: SYSTEMTIME = unsafe { std::mem::zeroed() };
        unsafe { GetLocalTime(&mut st) };
        format!(
            "{:04}-{:02}-{:02}_{:02}-{:02}-{:02}",
            st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
        )
    }

    pub fn pictures_dir() -> PathBuf {
        unsafe {
            let mut p = null_mut();
            if SHGetKnownFolderPath(&FOLDERID_Pictures, 0, null_mut(), &mut p) == 0 && !p.is_null() {
                let mut n = 0;
                while *p.add(n) != 0 {
                    n += 1;
                }
                let path = PathBuf::from(OsString::from_wide(std::slice::from_raw_parts(p, n)));
                CoTaskMemFree(p as *const _);
                return path;
            }
            if !p.is_null() {
                CoTaskMemFree(p as *const _);
            }
        }
        std::env::var_os("USERPROFILE")
            .map(|h| PathBuf::from(h).join("Pictures"))
            .unwrap_or_else(|| PathBuf::from("."))
    }

    pub fn app_data_dir() -> PathBuf {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join("screen-capture")
    }

    /// UI scale factor (1.0 = 96 DPI) of the monitor nearest to a screen point.
    pub fn dpi_scale_at(x: i32, y: i32) -> f32 {
        unsafe {
            let mon = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
            let (mut dx, mut dy) = (96u32, 96u32);
            if GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy) != 0 || dx == 0 {
                return 1.0;
            }
            dx as f32 / 96.0
        }
    }

    /// Full rectangle of the monitor nearest to a screen point.
    pub fn monitor_rect_at(x: i32, y: i32) -> Rect {
        unsafe {
            let mon = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
            let mut mi: MONITORINFO = std::mem::zeroed();
            mi.cbSize = size_of::<MONITORINFO>() as u32;
            if GetMonitorInfoW(mon, &mut mi) == 0 {
                return Rect::xywh(0, 0, 1920, 1080);
            }
            Rect::from_win(&mi.rcMonitor)
        }
    }
}
