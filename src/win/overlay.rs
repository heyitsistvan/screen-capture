//! The editor's full-screen window on Windows: captures the screen, feeds
//! Win32 input to the [`Editor`], presents its frames and carries out its
//! actions (clipboard, save dialog, color picker, starting a GIF).

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use std::sync::Arc;

use windows_sys::Win32::Foundation::{BOOL, FALSE, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, TRUE, WPARAM};
use windows_sys::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateRectRgn, DeleteObject, EndPaint, EnumDisplayMonitors, GetMonitorInfoW, GetRegionData,
    GetUpdateRgn, InvalidateRect, UpdateWindow, HDC, HMONITOR, HRGN, MONITORINFO, PAINTSTRUCT, RGNDATAHEADER, SRCCOPY,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::SystemServices::MK_SHIFT;
use windows_sys::Win32::System::Threading::{AttachThreadInput, GetCurrentProcessId, GetCurrentThreadId};
use windows_sys::Win32::UI::Controls::Dialogs::{
    ChooseColorW, GetSaveFileNameW, CC_FULLOPEN, CC_RGBINIT, CHOOSECOLORW, OFN_EXPLORER, OFN_NOCHANGEDIR,
    OFN_OVERWRITEPROMPT, OFN_PATHMUSTEXIST, OPENFILENAMEW,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, ReleaseCapture, SetCapture, SetFocus, VK_CONTROL, VK_SHIFT};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, CreateWindowExW, DefWindowProcW, DestroyWindow, EnumWindows, GetClassNameW, GetCursorPos,
    GetForegroundWindow, GetWindowLongPtrW, GetWindowLongW, GetWindowRect, GetWindowThreadProcessId, IsIconic,
    IsWindowVisible, KillTimer, LoadCursorW, PostMessageW, RegisterClassExW, SetCursor, SetForegroundWindow,
    SetTimer, SetWindowLongPtrW, SetWindowPos, ShowWindow, CS_DBLCLKS, GWLP_USERDATA, GWL_EXSTYLE, HTCLIENT,
    HWND_TOPMOST, IDC_ARROW, IDC_CROSS, IDC_HAND, IDC_IBEAM, IDC_SIZEALL, IDC_SIZENESW, IDC_SIZENS, IDC_SIZENWSE,
    IDC_SIZEWE, IDC_WAIT, MB_ICONERROR, SWP_SHOWWINDOW, SW_SHOW, WM_CAPTURECHANGED, WM_CHAR, WM_CLOSE, WM_DPICHANGED,
    WM_ERASEBKGND, WM_KEYDOWN, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL,
    WM_NCDESTROY, WM_PAINT, WM_RBUTTONDOWN, WM_SETCURSOR, WM_TIMER, WNDCLASSEXW, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_EX_TRANSPARENT, WS_POPUP,
};

use crate::annotate::Rgba;
use super::capture::{self, Dib};
use super::clipboard;
use crate::editor::{Action, Cursor, Editor, Host, Mods, Screen};
use crate::text::Fonts;
use crate::util::{self, wide, Rect};
use super::{WM_APP_OVERLAY_DONE, WM_APP_START_GIF};

const CLASS: &str = "ScreenCaptureOverlay";

