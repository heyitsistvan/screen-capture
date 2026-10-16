//! Clipboard: images (CF_DIB + PNG), files (CF_HDROP) and reading text.

use std::path::Path;
use std::ptr::copy_nonoverlapping;
use std::time::Duration;

use windows_sys::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND, POINT};
use windows_sys::Win32::Graphics::Gdi::{BITMAPINFOHEADER, BI_RGB};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE};
use windows_sys::Win32::System::Ole::{CF_DIB, CF_HDROP, CF_UNICODETEXT};
use windows_sys::Win32::UI::Shell::DROPFILES;

use crate::util::wide;

/// Puts an opaque BGRA image on the clipboard as both CF_DIB and PNG.
pub fn set_image(hwnd: HWND, w: i32, h: i32, bgra: &[u8]) -> bool {
    let row = w as usize * 4;
    let header = size_of::<BITMAPINFOHEADER>();
    let mut dib = vec![0u8; header + row * h as usize];
    let bih = BITMAPINFOHEADER {
        biSize: header as u32,
        biWidth: w,
        biHeight: h, // bottom-up: the most widely understood layout
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB,
        biSizeImage: (row * h as usize) as u32,
        biXPelsPerMeter: 0,
        biYPelsPerMeter: 0,
        biClrUsed: 0,
        biClrImportant: 0,
    };
    unsafe {
        copy_nonoverlapping(&bih as *const _ as *const u8, dib.as_mut_ptr(), header);
    }
    for y in 0..h as usize {
        let src = &bgra[y * row..(y + 1) * row];
        let dst_off = header + (h as usize - 1 - y) * row;
        let dst = &mut dib[dst_off..dst_off + row];
        dst.copy_from_slice(src);
        for a in dst.iter_mut().skip(3).step_by(4) {
            *a = 255;
        }
    }

    let mut items = vec![(CF_DIB as u32, dib)];
    if let Ok(png) = crate::util::encode_png(w, h, bgra) {
        items.push((register("PNG"), png));
    }
    set(hwnd, items)
}

/// Puts a file on the clipboard so it can be pasted into Explorer, chat apps,
/// e-mail and so on (the way an animated GIF survives a copy/paste).
pub fn set_file(hwnd: HWND, path: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    let df = DROPFILES {
        pFiles: size_of::<DROPFILES>() as u32,
        pt: POINT { x: 0, y: 0 },
        fNC: 0,
        fWide: 1,
    };
    let mut data = vec![0u8; size_of::<DROPFILES>()];
    unsafe {
        copy_nonoverlapping(&df as *const _ as *const u8, data.as_mut_ptr(), data.len());
    }
    for c in path.as_os_str().encode_wide().chain([0, 0]) {
        data.extend_from_slice(&c.to_le_bytes());
    }
    const DROPEFFECT_COPY: u32 = 1;
    set(
        hwnd,
        vec![
            (CF_HDROP as u32, data),
            (register("Preferred DropEffect"), DROPEFFECT_COPY.to_le_bytes().to_vec()),
        ],
    )
}

pub fn set_text(hwnd: HWND, s: &str) -> bool {
    let data = s.encode_utf16().chain([0]).flat_map(|c| c.to_le_bytes()).collect();
    set(hwnd, vec![(CF_UNICODETEXT as u32, data)])
}

pub fn get_text(hwnd: HWND) -> Option<String> {
    if !open(hwnd) {
        return None;
    }
    let mut out = None;
    unsafe {
        let h = GetClipboardData(CF_UNICODETEXT as u32);
        if !h.is_null() {
            let p = GlobalLock(h as HGLOBAL) as *const u16;
            if !p.is_null() {
                let max = GlobalSize(h as HGLOBAL) / 2;
                let s = std::slice::from_raw_parts(p, max);
                let n = s.iter().position(|&c| c == 0).unwrap_or(max);
                out = Some(String::from_utf16_lossy(&s[..n]));
                GlobalUnlock(h as HGLOBAL);
            }
        }
        CloseClipboard();
    }
    out
}

fn register(name: &str) -> u32 {
    let w = wide(name);
    unsafe { RegisterClipboardFormatW(w.as_ptr()) }
}

fn open(hwnd: HWND) -> bool {
    // Another app may briefly hold the clipboard open.
    for _ in 0..10 {
        if unsafe { OpenClipboard(hwnd) } != 0 {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    false
}

fn set(hwnd: HWND, items: Vec<(u32, Vec<u8>)>) -> bool {
    if !open(hwnd) {
        return false;
    }
    let mut ok = true;
    unsafe {
        EmptyClipboard();
        for (format, bytes) in items {
            let h = GlobalAlloc(GMEM_MOVEABLE, bytes.len());
            if h.is_null() {
                ok = false;
                continue;
            }
            let p = GlobalLock(h) as *mut u8;
            if p.is_null() {
                GlobalFree(h);
                ok = false;
                continue;
            }
            copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
            GlobalUnlock(h);
            // On success the clipboard owns the memory.
            if SetClipboardData(format, h as HANDLE).is_null() {
                GlobalFree(h);
                ok = false;
            }
        }
        CloseClipboard();
    }
    ok
}
