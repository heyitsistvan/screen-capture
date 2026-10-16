//! The full-screen capture overlay: a frozen screenshot on which the user
//! picks an area (drag, or click a window), annotates it, then copies/saves
//! it or starts a GIF recording of it.
//!
//! Rendering: the window is backed by one screen-sized DIB. A paint recomposes
//! only the invalidated rectangles from the dimmed screenshot, the annotated
//! selection and the UI chrome. The annotated selection is cached:
//! `committed` holds finished shapes, `comp` adds the shape being drawn.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use std::sync::Arc;

use windows_sys::Win32::Foundation::{BOOL, FALSE, HWND, LPARAM, LRESULT, POINT, RECT, TRUE, WPARAM};
use windows_sys::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateRectRgn, DeleteObject, EndPaint, EnumDisplayMonitors, GetMonitorInfoW, GetRegionData,
    GetUpdateRgn, InvalidateRect, UpdateWindow, HDC, HMONITOR, MONITORINFO, PAINTSTRUCT, RGNDATAHEADER, SRCCOPY,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::SystemServices::MK_SHIFT;
use windows_sys::Win32::System::Threading::{AttachThreadInput, GetCurrentProcessId, GetCurrentThreadId};
use windows_sys::Win32::UI::Controls::Dialogs::{
    ChooseColorW, GetSaveFileNameW, CC_FULLOPEN, CC_RGBINIT, CHOOSECOLORW, OFN_EXPLORER, OFN_NOCHANGEDIR,
    OFN_OVERWRITEPROMPT, OFN_PATHMUSTEXIST, OPENFILENAMEW,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, SetFocus, VK_BACK, VK_CONTROL, VK_DELETE, VK_DOWN, VK_ESCAPE, VK_LEFT,
    VK_RETURN, VK_RIGHT,
    VK_SHIFT, VK_UP,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, CreateWindowExW, DefWindowProcW, DestroyWindow, EnumWindows, GetClassNameW, GetCursorPos,
    GetForegroundWindow, GetWindowLongPtrW, GetWindowLongW, GetWindowRect, GetWindowThreadProcessId, IsIconic,
    IsWindowVisible, KillTimer, LoadCursorW, PostMessageW, RegisterClassExW, SetCursor, SetForegroundWindow,
    SetTimer, SetWindowLongPtrW, SetWindowPos, ShowWindow, CREATESTRUCTW, CS_DBLCLKS, GWLP_USERDATA, GWL_EXSTYLE,
    HTCLIENT, HWND_TOPMOST, IDC_ARROW, IDC_CROSS, IDC_HAND, IDC_IBEAM, IDC_SIZEALL, IDC_SIZENESW, IDC_SIZENS,
    IDC_SIZENWSE, IDC_SIZEWE, IDC_WAIT, MB_ICONERROR, SWP_SHOWWINDOW, SW_SHOW, WM_CAPTURECHANGED, WM_CHAR, WM_CLOSE,
    WM_DPICHANGED, WM_ERASEBKGND, WM_KEYDOWN, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE,
    WM_MOUSEWHEEL, WM_NCCREATE, WM_NCDESTROY, WM_PAINT, WM_RBUTTONDOWN, WM_SETCURSOR, WM_TIMER, WNDCLASSEXW,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

use crate::annotate::{self, Pt, Rgba, Shape, Tool};
use crate::capture::{self, Dib};
use crate::clipboard;
use crate::gif_rec::GifRequest;
use crate::text::{self, Fonts};
use crate::ui::{Canvas, Icon};
use crate::util::{self, wide, Rect};
use crate::{WM_APP_OVERLAY_DONE, WM_APP_START_GIF};

const CLASS: &str = "ScreenCaptureOverlay";

const ACCENT: Rgba = Rgba(10, 132, 255, 255);
const WHITE: Rgba = Rgba(255, 255, 255, 255);
const BAR_BG: Rgba = Rgba(30, 31, 34, 242);
const ICON_FG: Rgba = Rgba(230, 232, 236, 255);
const ICON_DIM: Rgba = Rgba(100, 103, 110, 255);
const HOVER_BG: Rgba = Rgba(255, 255, 255, 30);
const LABEL_BG: Rgba = Rgba(20, 20, 22, 225);
const LABEL_FG: Rgba = Rgba(240, 240, 242, 255);

pub const PALETTE: [Rgba; 11] = [
    Rgba(255, 59, 48, 255),   // red
    Rgba(255, 149, 0, 255),   // orange
    Rgba(255, 214, 10, 255),  // yellow
    Rgba(52, 199, 89, 255),   // green
    Rgba(0, 199, 190, 255),   // teal
    Rgba(10, 132, 255, 255),  // blue
    Rgba(175, 82, 222, 255),  // purple
    Rgba(255, 45, 85, 255),   // pink
    Rgba(255, 255, 255, 255), // white
    Rgba(142, 142, 147, 255), // gray
    Rgba(0, 0, 0, 255),       // black
];

const TOOLS: [(Tool, Icon, &str, u8); 9] = [
    (Tool::Pen, Icon::Pen, "Pen (P)", b'P'),
    (Tool::Line, Icon::Line, "Line (L)", b'L'),
    (Tool::Arrow, Icon::Arrow, "Arrow (A)", b'A'),
    (Tool::Rect, Icon::Rect, "Rectangle (R)", b'R'),
    (Tool::Ellipse, Icon::Ellipse, "Ellipse (E)", b'E'),
    (Tool::Marker, Icon::Marker, "Marker (M)", b'M'),
    (Tool::Text, Icon::Text, "Text (T)", b'T'),
    (Tool::Step, Icon::Step, "Step number (N)", b'N'),
    (Tool::Pixelate, Icon::Pixelate, "Pixelate (X)", b'X'),
];

const TIMER_CARET: usize = 1;
const TIMER_FLASH: usize = 2;
const MAG_CELLS: i32 = 15;

/// Settings that carry over between captures. Sizes are in 96-DPI pixels.
#[derive(Clone, Copy)]
struct Prefs {
    color: Rgba,
    stroke: f32,
    marker: f32,
    text: f32,
    pixel: f32,
    /// Diameter of step numbers.
    step: f32,
    /// Halo around new drawings.
    outline: OutlineMode,
    /// The mode the O key switches back to after turning the outline off.
    outline_on: OutlineMode,
    outline_custom: Rgba,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum OutlineMode {
    Off,
    /// Black or white, whichever stands out (see `annotate::auto_outline`).
    Auto,
    White,
    Black,
    Custom,
}

const OUTLINE_MODES: [OutlineMode; 5] =
    [OutlineMode::Off, OutlineMode::Auto, OutlineMode::White, OutlineMode::Black, OutlineMode::Custom];

/// The outline setting as stored in settings.ini.
pub fn outline_setting() -> String {
    let p = PREFS.with(|p| p.get());
    match p.outline {
        OutlineMode::Off => "off".into(),
        OutlineMode::Auto => "auto".into(),
        OutlineMode::White => "white".into(),
        OutlineMode::Black => "black".into(),
        OutlineMode::Custom => {
            let c = p.outline_custom;
            format!("#{:02x}{:02x}{:02x}", c.0, c.1, c.2)
        }
    }
}

pub fn apply_outline_setting(s: &str) {
    let mut p = PREFS.with(|p| p.get());
    p.outline = match s.trim().to_ascii_lowercase().as_str() {
        "off" => OutlineMode::Off,
        "white" => OutlineMode::White,
        "black" => OutlineMode::Black,
        hex if hex.len() == 7 && hex.starts_with('#') => match u32::from_str_radix(&hex[1..], 16) {
            Ok(v) => {
                p.outline_custom = Rgba((v >> 16) as u8, (v >> 8) as u8, v as u8, 255);
                OutlineMode::Custom
            }
            Err(_) => OutlineMode::Auto,
        },
        _ => OutlineMode::Auto,
    };
    if p.outline != OutlineMode::Off {
        p.outline_on = p.outline;
    }
    PREFS.with(|c| c.set(p));
}

thread_local! {
    static PREFS: Cell<Prefs> = const {
        Cell::new(Prefs {
            color: PALETTE[0],
            stroke: 3.0,
            marker: 16.0,
            text: 20.0,
            pixel: 10.0,
            step: 28.0,
            outline: OutlineMode::Auto,
            outline_on: OutlineMode::Auto,
            outline_custom: Rgba(255, 214, 10, 255),
        })
    };
    static CUSTOM_COLORS: Cell<[u32; 16]> = const { Cell::new([0x00FF_FFFF; 16]) };
    static SAVE_DIR: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Btn {
    Tool(Tool),
    Color,
    Undo,
    Redo,
    Gif,
    Copy,
    Save,
    Close,
    Swatch(usize),
    Custom,
    Outline(OutlineMode),
}

impl Btn {
    fn tooltip(self) -> Option<&'static str> {
        Some(match self {
            Btn::Tool(t) => TOOLS.iter().find(|x| x.0 == t)?.2,
            Btn::Color => "Color",
            Btn::Undo => "Undo (Ctrl+Z)",
            Btn::Redo => "Redo (Ctrl+Y)",
            Btn::Gif => "Record GIF of this area (G)",
            Btn::Copy => "Copy to clipboard (Ctrl+C)",
            Btn::Save => "Save as PNG (Ctrl+S)",
            Btn::Close => "Close (Esc)",
            Btn::Custom => "Custom color…",
            Btn::Outline(OutlineMode::Off) => "No outline (O)",
            Btn::Outline(OutlineMode::Auto) => "Automatic outline: black or white, whichever stands out (O)",
            Btn::Outline(OutlineMode::White) => "White outline",
            Btn::Outline(OutlineMode::Black) => "Black outline",
            Btn::Outline(OutlineMode::Custom) => "Custom outline color",
            Btn::Swatch(_) => return None,
        })
    }

    fn icon(self) -> Option<Icon> {
        Some(match self {
            Btn::Tool(t) => TOOLS.iter().find(|x| x.0 == t)?.1,
            Btn::Undo => Icon::Undo,
            Btn::Redo => Icon::Redo,
            Btn::Gif => Icon::Gif,
            Btn::Copy => Icon::Copy,
            Btn::Save => Icon::Save,
            Btn::Close => Icon::Close,
            Btn::Custom => Icon::Plus,
            Btn::Color | Btn::Swatch(_) | Btn::Outline(_) => return None,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Bar {
    V,
    H,
    P,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Handle {
    N,
    S,
    E,
    W,
    NE,
    NW,
    SE,
    SW,
}

#[derive(Clone, Copy)]
enum Drag {
    None,
    /// Mouse is down but hasn't moved far enough to start a selection.
    Pending,
    NewSel { ax: i32, ay: i32 },
    Move { orig: Rect, sx: i32, sy: i32 },
    /// Dragging a placed drawing by (`dx`, `dy`) so far; a click without
    /// moving edits a text instead.
    MoveShape { index: usize, sx: i32, sy: i32, dx: i32, dy: i32, moved: bool },
    /// Dragging the text being typed.
    MoveEdit { sx: i32, sy: i32, orig: Pt },
    Resize { h: Handle, orig: Rect, sx: i32, sy: i32 },
    Draw,
    Button(Btn),
}

struct TextEdit {
    pos: Pt,
    text: String,
    color: Rgba,
    size: f32,
    /// When re-editing a placed text: its index in `shapes` and how it was.
    original: Option<(usize, Shape)>,
}

/// Things done after the handler returns, outside any borrow of the overlay,
/// because they run modal loops or destroy the window.
enum Action {
    Close,
    Copy,
    Save,
    Gif,
    ChooseColor,
    ChooseOutlineColor,
    CopyColor,
}

struct Layout {
    vbar: Rect,
    hbar: Rect,
    palette: Option<Rect>,
    buttons: Vec<(Btn, Rect, Bar)>,
    seps: Vec<Rect>,
}

/// Independently invalidated regions of the scene.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Part {
    Sel,
    Bars,
    Hover,
    Hint,
    Mag,
    Flash,
}

pub struct Overlay {
    hwnd: HWND,
    main: HWND,
    gif_mode: bool,
    /// Screen position of the window's (0, 0).
    ox: i32,
    oy: i32,
    w: i32,
    h: i32,
    shot: Vec<u8>,
    dim: Vec<u8>,
    back: Dib,
    fonts: Arc<Fonts>,
    scale: f32,
    monitors: Vec<Rect>,
    windows: Vec<Rect>,
    sel: Option<Rect>,
    drag: Drag,
    down: (i32, i32),
    mouse: (i32, i32),
    hover_win: Option<Rect>,
    hover_btn: Option<Btn>,
    tool: Option<Tool>,
    prefs: Prefs,
    shapes: Vec<Shape>,
    /// Undo/redo snapshots of `shapes`.
    history: Vec<Vec<Shape>>,
    future: Vec<Vec<Shape>>,
    current: Option<Shape>,
    edit: Option<TextEdit>,
    caret_on: bool,
    high_surrogate: Option<u16>,
    committed: Vec<u8>,
    committed_ok: bool,
    comp: Vec<u8>,
    comp_ok: bool,
    palette_open: bool,
    flash: Option<String>,
    last_parts: Vec<(Part, Rect)>,
}

pub fn register_class(hinst: windows_sys::Win32::Foundation::HINSTANCE) {
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

/// Captures the screen and shows the overlay. Returns the overlay window or
/// null on failure.
pub fn open(main: HWND, gif_mode: bool, fonts: Arc<Fonts>) -> HWND {
    let Some(ov) = Overlay::new(main, gif_mode, fonts) else {
        return null_mut();
    };
    let (x, y, w, h) = (ov.ox, ov.oy, ov.w, ov.h);
    unsafe {
        let cls = wide(CLASS);
        let title = wide(util::APP_NAME);
        let p = Box::into_raw(ov);
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            cls.as_ptr(),
            title.as_ptr(),
            WS_POPUP,
            x,
            y,
            w,
            h,
            null_mut(),
            null_mut(),
            GetModuleHandleW(null()),
            p as *const c_void,
        );
        if hwnd.is_null() {
            // Ownership is unclear if creation failed half-way; leak rather
            // than risk a double free.
            return null_mut();
        }
        ShowWindow(hwnd, SW_SHOW);
        SetWindowPos(hwnd, HWND_TOPMOST, x, y, w, h, SWP_SHOWWINDOW);
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

fn key_down(vk: u16) -> bool {
    unsafe { GetKeyState(vk as i32) < 0 }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_NCCREATE {
        let cs = &*(lp as *const CREATESTRUCTW);
        let p = cs.lpCreateParams as *mut Overlay;
        (*p).hwnd = hwnd;
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, p as isize);
        return DefWindowProcW(hwnd, msg, wp, lp);
    }
    let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Overlay;
    // WM_CAPTURECHANGED is sent synchronously from ReleaseCapture inside our
    // own handlers; don't re-enter the overlay for it.
    if p.is_null() || msg == WM_CAPTURECHANGED {
        return DefWindowProcW(hwnd, msg, wp, lp);
    }
    if msg == WM_NCDESTROY {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        let ov = Box::from_raw(p);
        PREFS.with(|c| c.set(ov.prefs));
        PostMessageW(ov.main, WM_APP_OVERLAY_DONE, hwnd as WPARAM, 0);
        drop(ov);
        return DefWindowProcW(hwnd, msg, wp, lp);
    }
    let (res, action) = (*p).handle(msg, wp, lp);
    if let Some(a) = action {
        perform(hwnd, p, a);
    }
    match res {
        Some(r) => r,
        None => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

unsafe fn perform(hwnd: HWND, p: *mut Overlay, action: Action) {
    match action {
        Action::Close => {
            DestroyWindow(hwnd);
        }
        Action::Copy => {
            if let Some((w, h, px)) = (*p).final_image() {
                clipboard::set_image(hwnd, w, h, &px);
            }
            DestroyWindow(hwnd);
        }
        Action::Save => {
            let Some((w, h, px)) = (*p).final_image() else { return };
            let Some(path) = save_dialog(hwnd) else { return };
            match capture::encode_png(w, h, &px).and_then(|png| std::fs::write(&path, png).map_err(|e| e.to_string()))
            {
                Ok(()) => {
                    DestroyWindow(hwnd);
                }
                Err(e) => util::message_box(hwnd, &format!("Could not save {}:\n{e}", path.display()), MB_ICONERROR),
            }
        }
        Action::Gif => {
            if let Some(req) = (*p).gif_request() {
                let main = (*p).main;
                PostMessageW(main, WM_APP_START_GIF, 0, Box::into_raw(Box::new(req)) as LPARAM);
            }
            DestroyWindow(hwnd);
        }
        Action::ChooseColor => {
            let cur = (*p).prefs.color;
            if let Some(c) = choose_color(hwnd, cur) {
                (*p).set_color(c);
            }
        }
        Action::CopyColor => {
            let hex = (*p).color_under_cursor();
            let ok = clipboard::set_text(hwnd, &hex);
            (*p).show_flash(if ok { format!("Copied {hex}") } else { "Clipboard busy, try again".into() });
        }
        Action::ChooseOutlineColor => {
            let cur = (*p).prefs.outline_custom;
            if let Some(c) = choose_color(hwnd, cur) {
                (*p).prefs.outline_custom = c;
                (*p).set_outline(OutlineMode::Custom);
            }
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

/// Copies rectangle `r` (destination coordinates) from `src`, whose pixel
/// (0, 0) sits at `(sx0, sy0)` in destination coordinates.
fn blit(src: &[u8], src_w: i32, sx0: i32, sy0: i32, dst: &mut [u8], dst_w: i32, r: Rect) {
    let n = r.w() as usize * 4;
    for y in r.t..r.b {
        let s = (((y - sy0) * src_w + (r.l - sx0)) * 4) as usize;
        let d = ((y * dst_w + r.l) * 4) as usize;
        dst[d..d + n].copy_from_slice(&src[s..s + n]);
    }
}

/// Outline of thickness `t` drawn inside `r`.
fn frame_px(cv: &mut Canvas, r: Rect, t: i32, c: Rgba) {
    cv.fill_px(Rect::new(r.l, r.t, r.r, r.t + t), c);
    cv.fill_px(Rect::new(r.l, r.b - t, r.r, r.b), c);
    cv.fill_px(Rect::new(r.l, r.t + t, r.l + t, r.b - t), c);
    cv.fill_px(Rect::new(r.r - t, r.t + t, r.r, r.b - t), c);
}

/// Rectangles of a GDI region (falls back to its bounding box).
unsafe fn region_rects(rgn: windows_sys::Win32::Graphics::Gdi::HRGN, fallback: Rect) -> Vec<Rect> {
    let size = GetRegionData(rgn, 0, null_mut());
    if size == 0 {
        return vec![fallback];
    }
    let mut buf = vec![0u64; (size as usize + 7) / 8];
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

/// Whether two text shapes have the same content, placement and look.
fn same_text(a: &Shape, b: &Shape) -> bool {
    match (a, b) {
        (
            Shape::Text { pos: p1, text: t1, color: c1, size: s1, .. },
            Shape::Text { pos: p2, text: t2, color: c2, size: s2, .. },
        ) => p1 == p2 && t1 == t2 && c1 == c2 && s1 == s2,
        _ => false,
    }
}

impl Overlay {
    fn new(main: HWND, gif_mode: bool, fonts: Arc<Fonts>) -> Option<Box<Overlay>> {
        let vs = capture::virtual_screen();
        let shot = capture::capture(vs)?;
        Self::with_image(main, gif_mode, fonts, vs, shot)
    }

    fn with_image(main: HWND, gif_mode: bool, fonts: Arc<Fonts>, vs: Rect, shot: Vec<u8>) -> Option<Box<Overlay>> {
        let back = Dib::new(vs.w(), vs.h())?;
        let mut dim = shot.clone();
        for p in dim.chunks_exact_mut(4) {
            p[0] >>= 1;
            p[1] >>= 1;
            p[2] >>= 1;
        }
        let mut cur = POINT { x: 0, y: 0 };
        unsafe { GetCursorPos(&mut cur) };
        let bounds = Rect::new(0, 0, vs.w(), vs.h());
        let monitors = enum_monitors().into_iter().map(|r| r.offset(-vs.l, -vs.t)).collect();
        let windows = enum_windows()
            .into_iter()
            .map(|r| r.offset(-vs.l, -vs.t).intersect(&bounds))
            .filter(|r| !r.is_empty())
            .collect();
        let mut ov = Box::new(Overlay {
            hwnd: null_mut(),
            main,
            gif_mode,
            ox: vs.l,
            oy: vs.t,
            w: vs.w(),
            h: vs.h(),
            shot,
            dim,
            back,
            fonts,
            scale: util::dpi_scale_at(cur.x, cur.y),
            monitors,
            windows,
            sel: None,
            drag: Drag::None,
            down: (0, 0),
            mouse: ((cur.x - vs.l).clamp(0, vs.w() - 1), (cur.y - vs.t).clamp(0, vs.h() - 1)),
            hover_win: None,
            hover_btn: None,
            tool: None,
            prefs: PREFS.with(|c| c.get()),
            shapes: Vec::new(),
            history: Vec::new(),
            future: Vec::new(),
            current: None,
            edit: None,
            caret_on: true,
            high_surrogate: None,
            committed: Vec::new(),
            committed_ok: false,
            comp: Vec::new(),
            comp_ok: false,
            palette_open: false,
            flash: None,
            last_parts: Vec::new(),
        });
        ov.hover_win = ov.window_at(ov.mouse.0, ov.mouse.1);
        ov.last_parts = ov.scene_parts();
        Some(ov)
    }

    // ----- geometry helpers -------------------------------------------------

    fn px(&self, v: f32) -> i32 {
        (v * self.scale).round() as i32
    }

    fn pxf(&self, v: f32) -> f32 {
        v * self.scale
    }

    fn bounds(&self) -> Rect {
        Rect::new(0, 0, self.w, self.h)
    }

    fn monitor_at(&self, x: i32, y: i32) -> Rect {
        self.monitors.iter().copied().find(|m| m.contains(x, y)).unwrap_or_else(|| self.bounds())
    }

    fn monitor_for(&self, r: &Rect) -> Rect {
        self.monitors
            .iter()
            .copied()
            .max_by_key(|m| m.intersect(r).area())
            .filter(|m| m.intersect(r).area() > 0)
            .unwrap_or_else(|| {
                let (cx, cy) = r.center();
                self.monitor_at(cx, cy)
            })
    }

    /// The window (or monitor, over the desktop) under a point.
    fn window_at(&self, x: i32, y: i32) -> Option<Rect> {
        let r = self
            .windows
            .iter()
            .copied()
            .find(|r| r.contains(x, y))
            .or_else(|| self.monitors.iter().copied().find(|m| m.contains(x, y)))?
            .intersect(&self.bounds());
        (!r.is_empty()).then_some(r)
    }

    fn hover_rect(&self) -> Option<Rect> {
        if self.sel.is_none() && matches!(self.drag, Drag::None | Drag::Pending) {
            self.hover_win
        } else {
            None
        }
    }

    fn set_sel(&mut self, r: Option<Rect>) {
        if self.sel != r {
            self.sel = r;
            self.committed_ok = false;
            self.comp_ok = false;
        }
    }

    fn handle_size(&self) -> i32 {
        self.px(7.0).max(5) | 1
    }

    fn hit_handle(&self, x: i32, y: i32) -> Option<Handle> {
        let s = self.sel?;
        let tol = self.px(6.0).max(4);
        let (cx, cy) = s.center();
        let pts = [
            (Handle::NW, s.l, s.t),
            (Handle::NE, s.r, s.t),
            (Handle::SW, s.l, s.b),
            (Handle::SE, s.r, s.b),
            (Handle::N, cx, s.t),
            (Handle::S, cx, s.b),
            (Handle::W, s.l, cy),
            (Handle::E, s.r, cy),
        ];
        for (h, px, py) in pts {
            if (x - px).abs() <= tol && (y - py).abs() <= tol {
                return Some(h);
            }
        }
        if self.tool.is_none() {
            // Without a drawing tool the whole edge is grabbable.
            let e = self.px(4.0).max(3);
            if y >= s.t - e && y <= s.b + e {
                if (x - s.l).abs() <= e {
                    return Some(Handle::W);
                }
                if (x - s.r).abs() <= e {
                    return Some(Handle::E);
                }
            }
            if x >= s.l - e && x <= s.r + e {
                if (y - s.t).abs() <= e {
                    return Some(Handle::N);
                }
                if (y - s.b).abs() <= e {
                    return Some(Handle::S);
                }
            }
        }
        None
    }

    // ----- layout -----------------------------------------------------------

    fn layout(&self) -> Option<Layout> {
        let sel = self.sel?;
        if self.gif_mode || matches!(self.drag, Drag::NewSel { .. }) {
            return None;
        }
        let b = self.px(34.0);
        let p = self.px(4.0);
        let gap = self.px(8.0);
        let sep = self.px(9.0);
        let mon = self.monitor_for(&sel);

        let mut vitems: Vec<Option<Btn>> = TOOLS.iter().map(|t| Some(Btn::Tool(t.0))).collect();
        vitems.extend([None, Some(Btn::Color), None, Some(Btn::Undo), Some(Btn::Redo)]);
        let nb = vitems.iter().filter(|i| i.is_some()).count() as i32;
        let ns = vitems.len() as i32 - nb;
        let (vw, vh) = (b + 2 * p, 2 * p + nb * b + ns * sep);
        let vx = if sel.r + gap + vw <= mon.r {
            sel.r + gap
        } else if sel.l - gap - vw >= mon.l {
            sel.l - gap - vw
        } else {
            sel.r - gap - vw
        };
        let vy = (sel.b - vh).min(mon.b - vh).max(mon.t);
        let vbar = Rect::xywh(vx, vy, vw, vh);

        let hitems = [Some(Btn::Gif), None, Some(Btn::Copy), Some(Btn::Save), None, Some(Btn::Close)];
        let (hw, hh) = (2 * p + 4 * b + 2 * sep, b + 2 * p);
        let hy = if sel.b + gap + hh <= mon.b {
            sel.b + gap
        } else if sel.t - gap - hh >= mon.t {
            sel.t - gap - hh
        } else {
            sel.b - gap - hh
        };
        let mut hx = (sel.r - hw).min(mon.r - hw).max(mon.l);
        if !Rect::xywh(hx, hy, hw, hh).intersect(&vbar).is_empty() {
            hx = if vbar.l - gap - hw >= mon.l { vbar.l - gap - hw } else { vbar.r + gap };
        }
        let hbar = Rect::xywh(hx, hy, hw, hh);

        let mut buttons = Vec::new();
        let mut seps = Vec::new();
        let mut y = vbar.t + p;
        for it in vitems {
            match it {
                Some(btn) => {
                    buttons.push((btn, Rect::xywh(vbar.l + p, y, b, b), Bar::V));
                    y += b;
                }
                None => {
                    seps.push(Rect::xywh(vbar.l + p + self.px(6.0), y + sep / 2, b - self.px(12.0), 1));
                    y += sep;
                }
            }
        }
        let mut x = hbar.l + p;
        for it in hitems {
            match it {
                Some(btn) => {
                    buttons.push((btn, Rect::xywh(x, hbar.t + p, b, b), Bar::H));
                    x += b;
                }
                None => {
                    seps.push(Rect::xywh(x + sep / 2, hbar.t + p + self.px(6.0), 1, b - self.px(12.0)));
                    x += sep;
                }
            }
        }

        let mut palette = None;
        if self.palette_open {
            let cb = buttons.iter().find(|(b, ..)| *b == Btn::Color).map(|x| x.1).unwrap_or(vbar);
            let cell = self.px(28.0);
            let pad = self.px(6.0);
            let (cols, rows) = (4, 3);
            // Below the colors: an "Outline" label and a row of outline modes.
            let (label_h, mode_h) = (self.px(22.0), self.px(28.0));
            let (pw, ph) = (2 * pad + cols * cell, 2 * pad + rows * cell + label_h + mode_h);
            let right = vbar.r + self.px(6.0);
            let left = vbar.l - self.px(6.0) - pw;
            let px = if right + pw <= mon.r { right } else { left };
            let py = (cb.t + b / 2 - ph / 2).min(mon.b - ph).max(mon.t);
            let pr = Rect::xywh(px, py, pw, ph);
            for i in 0..(cols * rows) {
                let btn = if (i as usize) < PALETTE.len() { Btn::Swatch(i as usize) } else { Btn::Custom };
                let r = Rect::xywh(pr.l + pad + (i % cols) * cell, pr.t + pad + (i / cols) * cell, cell, cell);
                buttons.push((btn, r, Bar::P));
            }
            let mode_w = cols * cell / OUTLINE_MODES.len() as i32;
            let mode_y = pr.t + pad + rows * cell + label_h;
            for (i, m) in OUTLINE_MODES.iter().enumerate() {
                let r = Rect::xywh(pr.l + pad + i as i32 * mode_w, mode_y, mode_w, mode_h);
                buttons.push((Btn::Outline(*m), r, Bar::P));
            }
            palette = Some(pr);
        }
        Some(Layout { vbar, hbar, palette, buttons, seps })
    }

    fn hit_button(&self, x: i32, y: i32) -> Option<Btn> {
        let lay = self.layout()?;
        // Palette entries were pushed last and sit on top.
        lay.buttons.iter().rev().find(|(_, r, _)| r.contains(x, y)).map(|(b, ..)| *b)
    }

    fn on_ui(&self, x: i32, y: i32) -> bool {
        self.layout().is_some_and(|l| {
            l.vbar.contains(x, y) || l.hbar.contains(x, y) || l.palette.is_some_and(|p| p.contains(x, y))
        })
    }

    fn label_rect(&self, text: &str, fpx: f32) -> (i32, i32) {
        let tw = text::line_width(&self.fonts.ui, text, fpx);
        let (_, lh) = text::line_metrics(&self.fonts.ui, fpx);
        (tw.ceil() as i32 + self.px(14.0), lh.ceil() as i32 + self.px(6.0))
    }

    fn size_label(&self, r: Rect) -> (Rect, String) {
        let text = format!("{} × {}", r.w(), r.h());
        let (pw, ph) = self.label_rect(&text, self.pxf(12.0));
        let mon = self.monitor_for(&r);
        let mut x = r.l;
        let mut y = r.t - ph - self.px(6.0);
        if y < mon.t {
            x = r.l + self.px(6.0);
            y = r.t + self.px(6.0);
        }
        (Rect::xywh(x.min(mon.r - pw).max(mon.l), y, pw, ph), text)
    }

    fn tooltip(&self, lay: &Layout) -> Option<(Rect, &'static str)> {
        if !matches!(self.drag, Drag::None) {
            return None;
        }
        let btn = self.hover_btn?;
        let text = btn.tooltip()?;
        let (_, r, bar) = *lay.buttons.iter().find(|(b, ..)| *b == btn)?;
        let (pw, ph) = self.label_rect(text, self.pxf(12.0));
        let mon = self.monitor_for(&r);
        let gap = self.px(8.0);
        let (cx, cy) = r.center();
        let (x, y) = match bar {
            Bar::V => {
                let x = if lay.vbar.l - gap - pw >= mon.l { lay.vbar.l - gap - pw } else { lay.vbar.r + gap };
                (x, cy - ph / 2)
            }
            Bar::H | Bar::P => {
                let top = if bar == Bar::H { lay.hbar.t } else { r.t };
                let y = if top - gap - ph >= mon.t { top - gap - ph } else { r.b + gap };
                (cx - pw / 2, y)
            }
        };
        let x = x.min(mon.r - pw).max(mon.l);
        let y = y.min(mon.b - ph).max(mon.t);
        Some((Rect::xywh(x, y, pw, ph), text))
    }

    fn hint(&self) -> Option<(Rect, &'static str)> {
        if self.sel.is_some() || !matches!(self.drag, Drag::None | Drag::Pending) {
            return None;
        }
        let text = if self.gif_mode {
            "Record GIF: drag to select an area, or click a window   ·   Esc to cancel"
        } else {
            "Drag to select an area, or click a window   ·   C copies the color under the cursor   ·   Esc to cancel"
        };
        let (pw, ph) = self.label_rect(text, self.pxf(13.0));
        let (pw, ph) = (pw + self.px(10.0), ph + self.px(8.0));
        let (mx, my) = self.mouse;
        let mon = self.monitor_at(mx, my);
        let x = mon.l + (mon.w() - pw) / 2;
        let mut y = mon.t + self.px(28.0);
        if Rect::xywh(x, y, pw, ph).inflate(self.px(40.0)).contains(mx, my) {
            y = mon.b - self.px(28.0) - ph; // keep out of the cursor's way
        }
        Some((Rect::xywh(x, y, pw, ph), text))
    }

    fn show_magnifier(&self) -> bool {
        match self.drag {
            Drag::NewSel { .. } | Drag::Resize { .. } => true,
            Drag::None | Drag::Pending => self.sel.is_none(),
            _ => false,
        }
    }

    fn magnifier_rect(&self) -> Option<Rect> {
        if !self.show_magnifier() {
            return None;
        }
        let cell = self.px(8.0).max(5);
        let size = MAG_CELLS * cell;
        let total_h = size + self.px(42.0);
        let (mx, my) = self.mouse;
        let off = self.px(24.0);
        let mon = self.monitor_at(mx, my);
        let mut x = mx + off;
        if x + size > mon.r {
            x = mx - off - size;
        }
        let mut y = my + off;
        if y + total_h > mon.b {
            y = my - off - total_h;
        }
        Some(Rect::xywh(x, y, size, total_h))
    }

    fn flash_rect(&self) -> Option<Rect> {
        let text = self.flash.as_ref()?;
        let (pw, ph) = self.label_rect(text, self.pxf(12.0));
        let (mx, my) = self.mouse;
        Some(Rect::xywh(mx + self.px(18.0), my + self.px(18.0), pw, ph))
    }

    fn scene_parts(&self) -> Vec<(Part, Rect)> {
        let mut v = Vec::new();
        let hs = self.handle_size() + 3;
        if let Some(sel) = self.sel {
            v.push((Part::Sel, sel.inflate(hs).union(&self.size_label(sel).0)));
            if let Some(lay) = self.layout() {
                v.push((Part::Bars, lay.vbar));
                v.push((Part::Bars, lay.hbar));
                if let Some(p) = lay.palette {
                    v.push((Part::Bars, p));
                }
                if let Some((r, _)) = self.tooltip(&lay) {
                    v.push((Part::Bars, r));
                }
            }
        }
        if let Some(hw) = self.hover_rect() {
            v.push((Part::Hover, hw.inflate(hs).union(&self.size_label(hw).0)));
        }
        if let Some((r, _)) = self.hint() {
            v.push((Part::Hint, r));
        }
        if let Some(r) = self.magnifier_rect() {
            v.push((Part::Mag, r.inflate(3)));
        }
        if let Some(r) = self.flash_rect() {
            v.push((Part::Flash, r));
        }
        v
    }

    /// Invalidates what may have changed since the last call. Parts with
    /// static content (hover highlight, hint) are skipped if they didn't move,
    /// which keeps hovering over a maximized window cheap.
    fn invalidate(&mut self) {
        let new = self.scene_parts();
        if !self.hwnd.is_null() {
            for part in new.iter().chain(self.last_parts.iter()) {
                let is_static = matches!(part.0, Part::Hover | Part::Hint);
                if is_static && new.contains(part) && self.last_parts.contains(part) {
                    continue;
                }
                let r = part.1.intersect(&self.bounds());
                if !r.is_empty() {
                    unsafe { InvalidateRect(self.hwnd, &r.to_win(), FALSE) };
                }
            }
        }
        self.last_parts = new;
    }

    // ----- composition ------------------------------------------------------

    fn ensure_committed(&mut self) {
        let Some(sel) = self.sel else { return };
        if self.committed_ok {
            return;
        }
        let (sw, sh) = (sel.w(), sel.h());
        self.committed.resize(sw as usize * sh as usize * 4, 0);
        let n = sw as usize * 4;
        for y in 0..sh {
            let src = (((sel.t + y) * self.w + sel.l) * 4) as usize;
            let dst = y as usize * n;
            self.committed[dst..dst + n].copy_from_slice(&self.shot[src..src + n]);
        }
        annotate::render(&mut self.committed, sw, sh, &self.shapes, sel.l as f32, sel.t as f32, &self.fonts);
        self.committed_ok = true;
        self.comp_ok = false;
    }

    fn edit_rect(&self) -> Option<Rect> {
        let e = self.edit.as_ref()?;
        let sample = if e.text.is_empty() { "M" } else { &e.text };
        let (w, h) = text::measure(&self.fonts.bold, sample, e.size);
        let pad = self.px(4.0);
        Some(Rect::new(
            e.pos.0 as i32 - pad,
            e.pos.1 as i32 - pad,
            (e.pos.0 + w).ceil() as i32 + pad + 2,
            (e.pos.1 + h).ceil() as i32 + pad,
        ))
    }

    fn ensure_comp(&mut self) {
        let Some(sel) = self.sel else { return };
        self.ensure_committed();
        if self.comp_ok {
            return;
        }
        let (sw, sh) = (sel.w(), sel.h());
        let (ox, oy) = (sel.l as f32, sel.t as f32);
        let edit_rect = self.edit_rect().map(|r| r.offset(-sel.l, -sel.t));
        let edit_outline = self.edit_outline();
        self.comp.clear();
        self.comp.extend_from_slice(&self.committed);
        if let Some(cur) = &self.current {
            annotate::render_one(&mut self.comp, sw, sh, cur, ox, oy, &self.fonts);
        }
        if let (Some(e), Some(er)) = (&self.edit, edit_rect) {
            let shape = Shape::Text { pos: e.pos, text: e.text.clone(), color: e.color, size: e.size, outline: edit_outline };
            annotate::render_one(&mut self.comp, sw, sh, &shape, ox, oy, &self.fonts);
            let mut cv = Canvas::new(&mut self.comp, sw, sh);
            cv.dashed_rect(er, Rgba(255, 255, 255, 190));
            if self.caret_on {
                let (_, lh) = text::line_metrics(&self.fonts.bold, e.size);
                let last = e.text.rsplit('\n').next().unwrap_or("");
                let lines = e.text.split('\n').count().max(1);
                let x = e.pos.0 - ox + text::line_width(&self.fonts.bold, last, e.size) + 1.0;
                let y = e.pos.1 - oy + lh * (lines - 1) as f32;
                let cw = (self.scale.round() as i32).max(2);
                cv.fill_px(Rect::xywh(x as i32, y as i32, cw, lh as i32), e.color);
            }
        }
        self.comp_ok = true;
    }

    /// Composes the scene into the back buffer for the given rectangles.
    fn render(&mut self, rects: &[Rect]) {
        let (w, h) = (self.w, self.h);
        let back = unsafe { std::slice::from_raw_parts_mut(self.back.bits, w as usize * h as usize * 4) };
        for rc in rects {
            let rc = rc.intersect(&self.bounds());
            if rc.is_empty() {
                continue;
            }
            blit(&self.dim, w, 0, 0, back, w, rc);
            if let Some(sel) = self.sel {
                self.ensure_comp();
                let i = rc.intersect(&sel);
                if !i.is_empty() {
                    blit(&self.comp, sel.w(), sel.l, sel.t, back, w, i);
                }
            } else if let Some(hw) = self.hover_rect() {
                let i = rc.intersect(&hw);
                if !i.is_empty() {
                    blit(&self.shot, w, 0, 0, back, w, i);
                }
            }
        }
        let mut cv = Canvas::new(back, w, h);
        self.draw_ui(&mut cv);
    }

    fn on_paint(&mut self) {
        unsafe {
            let rgn = CreateRectRgn(0, 0, 0, 0);
            GetUpdateRgn(self.hwnd, rgn, FALSE);
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let hdc = BeginPaint(self.hwnd, &mut ps);
            let rects = region_rects(rgn, Rect::from_win(&ps.rcPaint));
            DeleteObject(rgn);
            self.render(&rects);
            for r in rects {
                let r = r.intersect(&self.bounds());
                if !r.is_empty() {
                    BitBlt(hdc, r.l, r.t, r.w(), r.h(), self.back.dc, r.l, r.t, SRCCOPY);
                }
            }
            EndPaint(self.hwnd, &ps);
        }
    }

    // ----- UI drawing -------------------------------------------------------

    fn draw_ui(&self, cv: &mut Canvas) {
        if let Some(sel) = self.sel {
            self.draw_frame(cv, sel, !self.gif_mode);
            let (lr, lt) = self.size_label(sel);
            self.draw_label(cv, lr, &lt, self.pxf(12.0));
            if let Some(lay) = self.layout() {
                self.draw_bars(cv, &lay);
            }
        } else if let Some(hw) = self.hover_rect() {
            self.draw_frame(cv, hw, false);
            let (lr, lt) = self.size_label(hw);
            self.draw_label(cv, lr, &lt, self.pxf(12.0));
        }
        if let Some((r, t)) = self.hint() {
            cv.fill_rrect(r, self.pxf(8.0), LABEL_BG);
            let fpx = self.pxf(13.0);
            let tw = text::line_width(&self.fonts.ui, t, fpx);
            let (_, lh) = text::line_metrics(&self.fonts.ui, fpx);
            let (cx, cy) = r.center();
            cv.text(cx as f32 - tw / 2.0, cy as f32 - lh / 2.0, t, fpx, LABEL_FG, &self.fonts.ui);
        }
        if let Some(r) = self.magnifier_rect() {
            self.draw_magnifier(cv, r);
        }
        if let (Some(r), Some(t)) = (self.flash_rect(), &self.flash) {
            self.draw_label(cv, r, t, self.pxf(12.0));
        }
    }

    fn draw_label(&self, cv: &mut Canvas, r: Rect, t: &str, fpx: f32) {
        cv.fill_rrect(r, self.pxf(4.0), LABEL_BG);
        let (_, lh) = text::line_metrics(&self.fonts.ui, fpx);
        let y = r.t as f32 + (r.h() as f32 - lh) / 2.0;
        cv.text(r.l as f32 + self.pxf(7.0), y, t, fpx, LABEL_FG, &self.fonts.ui);
    }

    fn draw_frame(&self, cv: &mut Canvas, r: Rect, handles: bool) {
        let t = (self.scale.round() as i32).max(1);
        frame_px(cv, r.inflate(t), t, ACCENT);
        if !handles {
            return;
        }
        let hs = self.handle_size();
        let (cx, cy) = r.center();
        for (x, y) in [(r.l, r.t), (cx, r.t), (r.r, r.t), (r.r, cy), (r.r, r.b), (cx, r.b), (r.l, r.b), (r.l, cy)] {
            let hr = Rect::xywh(x - hs / 2, y - hs / 2, hs, hs);
            cv.fill_px(hr, WHITE);
            frame_px(cv, hr, 1, ACCENT);
        }
    }

    fn draw_bars(&self, cv: &mut Canvas, lay: &Layout) {
        let rad = self.pxf(7.0);
        cv.fill_rrect(lay.vbar, rad, BAR_BG);
        cv.fill_rrect(lay.hbar, rad, BAR_BG);
        for s in &lay.seps {
            cv.fill_px(*s, Rgba(255, 255, 255, 40));
        }
        if let Some(p) = lay.palette {
            cv.fill_rrect(p, rad, BAR_BG);
        }
        let icon = self.pxf(20.0);
        let pressed = match self.drag {
            Drag::Button(b) => Some(b),
            _ => None,
        };
        for &(btn, r, _) in &lay.buttons {
            let hovered = self.hover_btn == Some(btn) && pressed.is_none_or(|p| p == btn);
            let inner = r.inflate(-self.px(3.0));
            let (cx, cy) = ((r.l + r.r) as f32 / 2.0, (r.t + r.b) as f32 / 2.0);
            let rr = self.pxf(5.0);
            match btn {
                Btn::Swatch(i) => {
                    let c = PALETTE[i];
                    let radius = inner.w() as f32 / 2.0;
                    if c == self.prefs.color {
                        cv.stroke_circle(cx, cy, radius - self.pxf(1.0), self.pxf(2.0), WHITE);
                    } else if hovered {
                        cv.stroke_circle(cx, cy, radius - self.pxf(1.0), self.pxf(1.5), Rgba(255, 255, 255, 110));
                    }
                    cv.fill_circle(cx, cy, radius - self.pxf(4.0), c);
                    if c == Rgba(0, 0, 0, 255) {
                        cv.stroke_circle(cx, cy, radius - self.pxf(4.0), 1.0, Rgba(255, 255, 255, 70));
                    }
                }
                Btn::Color => {
                    if hovered || self.palette_open {
                        cv.fill_rrect(inner, rr, HOVER_BG);
                    }
                    // The ring shows the outline that new drawings will get.
                    self.draw_outline_disc(cv, self.prefs.outline, cx, cy, self.pxf(9.5), false);
                    cv.fill_circle(cx, cy, self.pxf(6.5), self.prefs.color);
                }
                Btn::Outline(m) => {
                    if m == OutlineMode::Off {
                        // Section label above the row of outline modes.
                        let fpx = self.pxf(11.5);
                        let (_, lh) = text::line_metrics(&self.fonts.ui, fpx);
                        let (lx, ly) = (r.l as f32 + self.pxf(4.0), r.t as f32 - lh - self.pxf(3.0));
                        cv.text(lx, ly, "Outline", fpx, Rgba(160, 163, 170, 255), &self.fonts.ui);
                    }
                    let radius = (r.w().min(r.h()) as f32 / 2.0 - self.pxf(4.5)).max(4.0);
                    if self.prefs.outline == m {
                        cv.stroke_circle(cx, cy, radius + self.pxf(2.5), self.pxf(2.0), WHITE);
                    } else if hovered {
                        cv.stroke_circle(cx, cy, radius + self.pxf(2.5), self.pxf(1.5), Rgba(255, 255, 255, 110));
                    }
                    self.draw_outline_disc(cv, m, cx, cy, radius, true);
                }
                _ => {
                    let active = matches!(btn, Btn::Tool(t) if self.tool == Some(t));
                    let enabled = match btn {
                        Btn::Undo => self.can_undo(),
                        Btn::Redo => !self.future.is_empty(),
                        _ => true,
                    };
                    if active {
                        cv.fill_rrect(inner, rr, ACCENT);
                    } else if hovered && enabled {
                        cv.fill_rrect(inner, rr, HOVER_BG);
                    }
                    let fg = if !enabled {
                        ICON_DIM
                    } else if active {
                        WHITE
                    } else {
                        ICON_FG
                    };
                    if let Some(ic) = btn.icon() {
                        let size = if btn == Btn::Custom { icon * 0.8 } else { icon };
                        cv.icon(ic, cx, cy, size, fg, &self.fonts);
                    }
                }
            }
        }
        if let Some((r, t)) = self.tooltip(lay) {
            self.draw_label(cv, r, t, self.pxf(12.0));
        }
    }

    /// A disc standing for an outline mode. In the palette (`picker`), the
    /// custom mode also shows a "+" since it opens the color picker.
    fn draw_outline_disc(&self, cv: &mut Canvas, m: OutlineMode, cx: f32, cy: f32, r: f32, picker: bool) {
        let faint = Rgba(255, 255, 255, 90);
        match m {
            OutlineMode::Off => {
                cv.stroke_circle(cx, cy, r - 0.75, 1.5, faint);
                if picker {
                    let k = r * 0.62;
                    cv.line(cx - k, cy + k, cx + k, cy - k, 1.5, faint);
                }
            }
            OutlineMode::Auto => {
                cv.fill_circle(cx, cy, r, WHITE);
                cv.fill_half_circle(cx, cy, r, annotate::OUTLINE_BLACK.alpha(255));
                cv.stroke_circle(cx, cy, r, 1.0, faint);
            }
            OutlineMode::White => cv.fill_circle(cx, cy, r, WHITE),
            OutlineMode::Black => {
                cv.fill_circle(cx, cy, r, annotate::OUTLINE_BLACK.alpha(255));
                cv.stroke_circle(cx, cy, r, 1.0, faint);
            }
            OutlineMode::Custom => {
                let c = self.prefs.outline_custom;
                cv.fill_circle(cx, cy, r, c);
                cv.stroke_circle(cx, cy, r, 1.0, faint);
                if picker {
                    let fg = if annotate::luma(c) > 150 { Rgba(20, 20, 22, 255) } else { WHITE };
                    cv.icon(Icon::Plus, cx, cy, r * 1.4, fg, &self.fonts);
                }
            }
        }
    }

    fn draw_magnifier(&self, cv: &mut Canvas, r: Rect) {
        let cell = self.px(8.0).max(5);
        let size = MAG_CELLS * cell;
        let half = MAG_CELLS / 2;
        let (mx, my) = self.mouse;
        let lens = Rect::xywh(r.l, r.t, size, size);
        cv.fill_px(lens.inflate(2), Rgba(0, 0, 0, 255));
        cv.fill_px(lens.inflate(1), WHITE);
        let pixel = |x: i32, y: i32| -> Rgba {
            if x < 0 || y < 0 || x >= self.w || y >= self.h {
                return Rgba(0, 0, 0, 255);
            }
            let k = ((y * self.w + x) * 4) as usize;
            Rgba(self.shot[k + 2], self.shot[k + 1], self.shot[k], 255)
        };
        for j in 0..MAG_CELLS {
            for i in 0..MAG_CELLS {
                let c = pixel(mx - half + i, my - half + j);
                cv.fill_px(Rect::xywh(lens.l + i * cell, lens.t + j * cell, cell, cell), c);
            }
        }
        let (cxp, cyp) = (lens.l + half * cell, lens.t + half * cell);
        cv.fill_px(Rect::new(lens.l, cyp, cxp, cyp + cell), Rgba(10, 132, 255, 60));
        cv.fill_px(Rect::new(cxp + cell, cyp, lens.r, cyp + cell), Rgba(10, 132, 255, 60));
        cv.fill_px(Rect::new(cxp, lens.t, cxp + cell, cyp), Rgba(10, 132, 255, 60));
        cv.fill_px(Rect::new(cxp, cyp + cell, cxp + cell, lens.b), Rgba(10, 132, 255, 60));
        let cc = Rect::xywh(cxp, cyp, cell, cell);
        frame_px(cv, cc.inflate(1), 1, Rgba(0, 0, 0, 255));
        frame_px(cv, cc, 1, WHITE);

        let lab = Rect::new(r.l - 2, lens.b + self.px(5.0), r.r + 2, r.b);
        cv.fill_rrect(lab, self.pxf(4.0), LABEL_BG);
        let c = pixel(mx, my);
        let fpx = self.pxf(11.0);
        let (_, lh) = text::line_metrics(&self.fonts.ui, fpx);
        let tx = lab.l as f32 + self.pxf(7.0);
        let line1 = match self.drag {
            Drag::NewSel { .. } | Drag::Resize { .. } => match self.sel {
                Some(s) => format!("{} × {}", s.w(), s.h()),
                None => String::new(),
            },
            _ => format!("X {}   Y {}", mx + self.ox, my + self.oy),
        };
        let top = lab.t as f32 + (lab.h() as f32 - 2.0 * lh) / 2.0;
        cv.text(tx, top, &line1, fpx, LABEL_FG, &self.fonts.ui);
        let sw = self.px(9.0);
        let sy = (top + lh * 1.5) as i32 - sw / 2;
        cv.fill_px(Rect::xywh(tx as i32, sy, sw, sw), WHITE);
        cv.fill_px(Rect::xywh(tx as i32 + 1, sy + 1, sw - 2, sw - 2), c);
        let hex = format!("#{:02X}{:02X}{:02X}", c.0, c.1, c.2);
        cv.text(tx + sw as f32 + self.pxf(5.0), top + lh, &hex, fpx, LABEL_FG, &self.fonts.ui);
    }

    // ----- editing ----------------------------------------------------------

    fn can_undo(&self) -> bool {
        !self.history.is_empty() || self.edit.is_some()
    }

    /// Turns the outline for new drawings (and the text being typed) on/off.
    fn set_outline(&mut self, m: OutlineMode) {
        self.prefs.outline = m;
        if m != OutlineMode::Off {
            self.prefs.outline_on = m;
        }
        self.comp_ok = false; // text being typed follows the setting
        let name = match m {
            OutlineMode::Off => "off",
            OutlineMode::Auto => "automatic",
            OutlineMode::White => "white",
            OutlineMode::Black => "black",
            OutlineMode::Custom => "custom",
        };
        self.show_flash(format!("Outline: {name}"));
        self.invalidate();
    }

    /// The outline for a drawing of `color` covering `area`, per the mode.
    fn resolve_outline(&self, color: Rgba, area: Rect) -> Option<Rgba> {
        match self.prefs.outline {
            OutlineMode::Off => None,
            OutlineMode::Auto => Some(annotate::auto_outline(color, self.background_luma(area))),
            OutlineMode::White => Some(annotate::OUTLINE_WHITE),
            OutlineMode::Black => Some(annotate::OUTLINE_BLACK),
            OutlineMode::Custom => Some(self.prefs.outline_custom.alpha(240)),
        }
    }

    /// Average brightness of the screenshot in an area (sampled).
    fn background_luma(&self, area: Rect) -> u8 {
        let r = area.intersect(&self.bounds());
        if r.is_empty() {
            return 128;
        }
        let step = ((r.area() / 4000) as f64).sqrt().ceil().max(1.0) as i32;
        let (mut sum, mut n) = (0u64, 0u64);
        for y in (r.t..r.b).step_by(step as usize) {
            for x in (r.l..r.r).step_by(step as usize) {
                let i = ((y * self.w + x) * 4) as usize;
                sum += annotate::luma(Rgba(self.shot[i + 2], self.shot[i + 1], self.shot[i], 255)) as u64;
                n += 1;
            }
        }
        (sum / n.max(1)) as u8
    }

    /// Re-picks the outline of the shape being drawn for where it now is.
    fn refresh_outline(&mut self) {
        let Some(s) = &self.current else { return };
        let (Some(color), Some((lo, hi))) = (s.color(), s.extent()) else { return };
        let pad = self.px(8.0);
        let area = Rect::new(lo.0 as i32 - pad, lo.1 as i32 - pad, hi.0 as i32 + pad + 1, hi.1 as i32 + pad + 1);
        let outline = self.resolve_outline(color, area);
        if let Some(s) = &mut self.current {
            s.set_outline(outline);
        }
    }

    /// The outline for the text being typed.
    fn edit_outline(&self) -> Option<Rgba> {
        match (&self.edit, self.edit_rect()) {
            (Some(e), Some(r)) => self.resolve_outline(e.color, r),
            _ => None,
        }
    }

    fn set_color(&mut self, c: Rgba) {
        self.prefs.color = c;
        if let Some(e) = &mut self.edit {
            e.color = c;
            self.comp_ok = false;
        }
        self.invalidate();
    }

    /// Saves the current shapes for undo; call before changing them.
    fn snapshot(&mut self) {
        self.history.push(self.shapes.clone());
        self.future.clear();
    }

    fn push_shape(&mut self, s: Shape) {
        if !s.is_degenerate() {
            self.snapshot();
            self.shapes.push(s);
            self.committed_ok = false;
        }
        self.comp_ok = false;
    }

    /// Makes a pixelate shape's final, safe pixels (text scrambled via OCR)
    /// from the clean screenshot. Frozen with the shape, so GIFs can't reveal
    /// changes happening underneath either.
    fn finish_pixelate(&mut self, s: &mut Shape) {
        if s.is_degenerate() {
            return;
        }
        if let Shape::Pixelate { a, b, block, seed, patch } = s {
            unsafe { SetCursor(LoadCursorW(null_mut(), IDC_WAIT)) };
            *patch = annotate::make_patch(&self.shot, self.w, self.h, *a, *b, *block, *seed, &self.fonts);
        }
    }

    fn commit_edit(&mut self) {
        let outline = self.edit_outline();
        let Some(e) = self.edit.take() else { return };
        unsafe { KillTimer(self.hwnd, TIMER_CARET) };
        let shape = Shape::Text { pos: e.pos, text: e.text, color: e.color, size: e.size, outline };
        match e.original {
            None => self.push_shape(shape),
            // Re-edited text: the snapshot was taken when editing began.
            Some((index, original)) => {
                let index = index.min(self.shapes.len());
                if same_text(&shape, &original) {
                    self.history.pop(); // nothing changed, so nothing to undo
                    self.shapes.insert(index, original);
                } else if !shape.is_degenerate() {
                    self.shapes.insert(index, shape);
                } // emptied: the text is deleted (undo brings it back)
                self.committed_ok = false;
                self.comp_ok = false;
            }
        }
    }

    fn undo(&mut self) {
        if let Some(e) = self.edit.take() {
            unsafe { KillTimer(self.hwnd, TIMER_CARET) };
            // Abandoning a re-edit puts the original text back.
            if e.original.is_some() {
                if let Some(prev) = self.history.pop() {
                    self.shapes = prev;
                    self.committed_ok = false;
                }
            }
            self.comp_ok = false;
        } else if let Some(prev) = self.history.pop() {
            self.future.push(std::mem::replace(&mut self.shapes, prev));
            self.committed_ok = false;
        }
    }

    fn redo(&mut self) {
        if let Some(next) = self.future.pop() {
            self.history.push(std::mem::replace(&mut self.shapes, next));
            self.committed_ok = false;
        }
    }

    /// The topmost drawing under a point (only texts if `texts_only`).
    fn shape_under(&self, x: i32, y: i32, texts_only: bool) -> Option<usize> {
        let p = (x as f32 + 0.5, y as f32 + 0.5);
        let tol = self.pxf(5.0);
        self.shapes
            .iter()
            .enumerate()
            .rev()
            .find(|(_, s)| (!texts_only || matches!(s, Shape::Text { .. })) && s.hit(p, tol, &self.fonts))
            .map(|(i, _)| i)
    }

    /// The drawing a press here would grab: any drawing with no tool
    /// selected, only texts with the text tool; drawing tools draw instead.
    fn shape_at(&self, x: i32, y: i32) -> Option<usize> {
        match self.tool {
            None => self.shape_under(x, y, false),
            Some(Tool::Text) => self.shape_under(x, y, true),
            Some(_) => None,
        }
    }

    /// Whether the point is over something a drag would move.
    fn over_shape(&self, x: i32, y: i32) -> bool {
        self.edit_rect().is_some_and(|r| r.contains(x, y)) || self.shape_at(x, y).is_some()
    }

    /// Shows a short message next to the cursor.
    fn show_flash(&mut self, msg: String) {
        self.flash = Some(msg);
        unsafe { SetTimer(self.hwnd, TIMER_FLASH, 1100, None) };
        self.invalidate();
    }

    /// The screenshot's color under the cursor, as `#RRGGBB`.
    fn color_under_cursor(&self) -> String {
        let (x, y) = (self.mouse.0.clamp(0, self.w - 1), self.mouse.1.clamp(0, self.h - 1));
        let k = ((y * self.w + x) * 4) as usize;
        format!("#{:02X}{:02X}{:02X}", self.shot[k + 2], self.shot[k + 1], self.shot[k])
    }

    /// The next step number: one more than the highest so far.
    fn next_step(&self) -> u32 {
        self.shapes
            .iter()
            .filter_map(|s| match s {
                Shape::Step { n, .. } => Some(*n),
                _ => None,
            })
            .max()
            .unwrap_or(0)
            + 1
    }

    /// Places the next step number; keeping the button down drags it.
    fn place_step(&mut self, x: i32, y: i32) {
        let size = self.prefs.step * self.scale;
        let r = (size / 2.0).ceil() as i32;
        let outline = self.resolve_outline(self.prefs.color, Rect::new(x - r, y - r, x + r, y + r));
        let n = self.next_step();
        self.push_shape(Shape::Step { pos: (x as f32, y as f32), n, color: self.prefs.color, size, outline });
        let index = self.shapes.len() - 1;
        self.drag = Drag::MoveShape { index, sx: x, sy: y, dx: 0, dy: 0, moved: true };
    }

    /// Reopens a placed text for editing.
    fn edit_text(&mut self, index: usize) {
        let Some(Shape::Text { pos, text, color, size, .. }) = self.shapes.get(index).cloned() else { return };
        self.snapshot();
        let original = self.shapes.remove(index);
        self.edit = Some(TextEdit { pos, text, color, size, original: Some((index, original)) });
        self.caret_on = true;
        self.committed_ok = false;
        self.comp_ok = false;
        unsafe { SetTimer(self.hwnd, TIMER_CARET, 530, None) };
    }

    fn start_text(&mut self, x: i32, y: i32) {
        let size = self.prefs.text * self.scale;
        let (_, lh) = text::line_metrics(&self.fonts.bold, size);
        self.edit = Some(TextEdit {
            pos: (x as f32, y as f32 - lh / 2.0),
            text: String::new(),
            color: self.prefs.color,
            size,
            original: None,
        });
        self.caret_on = true;
        self.comp_ok = false;
        unsafe { SetTimer(self.hwnd, TIMER_CARET, 530, None) };
    }

    fn start_shape(&mut self, tool: Tool, x: i32, y: i32) {
        let p = (x as f32 + 0.5, y as f32 + 0.5);
        let color = self.prefs.color;
        let width = self.prefs.stroke * self.scale;
        let outline = None; // picked by refresh_outline below
        self.current = Some(match tool {
            Tool::Pen => Shape::Pen { pts: vec![p], color, width, outline },
            Tool::Marker => Shape::Marker { pts: vec![p], color, width: self.prefs.marker * self.scale },
            Tool::Line => Shape::Line { a: p, b: p, color, width, outline },
            Tool::Arrow => Shape::Arrow { a: p, b: p, color, width, outline },
            Tool::Rect => Shape::Rect { a: p, b: p, color, width, outline },
            Tool::Ellipse => Shape::Ellipse { a: p, b: p, color, width, outline },
            Tool::Pixelate => Shape::Pixelate {
                a: (x as f32, y as f32),
                b: (x as f32, y as f32),
                block: self.prefs.pixel * self.scale,
                seed: annotate::new_seed(),
                patch: None,
            },
            Tool::Text | Tool::Step => return,
        });
        self.refresh_outline();
        self.comp_ok = false;
    }

    fn update_shape(&mut self, x: i32, y: i32, constrain: bool) {
        let p = (x as f32 + 0.5, y as f32 + 0.5);
        match &mut self.current {
            Some(Shape::Pen { pts, .. }) | Some(Shape::Marker { pts, .. }) => {
                if pts.last().is_none_or(|l| annotate::dist(*l, p) >= 1.0) {
                    pts.push(p);
                }
            }
            Some(Shape::Line { a, b, .. }) | Some(Shape::Arrow { a, b, .. }) => {
                *b = if constrain { annotate::snap_angle(*a, p) } else { p };
            }
            Some(Shape::Rect { a, b, .. }) | Some(Shape::Ellipse { a, b, .. }) => {
                *b = if constrain { annotate::snap_square(*a, p) } else { p };
            }
            Some(Shape::Pixelate { a, b, .. }) => {
                let q = (x as f32, y as f32);
                *b = if constrain { annotate::snap_square(*a, q) } else { q };
            }
            _ => {}
        }
        self.refresh_outline();
        self.comp_ok = false;
    }

    fn final_image(&mut self) -> Option<(i32, i32, Vec<u8>)> {
        self.commit_edit();
        self.current = None;
        let sel = self.sel?;
        self.ensure_committed();
        Some((sel.w(), sel.h(), self.committed.clone()))
    }

    fn gif_request(&mut self) -> Option<GifRequest> {
        self.commit_edit();
        let sel = self.sel?;
        let mut shapes = self.shapes.clone();
        for s in &mut shapes {
            s.translate(-(sel.l as f32), -(sel.t as f32));
        }
        Some(GifRequest { region: sel.offset(self.ox, self.oy), shapes })
    }

    // ----- input ------------------------------------------------------------

    fn handle(&mut self, msg: u32, wp: WPARAM, lp: LPARAM) -> (Option<LRESULT>, Option<Action>) {
        match msg {
            WM_PAINT => {
                self.on_paint();
                (Some(0), None)
            }
            WM_ERASEBKGND => (Some(1), None),
            WM_SETCURSOR => {
                if (lp & 0xFFFF) as u32 == HTCLIENT {
                    self.update_cursor();
                    (Some(1), None)
                } else {
                    (None, None)
                }
            }
            WM_MOUSEMOVE => {
                let (x, y) = lparam_xy(lp);
                self.on_move(x, y, wp & MK_SHIFT as usize != 0);
                (Some(0), None)
            }
            WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => {
                let (x, y) = lparam_xy(lp);
                (Some(0), self.on_down(x, y, msg == WM_LBUTTONDBLCLK))
            }
            WM_LBUTTONUP => {
                let (x, y) = lparam_xy(lp);
                (Some(0), self.on_up(x, y))
            }
            WM_RBUTTONDOWN => (Some(0), self.on_right()),
            WM_MOUSEWHEEL => {
                self.on_wheel(((wp >> 16) & 0xFFFF) as i16 as i32);
                (Some(0), None)
            }
            WM_KEYDOWN => (Some(0), self.on_key(wp as u16)),
            WM_CHAR => {
                self.on_char(wp as u16);
                (Some(0), None)
            }
            WM_TIMER => {
                self.on_timer(wp);
                (Some(0), None)
            }
            // Spanning monitors with different DPIs must not resize us.
            WM_DPICHANGED => (Some(0), None),
            WM_CLOSE => (Some(0), Some(Action::Close)),
            _ => (None, None),
        }
    }

    fn update_cursor(&self) {
        let (x, y) = self.mouse;
        let handle_cursor = |h: Handle| match h {
            Handle::N | Handle::S => IDC_SIZENS,
            Handle::E | Handle::W => IDC_SIZEWE,
            Handle::NW | Handle::SE => IDC_SIZENWSE,
            Handle::NE | Handle::SW => IDC_SIZENESW,
        };
        let id = match self.drag {
            Drag::Move { .. } | Drag::MoveShape { .. } | Drag::MoveEdit { .. } => IDC_SIZEALL,
            Drag::Resize { h, .. } => handle_cursor(h),
            Drag::Draw | Drag::NewSel { .. } | Drag::Pending => IDC_CROSS,
            Drag::Button(_) => IDC_HAND,
            Drag::None => {
                if self.hit_button(x, y).is_some() {
                    IDC_HAND
                } else if self.on_ui(x, y) {
                    IDC_ARROW
                } else if let Some(sel) = self.sel {
                    if let Some(h) = self.hit_handle(x, y) {
                        handle_cursor(h)
                    } else if self.over_shape(x, y) {
                        IDC_SIZEALL
                    } else if let Some(t) = self.tool {
                        if t == Tool::Text {
                            IDC_IBEAM
                        } else {
                            IDC_CROSS
                        }
                    } else if sel.contains(x, y) && !self.gif_mode {
                        IDC_SIZEALL
                    } else {
                        IDC_CROSS
                    }
                } else {
                    IDC_CROSS
                }
            }
        };
        unsafe { SetCursor(LoadCursorW(null_mut(), id)) };
    }

    fn on_move(&mut self, x: i32, y: i32, shift: bool) {
        let (x, y) = (x.clamp(0, self.w - 1), y.clamp(0, self.h - 1));
        self.mouse = (x, y);
        match self.drag {
            Drag::None => {
                self.hover_btn = self.hit_button(x, y);
                if self.sel.is_none() {
                    self.hover_win = self.window_at(x, y);
                }
            }
            Drag::Pending => {
                let th = self.px(4.0).max(3);
                if (x - self.down.0).abs() > th || (y - self.down.1).abs() > th {
                    let (ax, ay) = self.down;
                    self.drag = Drag::NewSel { ax, ay };
                    self.set_sel(Some(Rect::from_points(ax, ay, x, y)));
                }
            }
            Drag::NewSel { ax, ay } => self.set_sel(Some(Rect::from_points(ax, ay, x, y))),
            Drag::Move { orig, sx, sy } => {
                let dx = (x - sx).clamp(-orig.l, self.w - orig.r);
                let dy = (y - sy).clamp(-orig.t, self.h - orig.b);
                self.set_sel(Some(orig.offset(dx, dy)));
            }
            Drag::Resize { h, orig, sx, sy } => {
                let (dx, dy) = (x - sx, y - sy);
                let (mut l, mut t, mut r, mut b) = (orig.l, orig.t, orig.r, orig.b);
                match h {
                    Handle::N => t += dy,
                    Handle::S => b += dy,
                    Handle::W => l += dx,
                    Handle::E => r += dx,
                    Handle::NW => (l, t) = (l + dx, t + dy),
                    Handle::NE => (r, t) = (r + dx, t + dy),
                    Handle::SW => (l, b) = (l + dx, b + dy),
                    Handle::SE => (r, b) = (r + dx, b + dy),
                }
                let mut s = Rect::from_points(l, t, r, b).intersect(&self.bounds());
                s.l = s.l.min(self.w - 1);
                s.t = s.t.min(self.h - 1);
                s.r = s.r.max(s.l + 1);
                s.b = s.b.max(s.t + 1);
                self.set_sel(Some(s));
            }
            Drag::MoveShape { index, sx, sy, dx, dy, moved } => {
                let (nx, ny) = (x - sx, y - sy);
                let th = self.px(3.0).max(2);
                if moved || nx.abs() > th || ny.abs() > th {
                    if !moved {
                        self.snapshot();
                        // Pixelate pixels are remade where it lands.
                        if let Some(Shape::Pixelate { patch, .. }) = self.shapes.get_mut(index) {
                            *patch = None;
                        }
                    }
                    if let Some(s) = self.shapes.get_mut(index) {
                        s.translate((nx - dx) as f32, (ny - dy) as f32);
                        self.committed_ok = false;
                    }
                    self.drag = Drag::MoveShape { index, sx, sy, dx: nx, dy: ny, moved: true };
                }
            }
            Drag::MoveEdit { sx, sy, orig } => {
                if let Some(e) = &mut self.edit {
                    e.pos = (orig.0 + (x - sx) as f32, orig.1 + (y - sy) as f32);
                    self.comp_ok = false;
                }
            }
            Drag::Draw => self.update_shape(x, y, shift),
            Drag::Button(_) => self.hover_btn = self.hit_button(x, y),
        }
        self.update_cursor();
        self.invalidate();
    }

    fn on_down(&mut self, x: i32, y: i32, dbl: bool) -> Option<Action> {
        self.mouse = (x, y);
        self.down = (x, y);
        unsafe { SetCapture(self.hwnd) };

        if dbl
            && self.tool.is_none()
            && self.edit.is_none()
            && self.hit_button(x, y).is_none()
            && self.shape_at(x, y).is_none()
            && self.sel.is_some_and(|s| s.contains(x, y))
        {
            return Some(Action::Copy);
        }
        if let Some(b) = self.hit_button(x, y) {
            self.drag = Drag::Button(b);
            self.invalidate();
            return None;
        }
        if self.palette_open {
            self.palette_open = false;
            self.invalidate();
            return None;
        }
        if self.on_ui(x, y) {
            return None;
        }
        if let Some(e) = &self.edit {
            if self.edit_rect().is_some_and(|r| r.contains(x, y)) {
                self.drag = Drag::MoveEdit { sx: x, sy: y, orig: e.pos };
                self.update_cursor();
                return None;
            }
            self.commit_edit();
        }
        if let Some(sel) = self.sel {
            if let Some(h) = self.hit_handle(x, y) {
                self.drag = Drag::Resize { h, orig: sel, sx: x, sy: y };
            } else if let Some(index) = self.shape_at(x, y) {
                self.drag = Drag::MoveShape { index, sx: x, sy: y, dx: 0, dy: 0, moved: false };
            } else if let Some(tool) = self.tool {
                if tool == Tool::Text {
                    self.start_text(x, y);
                } else if tool == Tool::Step {
                    self.place_step(x, y);
                } else {
                    self.start_shape(tool, x, y);
                    self.drag = Drag::Draw;
                }
            } else if sel.contains(x, y) && !self.gif_mode {
                self.drag = Drag::Move { orig: sel, sx: x, sy: y };
            } else {
                self.drag = Drag::Pending;
            }
        } else {
            self.drag = Drag::Pending;
        }
        self.update_cursor();
        self.invalidate();
        None
    }

    fn on_up(&mut self, x: i32, y: i32) -> Option<Action> {
        let (x, y) = (x.clamp(0, self.w - 1), y.clamp(0, self.h - 1));
        unsafe { ReleaseCapture() };
        let drag = std::mem::replace(&mut self.drag, Drag::None);
        let mut action = None;
        match drag {
            Drag::Pending => {
                // A click: snap to the window (or monitor) under the cursor.
                if let Some(r) = self.window_at(x, y) {
                    self.set_sel(Some(r));
                    self.hover_win = None;
                    if self.gif_mode {
                        action = Some(Action::Gif);
                    }
                }
            }
            Drag::NewSel { .. } => {
                if let Some(s) = self.sel {
                    if s.w() < 3 || s.h() < 3 {
                        self.set_sel(None);
                        self.hover_win = self.window_at(x, y);
                    } else if self.gif_mode {
                        action = Some(Action::Gif);
                    }
                }
            }
            Drag::Draw => {
                if let Some(mut s) = self.current.take() {
                    self.finish_pixelate(&mut s);
                    self.push_shape(s);
                }
            }
            Drag::Button(b) => {
                if self.hit_button(x, y) == Some(b) {
                    action = self.activate(b);
                }
            }
            // A click on a placed text (no drag) edits it.
            Drag::MoveShape { index, moved: false, .. } => {
                if matches!(self.shapes.get(index), Some(Shape::Text { .. })) {
                    self.edit_text(index);
                }
            }
            Drag::MoveShape { index, moved: true, .. } => {
                if let Some(mut s) = self.shapes.get(index).filter(|s| matches!(s, Shape::Pixelate { .. })).cloned() {
                    self.finish_pixelate(&mut s);
                    self.shapes[index] = s;
                    self.committed_ok = false;
                }
            }
            Drag::MoveEdit { .. } | Drag::Move { .. } | Drag::Resize { .. } | Drag::None => {}
        }
        self.hover_btn = self.hit_button(x, y);
        self.update_cursor();
        self.invalidate();
        action
    }

    fn activate(&mut self, b: Btn) -> Option<Action> {
        match b {
            Btn::Tool(t) => {
                self.commit_edit();
                self.tool = if self.tool == Some(t) { None } else { Some(t) };
                self.palette_open = false;
                None
            }
            Btn::Color => {
                self.palette_open = !self.palette_open;
                None
            }
            Btn::Swatch(i) => {
                self.palette_open = false;
                self.set_color(PALETTE[i]);
                None
            }
            Btn::Custom => {
                self.palette_open = false;
                Some(Action::ChooseColor)
            }
            Btn::Outline(OutlineMode::Custom) => Some(Action::ChooseOutlineColor),
            Btn::Outline(m) => {
                self.set_outline(m);
                None
            }
            Btn::Undo => {
                self.undo();
                None
            }
            Btn::Redo => {
                self.redo();
                None
            }
            Btn::Gif => Some(Action::Gif),
            Btn::Copy => Some(Action::Copy),
            Btn::Save => Some(Action::Save),
            Btn::Close => Some(Action::Close),
        }
    }

    /// Right click steps back: cancel drawing → close palette → finish text →
    /// drop tool → drop selection → close.
    fn on_right(&mut self) -> Option<Action> {
        if matches!(self.drag, Drag::Draw) {
            self.current = None;
            self.drag = Drag::None;
            self.comp_ok = false;
            unsafe { ReleaseCapture() };
        } else if !matches!(self.drag, Drag::None) {
            return None;
        } else if self.palette_open {
            self.palette_open = false;
        } else if self.edit.is_some() {
            self.commit_edit();
        } else if self.tool.is_some() {
            self.tool = None;
        } else if self.sel.is_some() {
            self.set_sel(None);
            self.hover_win = self.window_at(self.mouse.0, self.mouse.1);
        } else {
            return Some(Action::Close);
        }
        self.update_cursor();
        self.invalidate();
        None
    }

    fn on_wheel(&mut self, delta: i32) {
        let dir = delta.signum() as f32;
        if dir == 0.0 {
            return;
        }
        let text_mode = self.edit.is_some() || self.tool == Some(Tool::Text);
        let (val, min, max, step) = if text_mode {
            (&mut self.prefs.text, 8.0, 120.0, 2.0)
        } else {
            match self.tool {
                Some(Tool::Marker) => (&mut self.prefs.marker, 4.0, 80.0, 2.0),
                Some(Tool::Step) => (&mut self.prefs.step, 16.0, 80.0, 2.0),
                Some(Tool::Pixelate) => (&mut self.prefs.pixel, 3.0, 60.0, 1.0),
                _ => (&mut self.prefs.stroke, 1.0, 40.0, 1.0),
            }
        };
        *val = (*val + step * dir).clamp(min, max);
        let v = *val;
        if let Some(e) = &mut self.edit {
            e.size = v * self.scale;
            self.comp_ok = false;
        }
        self.show_flash(format!("Size {v}"));
        self.invalidate();
    }

    fn on_timer(&mut self, id: usize) {
        match id {
            TIMER_CARET => {
                self.caret_on = !self.caret_on;
                self.comp_ok = false;
            }
            TIMER_FLASH => {
                unsafe { KillTimer(self.hwnd, TIMER_FLASH) };
                self.flash = None;
            }
            _ => {}
        }
        self.invalidate();
    }

    fn on_key(&mut self, vk: u16) -> Option<Action> {
        let ctrl = key_down(VK_CONTROL);
        let shift = key_down(VK_SHIFT);

        if self.edit.is_some() {
            match vk {
                VK_ESCAPE => self.commit_edit(),
                VK_RETURN if ctrl => self.commit_edit(),
                _ if ctrl && vk == b'V' as u16 => {
                    if let Some(t) = clipboard::get_text(self.hwnd) {
                        if let Some(e) = &mut self.edit {
                            e.text.push_str(&t.replace("\r\n", "\n").replace('\r', "\n"));
                            self.comp_ok = false;
                        }
                    }
                }
                _ if ctrl && vk == b'Z' as u16 => self.undo(),
                _ if ctrl && (vk == b'C' as u16 || vk == b'S' as u16) => {
                    self.commit_edit();
                    return self.on_key(vk);
                }
                _ => return None, // typed characters arrive as WM_CHAR
            }
            self.invalidate();
            return None;
        }

        let mut action = None;
        match vk {
            VK_ESCAPE => {
                if matches!(self.drag, Drag::Draw) {
                    return self.on_right();
                } else if self.palette_open {
                    self.palette_open = false;
                } else {
                    return Some(Action::Close);
                }
            }
            VK_RETURN if self.sel.is_some() => action = Some(Action::Copy),
            VK_LEFT | VK_RIGHT | VK_UP | VK_DOWN => {
                if let (Some(s), Drag::None) = (self.sel, self.drag) {
                    let step = if shift { 10 } else { 1 };
                    let (dx, dy) = match vk {
                        VK_LEFT => (-step, 0),
                        VK_RIGHT => (step, 0),
                        VK_UP => (0, -step),
                        _ => (0, step),
                    };
                    let r = if ctrl {
                        // Ctrl+arrows resize from the bottom-right corner.
                        Rect::new(s.l, s.t, (s.r + dx).max(s.l + 1), (s.b + dy).max(s.t + 1)).intersect(&self.bounds())
                    } else {
                        let dx = dx.clamp(-s.l, self.w - s.r);
                        let dy = dy.clamp(-s.t, self.h - s.b);
                        s.offset(dx, dy)
                    };
                    self.set_sel(Some(r));
                }
            }
            // Delete removes the drawing under the cursor.
            VK_DELETE | VK_BACK if matches!(self.drag, Drag::None) => {
                if let Some(i) = self.shape_under(self.mouse.0, self.mouse.1, false) {
                    self.snapshot();
                    self.shapes.remove(i);
                    self.committed_ok = false;
                }
            }
            _ if !ctrl && vk == b'C' as u16 => action = Some(Action::CopyColor),
            _ if ctrl => {
                let key = vk as u8;
                match key {
                    b'C' if self.sel.is_some() => action = Some(Action::Copy),
                    b'S' if self.sel.is_some() => action = Some(Action::Save),
                    b'Z' if shift => self.redo(),
                    b'Z' => self.undo(),
                    b'Y' => self.redo(),
                    b'A' => {
                        let m = self.monitor_at(self.mouse.0, self.mouse.1);
                        self.set_sel(Some(m));
                    }
                    _ => {}
                }
            }
            _ if self.sel.is_some() && !self.gif_mode => {
                let key = vk as u8;
                if key == b'G' {
                    action = Some(Action::Gif);
                } else if key == b'O' {
                    let m = if self.prefs.outline == OutlineMode::Off { self.prefs.outline_on } else { OutlineMode::Off };
                    self.set_outline(m);
                } else if let Some(t) = TOOLS.iter().find(|t| t.3 == key) {
                    action = self.activate(Btn::Tool(t.0));
                }
            }
            _ => {}
        }
        self.update_cursor();
        self.invalidate();
        action
    }

    fn on_char(&mut self, c: u16) {
        let Some(e) = &mut self.edit else { return };
        if (0xD800..0xDC00).contains(&c) {
            self.high_surrogate = Some(c);
            return;
        }
        let ch = if (0xDC00..0xE000).contains(&c) {
            match self.high_surrogate.take() {
                Some(hi) => char::decode_utf16([hi, c]).next().and_then(|r| r.ok()),
                None => None,
            }
        } else {
            char::from_u32(c as u32)
        };
        match ch {
            Some('\r') => e.text.push('\n'),
            Some('\u{8}') => {
                e.text.pop();
            }
            Some(ch) if !ch.is_control() => e.text.push(ch),
            _ => return,
        }
        self.caret_on = true;
        self.comp_ok = false;
        self.invalidate();
    }
}

/// Drives the real input handlers to type, move, re-edit and undo text
/// (debug self-test).
#[cfg(debug_assertions)]
pub fn text_editing_selftest(fonts: Arc<Fonts>) -> Result<String, String> {
    let (w, h) = (800, 500);
    let vs = Rect::new(0, 0, w, h);
    let mut ov = Overlay::with_image(null_mut(), false, fonts, vs, vec![255u8; (w * h * 4) as usize])
        .ok_or("overlay init failed")?;
    ov.monitors = vec![vs];
    ov.windows.clear();
    ov.sel = Some(Rect::new(50, 50, 750, 450));
    let text = |ov: &Overlay| match ov.shapes.first() {
        Some(Shape::Text { pos, text, .. }) => Ok((*pos, text.clone())),
        other => Err(format!("expected one text shape, found {other:?}")),
    };
    let check = |ok: bool, what: &str| if ok { Ok(()) } else { Err(what.to_string()) };

    // Type "Hello" with the text tool, then leave the tool (commits it).
    ov.tool = Some(Tool::Text);
    ov.on_down(200, 200, false);
    ov.on_up(200, 200);
    "Hello".encode_utf16().for_each(|c| ov.on_char(c));
    ov.activate(Btn::Tool(Tool::Text));
    let (p0, _) = text(&ov)?;

    // Drag it by (100, 50).
    let (gx, gy) = (p0.0 as i32 + 10, p0.1 as i32 + 10);
    ov.on_down(gx, gy, false);
    ov.on_move(gx + 50, gy + 25, false);
    ov.on_move(gx + 100, gy + 50, false);
    ov.on_up(gx + 100, gy + 50);
    let (p1, _) = text(&ov)?;
    check(p1 == (p0.0 + 100.0, p0.1 + 50.0), "dragging didn't move the text")?;
    ov.undo();
    check(text(&ov)?.0 == p0, "undo didn't restore the position")?;
    ov.redo();
    check(text(&ov)?.0 == p1, "redo didn't move it back")?;

    // Double-clicking a text must not copy-and-close.
    check(!matches!(ov.on_down(gx + 100, gy + 50, true), Some(Action::Copy)), "double-click on text copied")?;
    ov.on_up(gx + 100, gy + 50);
    ov.commit_edit();

    // A click (no drag) reopens it; typing and dragging the box apply.
    ov.on_down(gx + 100, gy + 50, false);
    ov.on_up(gx + 100, gy + 50);
    check(ov.edit.is_some() && ov.shapes.is_empty(), "clicking the text didn't open it for editing")?;
    ov.on_char('!' as u16);
    ov.on_down(gx + 100, gy + 50, false);
    ov.on_move(gx + 130, gy + 70, false);
    ov.on_up(gx + 130, gy + 70);
    ov.commit_edit();
    let (p2, t2) = text(&ov)?;
    check(t2 == "Hello!" && p2 == (p1.0 + 30.0, p1.1 + 20.0), "editing or moving the edit box failed")?;
    ov.undo();
    check(text(&ov)? == (p1, "Hello".into()), "undo didn't restore the text before editing")?;

    // Abandoning a re-edit (Ctrl+Z while typing) keeps the original.
    ov.on_down(gx + 100, gy + 50, false);
    ov.on_up(gx + 100, gy + 50);
    ov.on_char('X' as u16);
    ov.undo();
    check(ov.edit.is_none() && text(&ov)? == (p1, "Hello".into()), "abandoned re-edit lost the text")?;
    Ok("type, drag, undo/redo, click to edit, drag while editing, abandon edit".into())
}

/// Drives the real input handlers for step numbers, moving drawings,
/// deleting and picking a color (debug self-test).
#[cfg(debug_assertions)]
pub fn drawing_selftest(fonts: Arc<Fonts>) -> Result<String, String> {
    let (w, h) = (800, 500);
    let vs = Rect::new(0, 0, w, h);
    let mut shot = vec![255u8; (w * h * 4) as usize];
    let k = ((400 * w + 700) * 4) as usize;
    shot[k..k + 4].copy_from_slice(&[0xFF, 0x90, 0x1E, 255]); // #1E90FF at (700, 400)
    let mut ov = Overlay::with_image(null_mut(), false, fonts, vs, shot).ok_or("overlay init failed")?;
    ov.monitors = vec![vs];
    ov.windows.clear();
    ov.sel = Some(Rect::new(50, 50, 750, 450));
    let check = |ok: bool, what: &str| if ok { Ok(()) } else { Err(what.to_string()) };
    let steps = |ov: &Overlay| -> Vec<(u32, Pt)> {
        ov.shapes
            .iter()
            .filter_map(|s| match s {
                Shape::Step { n, pos, .. } => Some((*n, *pos)),
                _ => None,
            })
            .collect()
    };
    let draw = |ov: &mut Overlay, tool: Tool, a: (i32, i32), b: (i32, i32)| {
        ov.tool = Some(tool);
        ov.on_down(a.0, a.1, false);
        ov.on_move(b.0, b.1, false);
        ov.on_up(b.0, b.1);
        ov.tool = None;
    };

    // Step numbers count up; holding the button down drags the new one.
    ov.tool = Some(Tool::Step);
    for x in [100, 200, 300] {
        ov.on_down(x, 100, false);
        if x == 300 {
            ov.on_move(320, 120, false);
        }
        ov.on_up(x, 100);
    }
    let s = steps(&ov);
    check(s.iter().map(|s| s.0).collect::<Vec<_>>() == [1, 2, 3], "steps weren't numbered 1, 2, 3")?;
    check(s[2].1 == (320.0, 120.0), "dragging a new step didn't move it")?;
    ov.undo();
    ov.on_down(400, 100, false);
    ov.on_up(400, 100);
    check(steps(&ov).last().map(|s| s.0) == Some(3), "after undo, the next step wasn't 3")?;

    // With no tool, an arrow is grabbed and moved anywhere along it.
    draw(&mut ov, Tool::Arrow, (100, 300), (200, 300));
    ov.on_down(150, 300, false);
    ov.on_move(150, 340, false);
    ov.on_up(150, 340);
    let moved = matches!(ov.shapes.last(), Some(Shape::Arrow { a, b, .. }) if a.1 == 340.5 && b.1 == 340.5);
    check(moved, "dragging an arrow didn't move it")?;

    // A rectangle is grabbed by its border; inside still moves the selection.
    draw(&mut ov, Tool::Rect, (400, 250), (600, 400));
    ov.on_down(500, 330, false);
    check(matches!(ov.drag, Drag::Move { .. }), "clicking inside a rectangle grabbed it")?;
    ov.on_up(500, 330);
    ov.on_down(400, 330, false);
    ov.on_move(410, 330, false);
    ov.on_up(410, 330);
    let moved = matches!(ov.shapes.last(), Some(Shape::Rect { a, .. }) if a.0 == 410.5);
    check(moved, "dragging a rectangle's border didn't move it")?;

    // Delete removes the drawing under the cursor; undo brings it back.
    let count = ov.shapes.len();
    ov.mouse = (150, 340);
    ov.on_key(VK_DELETE);
    let arrows = |ov: &Overlay| ov.shapes.iter().filter(|s| matches!(s, Shape::Arrow { .. })).count();
    check(ov.shapes.len() == count - 1 && arrows(&ov) == 0, "Delete didn't remove the arrow under the cursor")?;
    ov.undo();
    check(ov.shapes.len() == count && arrows(&ov) == 1, "undo didn't restore the deleted arrow")?;

    // C copies the screen color under the cursor.
    ov.mouse = (700, 400);
    check(matches!(ov.on_key(b'C' as u16), Some(Action::CopyColor)), "C didn't ask to copy the color")?;
    check(ov.color_under_cursor() == "#1E90FF", "wrong color under the cursor")?;
    Ok("steps 1-2-3, drag while placing, move arrow/rect, delete + undo, color #1E90FF".into())
}

/// Renders the overlay into a PNG without showing a window (debug self-test).
#[cfg(debug_assertions)]
pub fn render_preview(fonts: Arc<Fonts>, out: &std::path::Path) -> Result<(), String> {
    let vs = capture::virtual_screen();
    let shot = capture::capture(vs).ok_or("capture failed")?;
    let mut ov = Overlay::with_image(null_mut(), false, fonts, vs, shot).ok_or("overlay init failed")?;
    let mon = ov.monitors.first().copied().unwrap_or(ov.bounds());
    let sel = Rect::xywh(mon.l + mon.w() / 4, mon.t + mon.h() / 4, mon.w() / 2, mon.h() / 2);
    ov.sel = Some(sel);
    let (l, t) = (sel.l as f32, sel.t as f32);
    let red = PALETTE[0];
    ov.shapes = vec![
        Shape::Rect { a: (l + 40.5, t + 40.5), b: (l + 300.5, t + 180.5), color: red, width: 3.0, outline: Some(annotate::OUTLINE_WHITE) },
        Shape::Arrow { a: (l + 420.0, t + 260.0), b: (l + 310.0, t + 190.0), color: red, width: 3.0, outline: Some(annotate::OUTLINE_WHITE) },
        Shape::Ellipse { a: (l + 340.0, t + 40.0), b: (l + 520.0, t + 140.0), color: PALETTE[5], width: 3.0, outline: None },
        Shape::Marker { pts: vec![(l + 60.0, t + 230.0), (l + 260.0, t + 232.0)], color: PALETTE[2], width: 16.0 },
        Shape::Pen {
            pts: (0..40).map(|i| (l + 60.0 + i as f32 * 6.0, t + 300.0 + (i as f32 * 0.4).sin() * 20.0)).collect(),
            color: PALETTE[3],
            width: 3.0,
            outline: Some(annotate::OUTLINE_WHITE),
        },
        Shape::Text { pos: (l + 450.0, t + 280.0), text: "Hello, árvíztűrő!\nSecond line".into(), color: red, size: 22.0, outline: Some(annotate::OUTLINE_WHITE) },
        Shape::Pixelate { a: (l + 40.0, t + 340.0), b: (l + 260.0, t + 420.0), block: 10.0, seed: 7, patch: None },
        Shape::Step { pos: (l + 600.0, t + 60.0), n: 1, color: red, size: 28.0, outline: Some(annotate::OUTLINE_WHITE) },
        Shape::Step { pos: (l + 640.0, t + 60.0), n: 2, color: PALETTE[5], size: 28.0, outline: Some(annotate::OUTLINE_WHITE) },
        Shape::Step { pos: (l + 680.0, t + 60.0), n: 3, color: PALETTE[2], size: 28.0, outline: Some(annotate::OUTLINE_BLACK) },
        Shape::Step { pos: (l + 725.0, t + 60.0), n: 12, color: PALETTE[3], size: 36.0, outline: None },
    ];
    let mut shapes = std::mem::take(&mut ov.shapes);
    for s in &mut shapes {
        ov.finish_pixelate(s);
    }
    ov.shapes = shapes;
    ov.tool = Some(Tool::Arrow);
    ov.palette_open = true;
    if let Some(lay) = ov.layout() {
        ov.hover_btn = lay.buttons.iter().find(|b| b.0 == Btn::Save).map(|b| b.0);
    }
    let all = ov.bounds();
    ov.render(&[all]);
    let px = unsafe { std::slice::from_raw_parts(ov.back.bits, ov.back.len()) }.to_vec();
    let png = capture::encode_png(ov.w, ov.h, &px)?;
    std::fs::write(out, png).map_err(|e| e.to_string())
}