thread_local! {
    static CUSTOM_COLORS: Cell<[u32; 16]> = const { Cell::new([0x00FF_FFFF; 16]) };
    static SAVE_DIR: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

struct WinHost {
    hwnd: HWND,
}

impl Host for WinHost {
    fn invalidate(&mut self, r: Rect) {
        unsafe { InvalidateRect(self.hwnd, &r.to_win(), FALSE) };
    }

    fn set_cursor(&mut self, c: Cursor) {
        let id = match c {
            Cursor::Arrow => IDC_ARROW,
            Cursor::Cross => IDC_CROSS,
            Cursor::Hand => IDC_HAND,
            Cursor::IBeam => IDC_IBEAM,
            Cursor::Move => IDC_SIZEALL,
            Cursor::SizeNS => IDC_SIZENS,
            Cursor::SizeWE => IDC_SIZEWE,
            Cursor::SizeNWSE => IDC_SIZENWSE,
            Cursor::SizeNESW => IDC_SIZENESW,
            Cursor::Wait => IDC_WAIT,
        };
        unsafe { SetCursor(LoadCursorW(null_mut(), id)) };
    }

    fn set_timer(&mut self, id: usize, ms: u32) {
        unsafe { SetTimer(self.hwnd, id, ms, None) };
    }

    fn kill_timer(&mut self, id: usize) {
        unsafe { KillTimer(self.hwnd, id) };
    }

    fn capture_mouse(&mut self, on: bool) {
        unsafe {
            if on {
                SetCapture(self.hwnd);
            } else {
                ReleaseCapture();
            }
        }
    }

    fn clipboard_text(&mut self) -> Option<String> {
        clipboard::get_text(self.hwnd)
    }
}

/// Per-window state, owned through GWLP_USERDATA.
struct Shell {
    editor: Box<Editor>,
    main: HWND,
    /// GDI surface the editor's frames are copied into for BitBlt.
    surface: Dib,
    high_surrogate: Option<u16>,
}

pub fn register_class(hinst: HINSTANCE) {
    let cls = wide(CLASS);
    unsafe {
        let mut wc: WNDCLASSEXW = std::mem::zeroed();
        wc.cbSize = size_of::<WNDCLASSEXW>() as u32;
        wc.style = CS_DBLCLKS;
        wc.lpfnWndProc = Some(wndproc);
        wc.hInstance = hinst;
        wc.lpszClassName = cls.as_ptr();
        RegisterClassExW(&wc);
    }
}

/// Captures the screen and shows the editor over it. Returns the window or
/// null on failure.
pub fn open(main: HWND, gif_mode: bool, fonts: Arc<Fonts>) -> HWND {
    let vs = capture::virtual_screen();
    let Some(pixels) = capture::capture(vs) else { return null_mut() };
    let Some(surface) = Dib::new(vs.w(), vs.h()) else { return null_mut() };
    let mut cur = POINT { x: 0, y: 0 };
    unsafe { GetCursorPos(&mut cur) };
    let bounds = Rect::new(0, 0, vs.w(), vs.h());
    let screen = Screen {
        w: vs.w(),
        h: vs.h(),
        pixels,
        monitors: enum_monitors().into_iter().map(|r| r.offset(-vs.l, -vs.t)).collect(),
        windows: enum_windows()
            .into_iter()
            .map(|r| r.offset(-vs.l, -vs.t).intersect(&bounds))
            .filter(|r| !r.is_empty())
            .collect(),
        mouse: (cur.x - vs.l, cur.y - vs.t),
        origin: (vs.l, vs.t),
    };
    unsafe {
        let cls = wide(CLASS);
        let title = wide(util::APP_NAME);
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            cls.as_ptr(),
            title.as_ptr(),
            WS_POPUP,
            vs.l,
            vs.t,
            vs.w(),
            vs.h(),
            null_mut(),
            null_mut(),
            GetModuleHandleW(null()),
            null(),
        );
        if hwnd.is_null() {
            return null_mut();
        }
        let editor = Editor::new(Box::new(WinHost { hwnd }), screen, fonts, gif_mode, util::dpi_scale_at(cur.x, cur.y));
        let shell = Box::new(Shell { editor, main, surface, high_surrogate: None });
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(shell) as isize);
        ShowWindow(hwnd, SW_SHOW);
        SetWindowPos(hwnd, HWND_TOPMOST, vs.l, vs.t, vs.w(), vs.h(), SWP_SHOWWINDOW);
        force_foreground(hwnd);
        UpdateWindow(hwnd);
        hwnd
    }
}

/// Takes the foreground even when triggered from a background hotkey.
unsafe fn force_foreground(hwnd: HWND) {
    let fg = GetForegroundWindow();
    let fg_thread = if fg.is_null() { 0 } else { GetWindowThreadProcessId(fg, null_mut()) };
    let me = GetCurrentThreadId();
    let attach = fg_thread != 0 && fg_thread != me;
    if attach {
        AttachThreadInput(me, fg_thread, TRUE);
    }
    BringWindowToTop(hwnd);
    SetForegroundWindow(hwnd);
    SetFocus(hwnd);
    if attach {
        AttachThreadInput(me, fg_thread, FALSE);
    }
}

