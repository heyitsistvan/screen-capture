//! Screen capture through GDI, plus PNG encoding of captured pixels.

use std::ffi::c_void;
use std::ptr::null_mut;

use windows_sys::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GdiFlush, GetDC, ReleaseDC, SelectObject,
    BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CAPTUREBLT, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ, SRCCOPY,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DrawIconEx, GetCursorInfo, GetIconInfo, GetSystemMetrics, CURSORINFO, CURSOR_SHOWING, DI_NORMAL, ICONINFO,
    SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

use crate::util::Rect;

/// A top-down 32-bit DIB section selected into its own memory DC.
pub struct Dib {
    pub dc: HDC,
    bmp: HBITMAP,
    old: HGDIOBJ,
    pub bits: *mut u8,
    pub w: i32,
    pub h: i32,
}

impl Dib {
    pub fn new(w: i32, h: i32) -> Option<Dib> {
        if w <= 0 || h <= 0 {
            return None;
        }
        unsafe {
            let screen = GetDC(null_mut());
            let dc = CreateCompatibleDC(screen);
            ReleaseDC(null_mut(), screen);
            if dc.is_null() {
                return None;
            }
            let mut bi: BITMAPINFO = std::mem::zeroed();
            bi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
            bi.bmiHeader.biWidth = w;
            bi.bmiHeader.biHeight = -h;
            bi.bmiHeader.biPlanes = 1;
            bi.bmiHeader.biBitCount = 32;
            bi.bmiHeader.biCompression = BI_RGB;
            let mut bits: *mut c_void = null_mut();
            let bmp = CreateDIBSection(dc, &bi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
            if bmp.is_null() || bits.is_null() {
                DeleteDC(dc);
                return None;
            }
            let old = SelectObject(dc, bmp);
            Some(Dib { dc, bmp, old, bits: bits as *mut u8, w, h })
        }
    }

    pub fn len(&self) -> usize {
        self.w as usize * self.h as usize * 4
    }

    pub fn bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.bits, self.len()) }
    }

    pub fn bytes_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.bits, self.len()) }
    }
}

impl Drop for Dib {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            DeleteObject(self.bmp);
            DeleteDC(self.dc);
        }
    }
}

/// Bounding box of all monitors, in physical screen coordinates.
pub fn virtual_screen() -> Rect {
    unsafe {
        Rect::xywh(
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    }
}

/// Captures a screen rectangle as opaque BGRA.
pub fn capture(r: Rect) -> Option<Vec<u8>> {
    let dib = Dib::new(r.w(), r.h())?;
    unsafe {
        let screen = GetDC(null_mut());
        BitBlt(dib.dc, 0, 0, r.w(), r.h(), screen, r.l, r.t, SRCCOPY | CAPTUREBLT);
        ReleaseDC(null_mut(), screen);
        GdiFlush();
    }
    let mut v = dib.bytes().to_vec();
    for a in v.iter_mut().skip(3).step_by(4) {
        *a = 255;
    }
    Some(v)
}

/// Repeatedly captures the same rectangle (for GIF recording), reusing one
/// DIB. Must be used on the thread that created it.
pub struct Grabber {
    dib: Dib,
}

impl Grabber {
    pub fn new(w: i32, h: i32) -> Option<Grabber> {
        Some(Grabber { dib: Dib::new(w, h)? })
    }

    /// Returns pixels as little-endian BGRA words (alpha forced to 255).
    pub fn grab(&mut self, x: i32, y: i32, cursor: bool) -> Vec<u32> {
        unsafe {
            let screen = GetDC(null_mut());
            // No CAPTUREBLT here: with DWM the composed desktop already
            // includes layered windows, and the flag makes the real cursor
            // flicker when capturing many times per second.
            BitBlt(self.dib.dc, 0, 0, self.dib.w, self.dib.h, screen, x, y, SRCCOPY);
            ReleaseDC(null_mut(), screen);
            if cursor {
                draw_cursor(self.dib.dc, x, y);
            }
            GdiFlush();
        }
        self.dib
            .bytes()
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], 255]))
            .collect()
    }
}

unsafe fn draw_cursor(dc: HDC, ox: i32, oy: i32) {
    let mut ci: CURSORINFO = std::mem::zeroed();
    ci.cbSize = size_of::<CURSORINFO>() as u32;
    if GetCursorInfo(&mut ci) == 0 || ci.flags & CURSOR_SHOWING == 0 || ci.hCursor.is_null() {
        return;
    }
    let mut ii: ICONINFO = std::mem::zeroed();
    if GetIconInfo(ci.hCursor, &mut ii) == 0 {
        return;
    }
    let x = ci.ptScreenPos.x - ii.xHotspot as i32 - ox;
    let y = ci.ptScreenPos.y - ii.yHotspot as i32 - oy;
    DrawIconEx(dc, x, y, ci.hCursor, 0, 0, 0, null_mut(), DI_NORMAL);
    if !ii.hbmMask.is_null() {
        DeleteObject(ii.hbmMask);
    }
    if !ii.hbmColor.is_null() {
        DeleteObject(ii.hbmColor);
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