fn lparam_xy(lp: LPARAM) -> (i32, i32) {
    ((lp & 0xFFFF) as i16 as i32, ((lp >> 16) & 0xFFFF) as i16 as i32)
}

fn mods() -> Mods {
    unsafe { Mods { ctrl: GetKeyState(VK_CONTROL as i32) < 0, shift: GetKeyState(VK_SHIFT as i32) < 0 } }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Shell;
    // WM_CAPTURECHANGED is sent synchronously from ReleaseCapture inside the
    // editor's own handlers; don't re-enter it for that.
    if p.is_null() || msg == WM_CAPTURECHANGED {
        return DefWindowProcW(hwnd, msg, wp, lp);
    }
    if msg == WM_NCDESTROY {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        let shell = Box::from_raw(p);
        PostMessageW(shell.main, WM_APP_OVERLAY_DONE, hwnd as WPARAM, 0);
        drop(shell); // the editor stores its preferences when dropped
        return DefWindowProcW(hwnd, msg, wp, lp);
    }
    let (res, action) = handle(&mut *p, hwnd, msg, wp, lp);
    if let Some(a) = action {
        perform(hwnd, p, a);
    }
    res.unwrap_or_else(|| DefWindowProcW(hwnd, msg, wp, lp))
}

unsafe fn handle(s: &mut Shell, hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> (Option<LRESULT>, Option<Action>) {
    match msg {
        WM_PAINT => {
            paint(s, hwnd);
            (Some(0), None)
        }
        WM_ERASEBKGND => (Some(1), None),
        WM_SETCURSOR if (lp & 0xFFFF) as u32 == HTCLIENT => {
            s.editor.update_cursor();
            (Some(1), None)
        }
        WM_MOUSEMOVE => {
            let (x, y) = lparam_xy(lp);
            s.editor.on_move(x, y, wp & MK_SHIFT as usize != 0);
            (Some(0), None)
        }
        WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => {
            let (x, y) = lparam_xy(lp);
            (Some(0), s.editor.on_down(x, y, msg == WM_LBUTTONDBLCLK))
        }
        WM_LBUTTONUP => {
            let (x, y) = lparam_xy(lp);
            (Some(0), s.editor.on_up(x, y))
        }
        WM_RBUTTONDOWN => (Some(0), s.editor.on_right()),
        WM_MOUSEWHEEL => {
            s.editor.on_wheel(((wp >> 16) & 0xFFFF) as i16 as i32);
            (Some(0), None)
        }
        WM_KEYDOWN => (Some(0), s.editor.on_key(wp as u16, mods())),
        WM_CHAR => {
            let c = wp as u16;
            if (0xD800..0xDC00).contains(&c) {
                s.high_surrogate = Some(c);
            } else {
                let ch = if (0xDC00..0xE000).contains(&c) {
                    s.high_surrogate.take().and_then(|hi| char::decode_utf16([hi, c]).next()?.ok())
                } else {
                    char::from_u32(c as u32)
                };
                if let Some(ch) = ch {
                    s.editor.on_char(ch);
                }
            }
            (Some(0), None)
        }
        WM_TIMER => {
            s.editor.on_timer(wp);
            (Some(0), None)
        }
        // Spanning monitors with different DPIs must not resize us.
        WM_DPICHANGED => (Some(0), None),
        WM_CLOSE => (Some(0), Some(Action::Close)),
        _ => (None, None),
    }
}

/// Renders the invalid rectangles and blits them to the window.
unsafe fn paint(s: &mut Shell, hwnd: HWND) {
    let rgn = CreateRectRgn(0, 0, 0, 0);
    GetUpdateRgn(hwnd, rgn, FALSE);
    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let hdc = BeginPaint(hwnd, &mut ps);
    let rects = region_rects(rgn, Rect::from_win(&ps.rcPaint));
    DeleteObject(rgn);
    s.editor.render(&rects);
    let (w, h) = s.editor.size();
    let dc = s.surface.dc;
    let src = s.editor.back();
    let dst = s.surface.bytes_mut();
    for r in rects {
        let r = r.intersect(&Rect::new(0, 0, w, h));
        if r.is_empty() {
            continue;
        }
        let n = r.w() as usize * 4;
        for y in r.t..r.b {
            let i = ((y * w + r.l) * 4) as usize;
            dst[i..i + n].copy_from_slice(&src[i..i + n]);
        }
        BitBlt(hdc, r.l, r.t, r.w(), r.h(), dc, r.l, r.t, SRCCOPY);
    }
    EndPaint(hwnd, &ps);
}

/// Carries out an action outside any borrow of the shell: these run modal
/// dialogs or destroy the window, which re-enter the window procedure.
unsafe fn perform(hwnd: HWND, p: *mut Shell, action: Action) {
    match action {
        Action::Close => {
            DestroyWindow(hwnd);
        }
        Action::Copy => {
            let Some((w, h, px)) = (*p).editor.final_image() else { return };
            if clipboard::set_image(hwnd, w, h, &px) {
                DestroyWindow(hwnd);
            } else {
                // Another app holds the clipboard: keep the screenshot.
                (*p).editor.show_flash("The clipboard is busy, try again".into());
            }
        }
        Action::Save => {
            let Some((w, h, px)) = (*p).editor.final_image() else { return };
            let Some(path) = save_dialog(hwnd) else { return };
            match util::encode_png(w, h, &px).and_then(|png| std::fs::write(&path, png).map_err(|e| e.to_string()))
            {
                Ok(()) => {
                    DestroyWindow(hwnd);
                }
                Err(e) => util::message_box(hwnd, &format!("Could not save {}:\n{e}", path.display()), MB_ICONERROR),
            }
        }
        Action::Gif => {
            if let Some(req) = (*p).editor.gif_request() {
                PostMessageW((*p).main, WM_APP_START_GIF, 0, Box::into_raw(Box::new(req)) as LPARAM);
            }
            DestroyWindow(hwnd);
        }
        Action::ChooseColor => {
            let cur = (*p).editor.color();
            if let Some(c) = choose_color(hwnd, cur) {
                (*p).editor.set_color(c);
            }
        }
        Action::ChooseOutlineColor => {
            let cur = (*p).editor.outline_custom();
            if let Some(c) = choose_color(hwnd, cur) {
                (*p).editor.set_custom_outline(c);
            }
        }
        Action::CopyColor => {
            let hex = (*p).editor.color_under_cursor();
            let ok = clipboard::set_text(hwnd, &hex);
            (*p).editor.show_flash(if ok { format!("Copied {hex}") } else { "The clipboard is busy, try again".into() });
        }
    }
}

unsafe fn save_dialog(owner: HWND) -> Option<PathBuf> {
    let dir = SAVE_DIR.with(|d| d.borrow().clone()).unwrap_or_else(util::pictures_dir);
    let name = format!("Screenshot_{}.png", util::timestamp());
    let mut file = [0u16; 1024];
    for (i, c) in name.encode_utf16().take(file.len() - 1).enumerate() {
        file[i] = c;
    }
    let filter: Vec<u16> = "PNG image (*.png)\0*.png\0\0".encode_utf16().collect();
    let dir_w = util::wide_path(&dir);
    let def_ext = wide("png");
    let title = wide("Save screenshot");
    let mut ofn: OPENFILENAMEW = std::mem::zeroed();
    ofn.lStructSize = size_of::<OPENFILENAMEW>() as u32;
    ofn.hwndOwner = owner;
    ofn.lpstrFilter = filter.as_ptr();
    ofn.lpstrFile = file.as_mut_ptr();
    ofn.nMaxFile = file.len() as u32;
    ofn.lpstrInitialDir = dir_w.as_ptr();
    ofn.lpstrDefExt = def_ext.as_ptr();
    ofn.lpstrTitle = title.as_ptr();
    ofn.Flags = OFN_OVERWRITEPROMPT | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR | OFN_EXPLORER;
    if GetSaveFileNameW(&mut ofn) == 0 {
        return None;
    }
    let path = util::path_from_wide(&file);
    if let Some(parent) = path.parent() {
        SAVE_DIR.with(|d| *d.borrow_mut() = Some(parent.to_path_buf()));
    }
    Some(path)
}

unsafe fn choose_color(owner: HWND, cur: Rgba) -> Option<Rgba> {
    let mut custom = CUSTOM_COLORS.with(|c| c.get());
    let mut cc: CHOOSECOLORW = std::mem::zeroed();
    cc.lStructSize = size_of::<CHOOSECOLORW>() as u32;
    cc.hwndOwner = owner;
    cc.rgbResult = cur.0 as u32 | (cur.1 as u32) << 8 | (cur.2 as u32) << 16;
    cc.lpCustColors = custom.as_mut_ptr();
    cc.Flags = CC_RGBINIT | CC_FULLOPEN;
    let ok = ChooseColorW(&mut cc) != 0;
    CUSTOM_COLORS.with(|c| c.set(custom));
    let c = cc.rgbResult;
    ok.then_some(Rgba(c as u8, (c >> 8) as u8, (c >> 16) as u8, 255))
}

/// Visible top-level windows in z-order (topmost first), screen coordinates.
fn enum_windows() -> Vec<Rect> {
    unsafe extern "system" fn cb(hwnd: HWND, lp: LPARAM) -> BOOL {
        let out = &mut *(lp as *mut Vec<Rect>);
        if IsWindowVisible(hwnd) == 0 || IsIconic(hwnd) != 0 {
            return TRUE;
        }
        if GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TRANSPARENT != 0 {
            return TRUE;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid == GetCurrentProcessId() {
            return TRUE;
        }
        let mut cloaked = 0u32;
        DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED as u32, &mut cloaked as *mut u32 as *mut c_void, 4);
        if cloaked != 0 {
            return TRUE;
        }
        let mut name = [0u16; 64];
        let n = GetClassNameW(hwnd, name.as_mut_ptr(), name.len() as i32).max(0) as usize;
        let class = String::from_utf16_lossy(&name[..n]);
        if class == "Progman" || class == "WorkerW" {
            return TRUE; // the desktop: clicking it selects the monitor instead
        }
        let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        if DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS as u32,
            &mut r as *mut RECT as *mut c_void,
            size_of::<RECT>() as u32,
        ) != 0
        {
            GetWindowRect(hwnd, &mut r);
        }
        let r = Rect::from_win(&r);
        if r.w() >= 8 && r.h() >= 8 {
            out.push(r);
        }
        TRUE
    }
    let mut v: Vec<Rect> = Vec::new();
    unsafe { EnumWindows(Some(cb), &mut v as *mut Vec<Rect> as LPARAM) };
    v
}

fn enum_monitors() -> Vec<Rect> {
    unsafe extern "system" fn cb(mon: HMONITOR, _: HDC, _: *mut RECT, lp: LPARAM) -> BOOL {
        let out = &mut *(lp as *mut Vec<Rect>);
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(mon, &mut mi) != 0 {
            out.push(Rect::from_win(&mi.rcMonitor));
        }
        TRUE
    }
    let mut v: Vec<Rect> = Vec::new();
    unsafe { EnumDisplayMonitors(null_mut(), null(), Some(cb), &mut v as *mut Vec<Rect> as LPARAM) };
    v
}

/// Rectangles of a GDI region (falls back to its bounding box).
unsafe fn region_rects(rgn: HRGN, fallback: Rect) -> Vec<Rect> {
    let size = GetRegionData(rgn, 0, null_mut());
    if size == 0 {
        return vec![fallback];
    }
    let mut buf = vec![0u64; (size as usize).div_ceil(8)];
    if GetRegionData(rgn, size, buf.as_mut_ptr() as *mut _) == 0 {
        return vec![fallback];
    }
    let hdr = &*(buf.as_ptr() as *const RGNDATAHEADER);
    let n = hdr.nCount as usize;
    if n == 0 || n > 64 {
        return vec![fallback];
    }
    let rects = (buf.as_ptr() as *const u8).add(hdr.dwSize as usize) as *const RECT;
    (0..n).map(|i| Rect::from_win(&*rects.add(i))).collect()
}

/// The whole screen as the editor would get it (debug self-test).
#[cfg(debug_assertions)]
pub fn screen_for_test() -> Option<Screen> {
    let vs = capture::virtual_screen();
    let pixels = capture::capture(vs)?;
    let monitors = enum_monitors().into_iter().map(|r| r.offset(-vs.l, -vs.t)).collect();
    Some(Screen { w: vs.w(), h: vs.h(), pixels, monitors, windows: Vec::new(), mouse: (0, 0), origin: (vs.l, vs.t) })
}
