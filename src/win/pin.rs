//! Pinned captures on Windows: one borderless, always-on-top layered window
//! per pin, drawn by [`crate::pin::Pin`]. Drag to move, drag an edge or
//! corner to resize, wheel to zoom, Ctrl+wheel for opacity, arrows to nudge;
//! the toolbar that appears on hover has zoom, opacity, copy, save and close;
//! right-click for a menu, Esc or double-click to close.
//!
//! Each window keeps everything it needs in its own state and only reports
//! back to the app by posting messages (it never touches the app state).

use std::ptr::{null, null_mut};
use std::sync::Arc;
use std::time::Instant;

use windows_sys::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{BeginPaint, BitBlt, EndPaint, InvalidateRect, PAINTSTRUCT, SRCCOPY};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::SystemServices::MK_CONTROL;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, VK_CONTROL, VK_DOWN, VK_ESCAPE, VK_LEFT, VK_RIGHT, VK_SHIFT, VK_UP,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow, GetCursorPos,
    GetWindowLongPtrW, GetWindowRect, KillTimer, LoadCursorW, PostMessageW, RegisterClassExW, SetCursor,
    SetForegroundWindow, SetLayeredWindowAttributes, SetTimer, SetWindowLongPtrW, SetWindowPos, ShowWindow,
    TrackPopupMenu, CS_DROPSHADOW, GWLP_USERDATA, HTCAPTION, HTCLIENT, IDC_ARROW, IDC_HAND, IDC_SIZEALL, IDC_SIZENESW,
    IDC_SIZENS, IDC_SIZENWSE, IDC_SIZEWE, LWA_ALPHA, MB_ICONERROR, MF_CHECKED, MF_GRAYED, MF_POPUP, MF_SEPARATOR,
    MF_STRING, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SW_SHOWNOACTIVATE, TPM_NONOTIFY, TPM_RETURNCMD,
    TPM_RIGHTBUTTON, WM_CAPTURECHANGED, WM_CONTEXTMENU, WM_DPICHANGED, WM_ERASEBKGND, WM_KEYDOWN, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_NCDESTROY, WM_NCHITTEST, WM_NCLBUTTONDBLCLK, WM_NCRBUTTONUP,
    WM_NULL, WM_PAINT, WM_SETCURSOR, WM_SIZE, WM_TIMER, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_POPUP,
};

use super::capture::Dib;
use super::{clipboard, overlay, EditPin, WM_APP_CLOSE_PINS, WM_APP_EDIT_PIN, WM_APP_PIN_CLOSED};
use crate::annotate::Shape;
use crate::pin::{Edge, Pin, PinBtn, BORDER, OPACITIES};
use crate::text::Fonts;
use crate::util::{self, wide};

const CLASS: &str = "ScreenCapturePin";
const TIMER_LABELS: usize = 1;
/// Checks whether the mouse has left the pin (to hide the toolbar).
const TIMER_HOVER: usize = 2;
const HINT: &[&str] = &[
    "Drag to move, drag the edges to resize",
    "Wheel: zoom · Ctrl+wheel: opacity",
    "Arrows: nudge · Right-click: more",
];

const ID_COPY: usize = 1;
const ID_SAVE: usize = 2;
const ID_ACTUAL: usize = 3;
const ID_CLOSE: usize = 4;
const ID_CLOSE_ALL: usize = 5;
const ID_EDIT: usize = 6;
const ID_OPACITY: usize = 10;

struct PinWin {
    pin: Pin,
    main: HWND,
    fonts: Arc<Fonts>,
    surface: Option<Dib>,
    /// Leftover wheel movement from high-resolution wheels and touchpads.
    wheel: i32,
    /// Resizing by an edge: which, the window and mouse (screen) at the start.
    sizing: Option<(Edge, RECT, POINT)>,
}

pub fn register_class(hinst: HINSTANCE) {
    let cls = wide(CLASS);
    unsafe {
        let mut wc: WNDCLASSEXW = std::mem::zeroed();
        wc.cbSize = size_of::<WNDCLASSEXW>() as u32;
        wc.style = CS_DROPSHADOW;
        wc.lpfnWndProc = Some(wndproc);
        wc.hInstance = hinst;
        wc.lpszClassName = cls.as_ptr();
        RegisterClassExW(&wc);
    }
}

/// Pins an image with its top-left corner at screen position `at` (the
/// frame goes just outside). Doesn't take the focus; a click does. Returns
/// the window, or null on failure.
pub fn open(
    main: HWND,
    w: i32,
    h: i32,
    pixels: Vec<u8>,
    layers: Option<(Vec<u8>, Vec<Shape>)>,
    at: (i32, i32),
    fonts: Arc<Fonts>,
) -> HWND {
    let mut pin = Pin::new(w, h, pixels, HINT, true);
    pin.set_layers(layers);
    let (ww, wh) = pin.size();
    unsafe {
        let cls = wide(CLASS);
        let title = wide("Pinned capture");
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED,
            cls.as_ptr(),
            title.as_ptr(),
            WS_POPUP,
            at.0 - BORDER,
            at.1 - BORDER,
            ww,
            wh,
            null_mut(),
            null_mut(),
            GetModuleHandleW(null()),
            null(),
        );
        if hwnd.is_null() {
            return null_mut();
        }
        let state = Box::into_raw(Box::new(PinWin { pin, main, fonts, surface: None, wheel: 0, sizing: None }));
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
        // A layered window stays invisible until its opacity is set once.
        SetLayeredWindowAttributes(hwnd, 0, 255, LWA_ALPHA);
        ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        schedule_labels(hwnd, &(*state).pin);
        hwnd
    }
}

/// What editing a pin starts from: its picture without drawings, the
/// drawings, and where the picture is on screen.
pub fn content(hwnd: HWND) -> Option<EditPin> {
    unsafe {
        let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const PinWin;
        if p.is_null() {
            return None;
        }
        let (w, h, _) = (*p).pin.image();
        let (base, shapes) = (*p).pin.layers();
        let r = window_rect(hwnd);
        Some(EditPin { target: hwnd, w, h, base, shapes, at: (r.left + BORDER, r.top + BORDER) })
    }
}

/// Shows the edited picture, keeping the pin's place, zoom and opacity.
pub fn replace(hwnd: HWND, w: i32, h: i32, pixels: Vec<u8>, layers: Option<(Vec<u8>, Vec<Shape>)>) {
    unsafe {
        let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut PinWin;
        if p.is_null() {
            return;
        }
        (*p).pin.replace(w, h, pixels, layers);
        let (ww, wh) = (*p).pin.size();
        SetWindowPos(hwnd, null_mut(), 0, 0, ww, wh, SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOMOVE);
        redraw(hwnd, &(*p).pin);
    }
}

/// Asks the app to reopen the pin in the editor.
unsafe fn edit(hwnd: HWND, s: &PinWin) {
    PostMessageW(s.main, WM_APP_EDIT_PIN, hwnd as WPARAM, 0);
}

/// Repaints when the next label (hint, zoom or opacity) disappears.
unsafe fn schedule_labels(hwnd: HWND, pin: &Pin) {
    match pin.next_deadline() {
        Some(t) => {
            let ms = t.saturating_duration_since(Instant::now()).as_millis().clamp(10, 60_000) as u32;
            SetTimer(hwnd, TIMER_LABELS, ms, None);
        }
        None => {
            KillTimer(hwnd, TIMER_LABELS);
        }
    }
}

unsafe fn redraw(hwnd: HWND, pin: &Pin) {
    InvalidateRect(hwnd, null(), 0);
    schedule_labels(hwnd, pin);
}

fn ctrl() -> bool {
    unsafe { GetKeyState(VK_CONTROL as i32) < 0 }
}

fn shift() -> bool {
    unsafe { GetKeyState(VK_SHIFT as i32) < 0 }
}

unsafe fn window_rect(hwnd: HWND) -> RECT {
    let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    GetWindowRect(hwnd, &mut r);
    r
}

/// The window's size and its UI scale (toolbar, labels).
unsafe fn metrics(hwnd: HWND) -> (i32, i32, f32) {
    let r = window_rect(hwnd);
    (r.right - r.left, r.bottom - r.top, util::dpi_scale_at((r.left + r.right) / 2, (r.top + r.bottom) / 2))
}

fn lparam_xy(lp: LPARAM) -> (i32, i32) {
    ((lp & 0xFFFF) as i16 as i32, ((lp >> 16) & 0xFFFF) as i16 as i32)
}

/// The window rectangle while dragging `edge` by (`dx`, `dy`) from `start`:
/// the opposite edges stay put and the picture keeps its proportions.
fn resized(pin: &Pin, start: RECT, edge: Edge, dx: i32, dy: i32) -> RECT {
    let mut r = start;
    let min = 2 * BORDER + 8;
    let (west, east) = (matches!(edge, Edge::W | Edge::NW | Edge::SW), matches!(edge, Edge::E | Edge::NE | Edge::SE));
    let (north, south) = (matches!(edge, Edge::N | Edge::NW | Edge::NE), matches!(edge, Edge::S | Edge::SW | Edge::SE));
    if west {
        r.left = (r.left + dx).min(r.right - min);
    }
    if east {
        r.right = (r.right + dx).max(r.left + min);
    }
    if north {
        r.top = (r.top + dy).min(r.bottom - min);
    }
    if south {
        r.bottom = (r.bottom + dy).max(r.top + min);
    }
    let by_width = west || east;
    let (w, h) = pin.keep_aspect(r.right - r.left, r.bottom - r.top, by_width);
    if !by_width {
        r.right = r.left + w;
    } else if north {
        r.top = r.bottom - h;
    } else {
        r.bottom = r.top + h;
    }
    r
}

/// Zooms around window position (`ax`, `ay`) and resizes the window so that
/// point stays put.
unsafe fn zoom(hwnd: HWND, s: &mut PinWin, steps: i32, ax: i32, ay: i32, actual: bool) {
    let moved = if actual { s.pin.actual_size(ax, ay) } else { s.pin.zoom_by(steps, ax, ay) };
    if let Some((dx, dy)) = moved {
        let r = window_rect(hwnd);
        let (w, h) = s.pin.size();
        SetWindowPos(hwnd, null_mut(), r.left + dx, r.top + dy, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
    }
    redraw(hwnd, &s.pin);
}

unsafe fn set_opacity(hwnd: HWND, s: &mut PinWin, change: impl FnOnce(&mut Pin) -> bool) {
    if change(&mut s.pin) {
        SetLayeredWindowAttributes(hwnd, 0, (s.pin.opacity() * 255.0).round() as u8, LWA_ALPHA);
    }
    redraw(hwnd, &s.pin);
}

unsafe fn copy(hwnd: HWND, s: &mut PinWin) {
    let (w, h, px) = s.pin.image();
    let ok = clipboard::set_image(hwnd, w, h, px);
    s.pin.set_badge(if ok { "Copied".into() } else { "The clipboard is busy, try again".into() });
    redraw(hwnd, &s.pin);
}

/// The save dialog is modal, so it runs without a borrow of the state.
unsafe fn save(hwnd: HWND, p: *mut PinWin) {
    let Some(path) = overlay::save_dialog(hwnd) else { return };
    let (w, h, px) = (*p).pin.image();
    match util::encode_png(w, h, px).and_then(|png| std::fs::write(&path, png).map_err(|e| e.to_string())) {
        Ok(()) => {
            (*p).pin.set_badge("Saved".into());
            redraw(hwnd, &(*p).pin);
        }
        Err(e) => util::message_box(hwnd, &format!("Could not save {}:\n{e}", path.display()), MB_ICONERROR),
    }
}

/// A toolbar button was clicked.
unsafe fn press(hwnd: HWND, p: *mut PinWin, btn: PinBtn) {
    match btn {
        // Back to 100% from the top-left corner, so the pin lines up with
        // where it was captured again.
        PinBtn::Zoom => zoom(hwnd, &mut *p, 0, BORDER, BORDER, true),
        PinBtn::Opacity => set_opacity(hwnd, &mut *p, |pin| {
            pin.next_opacity();
            true
        }),
        PinBtn::Edit => edit(hwnd, &*p),
        PinBtn::Copy => copy(hwnd, &mut *p),
        PinBtn::Save => save(hwnd, p),
        PinBtn::Close => {
            DestroyWindow(hwnd);
        }
    }
}

unsafe fn show_menu(hwnd: HWND, p: *mut PinWin) {
    let (current_zoom, opacity, main) = ((*p).pin.zoom(), (*p).pin.opacity(), (*p).main);
    let menu = CreatePopupMenu();
    let item = |m, id: usize, text: &str, flags: u32| {
        let w = wide(text);
        AppendMenuW(m, MF_STRING | flags, id, w.as_ptr());
    };
    item(menu, ID_EDIT, "Edit…\tCtrl+E", 0);
    item(menu, ID_COPY, "Copy\tCtrl+C", 0);
    item(menu, ID_SAVE, "Save as…\tCtrl+S", 0);
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    item(menu, ID_ACTUAL, "Actual size (100%)\tCtrl+0", if current_zoom == 1.0 { MF_GRAYED } else { 0 });
    let op = CreatePopupMenu();
    for (i, o) in OPACITIES.iter().enumerate() {
        let checked = (o - opacity).abs() < 0.01;
        item(op, ID_OPACITY + i, &format!("{}%", (o * 100.0).round()), if checked { MF_CHECKED } else { 0 });
    }
    let label = wide("Opacity");
    AppendMenuW(menu, MF_POPUP, op as usize, label.as_ptr());
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    item(menu, ID_CLOSE, "Close\tEsc", 0);
    item(menu, ID_CLOSE_ALL, "Close all pins", 0);

    let mut pt = POINT { x: 0, y: 0 };
    GetCursorPos(&mut pt);
    SetForegroundWindow(hwnd);
    let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON, pt.x, pt.y, 0, hwnd, null()) as usize;
    PostMessageW(hwnd, WM_NULL, 0, 0);
    DestroyMenu(menu);
    match cmd {
        ID_EDIT => edit(hwnd, &*p),
        ID_COPY => copy(hwnd, &mut *p),
        ID_SAVE => save(hwnd, p),
        ID_ACTUAL => zoom(hwnd, &mut *p, 0, BORDER, BORDER, true),
        ID_CLOSE => {
            DestroyWindow(hwnd);
        }
        ID_CLOSE_ALL => {
            PostMessageW(main, WM_APP_CLOSE_PINS, 0, 0);
        }
        c if (ID_OPACITY..ID_OPACITY + OPACITIES.len()).contains(&c) => {
            let o = OPACITIES[c - ID_OPACITY];
            set_opacity(hwnd, &mut *p, |pin| {
                pin.set_opacity(o);
                true
            });
        }
        _ => {}
    }
}

/// Tracks the mouse over the pin (window position); the toolbar shows while
/// it's there.
unsafe fn hover(hwnd: HWND, s: &mut PinWin, pos: Option<(i32, i32)>) {
    let (w, h, scale) = metrics(hwnd);
    if s.pin.set_hover(pos, w, h, scale) {
        InvalidateRect(hwnd, null(), 0);
    }
    if pos.is_some() {
        SetTimer(hwnd, TIMER_HOVER, 80, None);
    } else {
        KillTimer(hwnd, TIMER_HOVER);
    }
}

unsafe fn paint(hwnd: HWND, s: &mut PinWin) {
    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let hdc = BeginPaint(hwnd, &mut ps);
    let (w, h, scale) = metrics(hwnd);
    if s.surface.as_ref().is_none_or(|d| (d.w, d.h) != (w, h)) {
        s.surface = Dib::new(w, h);
    }
    if let Some(dib) = s.surface.as_mut() {
        s.pin.render(dib.bytes_mut(), w, h, &s.fonts, scale);
        BitBlt(hdc, 0, 0, w, h, dib.dc, 0, 0, SRCCOPY);
    }
    EndPaint(hwnd, &ps);
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut PinWin;
    if p.is_null() {
        return DefWindowProcW(hwnd, msg, wp, lp);
    }
    match msg {
        // Toolbar buttons and edges are "client area" (clicks, handled here);
        // the rest is a title bar: dragging moves the pin natively.
        WM_NCHITTEST => {
            let r = window_rect(hwnd);
            let (sx, sy) = lparam_xy(lp);
            let (x, y) = (sx - r.left, sy - r.top);
            hover(hwnd, &mut *p, Some((x, y)));
            let (w, h, scale) = metrics(hwnd);
            let pin = &(*p).pin;
            if pin.button_at(x, y, w, h, scale).is_some() || pin.edge_at(x, y, w, h, scale).is_some() {
                HTCLIENT as LRESULT
            } else {
                HTCAPTION as LRESULT
            }
        }
        WM_SETCURSOR => match (lp & 0xFFFF) as u32 {
            HTCAPTION => {
                SetCursor(LoadCursorW(null_mut(), IDC_SIZEALL));
                1
            }
            HTCLIENT => {
                let mut pt = POINT { x: 0, y: 0 };
                GetCursorPos(&mut pt);
                let r = window_rect(hwnd);
                let (x, y) = (pt.x - r.left, pt.y - r.top);
                let (w, h, scale) = metrics(hwnd);
                let pin = &(*p).pin;
                let id = if pin.button_at(x, y, w, h, scale).is_some() {
                    IDC_HAND
                } else {
                    match pin.edge_at(x, y, w, h, scale) {
                        Some(Edge::N | Edge::S) => IDC_SIZENS,
                        Some(Edge::E | Edge::W) => IDC_SIZEWE,
                        Some(Edge::NW | Edge::SE) => IDC_SIZENWSE,
                        Some(Edge::NE | Edge::SW) => IDC_SIZENESW,
                        None => IDC_ARROW,
                    }
                };
                SetCursor(LoadCursorW(null_mut(), id));
                1
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        },
        WM_MOUSEMOVE => {
            let (x, y) = lparam_xy(lp);
            if let Some((edge, start, from)) = (*p).sizing {
                let r = window_rect(hwnd);
                let n = resized(&(*p).pin, start, edge, r.left + x - from.x, r.top + y - from.y);
                SetWindowPos(hwnd, null_mut(), n.left, n.top, n.right - n.left, n.bottom - n.top, SWP_NOZORDER | SWP_NOACTIVATE);
            } else {
                hover(hwnd, &mut *p, Some((x, y)));
            }
            0
        }
        WM_LBUTTONDOWN => {
            let (x, y) = lparam_xy(lp);
            let (w, h, scale) = metrics(hwnd);
            let btn = (*p).pin.button_at(x, y, w, h, scale);
            (*p).pin.set_pressed(btn);
            if btn.is_some() {
                SetCapture(hwnd);
            } else if let Some(edge) = (*p).pin.edge_at(x, y, w, h, scale) {
                let r = window_rect(hwnd);
                (*p).sizing = Some((edge, r, POINT { x: r.left + x, y: r.top + y }));
                SetCapture(hwnd);
            }
            0
        }
        WM_LBUTTONUP => {
            if let Some((edge, _, _)) = (*p).sizing.take() {
                ReleaseCapture();
                // Settle on the exact size for the zoom it ended at, keeping
                // the edges that didn't move.
                let r = window_rect(hwnd);
                let (w, h) = (*p).pin.size();
                let left = if matches!(edge, Edge::W | Edge::NW | Edge::SW) { r.right - w } else { r.left };
                let top = if matches!(edge, Edge::N | Edge::NW | Edge::NE) { r.bottom - h } else { r.top };
                SetWindowPos(hwnd, null_mut(), left, top, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
                return 0;
            }
            ReleaseCapture();
            let (x, y) = lparam_xy(lp);
            let (w, h, scale) = metrics(hwnd);
            let pressed = (*p).pin.pressed();
            (*p).pin.set_pressed(None);
            if let Some(btn) = pressed.filter(|&b| (*p).pin.button_at(x, y, w, h, scale) == Some(b)) {
                press(hwnd, p, btn);
            }
            0
        }
        WM_CAPTURECHANGED => {
            (*p).sizing = None;
            0
        }
        WM_NCLBUTTONDBLCLK if wp == HTCAPTION as WPARAM => {
            DestroyWindow(hwnd);
            0
        }
        WM_NCRBUTTONUP | WM_CONTEXTMENU => {
            show_menu(hwnd, p);
            0
        }
        WM_MOUSEWHEEL => {
            let s = &mut *p;
            s.wheel += ((wp >> 16) & 0xFFFF) as i16 as i32;
            let steps = s.wheel / 120;
            s.wheel %= 120;
            if steps != 0 {
                if (wp & 0xFFFF) as u32 & MK_CONTROL != 0 {
                    set_opacity(hwnd, s, |pin| pin.opacity_by(steps));
                } else {
                    // The cursor position comes in screen coordinates.
                    let r = window_rect(hwnd);
                    let (x, y) = lparam_xy(lp);
                    zoom(hwnd, s, steps, x - r.left, y - r.top, false);
                }
            }
            0
        }
        WM_SIZE => {
            let (w, h) = ((lp & 0xFFFF) as i32, ((lp >> 16) & 0xFFFF) as i32);
            if w > 0 && h > 0 && (*p).pin.fit_to(w, h, None) {
                redraw(hwnd, &(*p).pin);
            }
            0
        }
        WM_KEYDOWN => {
            let vk = wp as u16;
            match vk {
                VK_ESCAPE => {
                    DestroyWindow(hwnd);
                }
                VK_LEFT | VK_RIGHT | VK_UP | VK_DOWN => {
                    let step = if shift() { 10 } else { 1 };
                    let (dx, dy) = match vk {
                        VK_LEFT => (-step, 0),
                        VK_RIGHT => (step, 0),
                        VK_UP => (0, -step),
                        _ => (0, step),
                    };
                    let r = window_rect(hwnd);
                    SetWindowPos(hwnd, null_mut(), r.left + dx, r.top + dy, 0, 0, SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOSIZE);
                }
                _ if ctrl() && vk == b'E' as u16 => edit(hwnd, &*p),
                _ if ctrl() && vk == b'C' as u16 => copy(hwnd, &mut *p),
                _ if ctrl() && vk == b'S' as u16 => save(hwnd, p),
                _ if ctrl() && vk == b'0' as u16 => zoom(hwnd, &mut *p, 0, BORDER, BORDER, true),
                _ => {}
            }
            0
        }
        WM_PAINT => {
            paint(hwnd, &mut *p);
            0
        }
        WM_TIMER if wp == TIMER_LABELS => {
            if (*p).pin.expire(Instant::now()) {
                InvalidateRect(hwnd, null(), 0);
            }
            schedule_labels(hwnd, &(*p).pin);
            0
        }
        WM_TIMER if wp == TIMER_HOVER => {
            let mut pt = POINT { x: 0, y: 0 };
            GetCursorPos(&mut pt);
            let r = window_rect(hwnd);
            let inside = pt.x >= r.left && pt.x < r.right && pt.y >= r.top && pt.y < r.bottom;
            if !inside && (*p).pin.pressed().is_none() {
                hover(hwnd, &mut *p, None);
            }
            0
        }
        WM_ERASEBKGND => 1,
        // Keep the size when dragged onto a monitor with another DPI.
        WM_DPICHANGED => 0,
        WM_NCDESTROY => {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            let s = Box::from_raw(p);
            PostMessageW(s.main, WM_APP_PIN_CLOSED, hwnd as WPARAM, 0);
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// A real pin window, off-screen: wheel zoom resizes it, Ctrl+wheel sets its
/// opacity, an arrow key nudges it, edges resize keeping proportions, and
/// the toolbar's close button closes it (debug self-test).
#[cfg(debug_assertions)]
pub fn selftest(fonts: Arc<Fonts>) -> Result<String, String> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetLayeredWindowAttributes, IsWindow, PeekMessageW, SendMessageW, LAYERED_WINDOW_ATTRIBUTES_FLAGS, MSG,
        PM_REMOVE,
    };
    let check = |ok: bool, what: &str| if ok { Ok(()) } else { Err(what.to_string()) };
    unsafe {
        register_class(GetModuleHandleW(null()));
        let (w, h, at) = (100, 80, (-10000, -10000));
        let hwnd = open(null_mut(), w, h, vec![200u8; (w * h * 4) as usize], None, at, fonts);
        check(!hwnd.is_null(), "couldn't create the pin window")?;
        let size = |hwnd| {
            let r = window_rect(hwnd);
            (r.left, r.top, r.right - r.left, r.bottom - r.top)
        };
        let xy = |x: i32, y: i32| ((y as u16 as u32) << 16 | x as u16 as u32) as LPARAM;
        let result = (|| {
            check(size(hwnd) == (at.0 - BORDER, at.1 - BORDER, w + 2 * BORDER, h + 2 * BORDER), "wrong initial placement")?;
            SendMessageW(hwnd, WM_MOUSEWHEEL, (120u16 as usize) << 16, xy(at.0, at.1));
            let (_, _, zw, zh) = size(hwnd);
            check((zw, zh) == (125 + 2 * BORDER, 100 + 2 * BORDER), "wheel didn't zoom to 125%")?;
            SendMessageW(hwnd, WM_MOUSEWHEEL, ((-120i16 as u16 as usize) << 16) | MK_CONTROL as usize, xy(at.0, at.1));
            let mut alpha = 0u8;
            let mut flags: LAYERED_WINDOW_ATTRIBUTES_FLAGS = 0;
            GetLayeredWindowAttributes(hwnd, null_mut(), &mut alpha, &mut flags);
            check(alpha == (0.9f32 * 255.0).round() as u8, "Ctrl+wheel didn't lower the opacity to 90%")?;
            let (x0, y0, _, _) = size(hwnd);
            SendMessageW(hwnd, WM_KEYDOWN, VK_RIGHT as usize, 0);
            check(size(hwnd).0 == x0 + 1 && size(hwnd).1 == y0, "the arrow key didn't nudge it")?;

            // Dragging the right edge 100 px further: 125% → 225%, the height
            // follows; then the bottom-left corner: the top-right stays put.
            let (x0, y0, ww, wh) = size(hwnd);
            let hit = |x: i32, y: i32| SendMessageW(hwnd, WM_NCHITTEST, 0, xy(x0 + x, y0 + y)) as u32;
            check(hit(ww - 2, wh / 2) == HTCLIENT, "the right edge doesn't take clicks")?;
            SendMessageW(hwnd, WM_LBUTTONDOWN, 1, xy(ww - 2, wh / 2));
            SendMessageW(hwnd, WM_MOUSEMOVE, 1, xy(ww - 2 + 50, wh / 2));
            SendMessageW(hwnd, WM_MOUSEMOVE, 1, xy(ww - 2 + 100, wh / 2));
            SendMessageW(hwnd, WM_LBUTTONUP, 0, xy(ww - 2 + 100, wh / 2));
            check(size(hwnd) == (x0, y0, 225 + 2 * BORDER, 180 + 2 * BORDER), "dragging the right edge didn't resize to 225%")?;
            let (x0, y0, ww, wh) = size(hwnd);
            SendMessageW(hwnd, WM_LBUTTONDOWN, 1, xy(1, wh - 2));
            SendMessageW(hwnd, WM_MOUSEMOVE, 1, xy(1 + 25, wh - 2));
            SendMessageW(hwnd, WM_LBUTTONUP, 0, xy(1, wh - 2));
            let (x1, y1, w1, h1) = size(hwnd);
            check(w1 == ww - 25 && x1 + w1 == x0 + ww && y1 == y0 && h1 < wh, "dragging the bottom-left corner didn't keep the top-right")?;
            SetWindowPos(hwnd, null_mut(), 0, 0, 2 * w + 2 * BORDER, 2 * h + 2 * BORDER, SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOMOVE);
            // Read fresh each time: the messages below change the state.
            let state = || &*(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const PinWin);
            check((state().pin.zoom() - 2.0).abs() < 1e-3, "the zoom didn't follow the new size")?;

            // Editing: the Edit button asks the app; the pin hands over its
            // picture and position; an edited picture replaces it at the zoom.
            let (x0, y0, ww, wh) = size(hwnd);
            let (_, _, scale) = metrics(hwnd);
            SendMessageW(hwnd, WM_NCHITTEST, 0, xy(x0 + ww / 2, y0 + wh / 2));
            let edit_btn = state().pin.buttons(ww, wh, scale).into_iter().find(|b| b.0 == PinBtn::Edit).ok_or("no edit button")?.1;
            let (ex, ey) = edit_btn.center();
            SendMessageW(hwnd, WM_LBUTTONDOWN, 1, xy(ex, ey));
            SendMessageW(hwnd, WM_LBUTTONUP, 0, xy(ex, ey));
            let mut msg: MSG = std::mem::zeroed();
            let asked = PeekMessageW(&mut msg, null_mut(), WM_APP_EDIT_PIN, WM_APP_EDIT_PIN, PM_REMOVE) != 0;
            check(asked && msg.wParam == hwnd as WPARAM, "the edit button didn't ask to edit this pin")?;
            let e = content(hwnd).ok_or("no content to edit")?;
            check(e.at == (x0 + BORDER, y0 + BORDER) && (e.w, e.h) == (w, h) && e.base.len() == (w * h * 4) as usize, "edit content")?;
            replace(hwnd, 150, 100, vec![10u8; 150 * 100 * 4], None);
            check(size(hwnd) == (x0, y0, 300 + 2 * BORDER, 200 + 2 * BORDER), "the edited picture didn't keep place and zoom")?;

            // Hit testing: middle moves, edges resize, the close button clicks.
            let (x0, y0, ww, wh) = size(hwnd);
            let hit = |x: i32, y: i32| SendMessageW(hwnd, WM_NCHITTEST, 0, xy(x0 + x, y0 + y)) as u32;
            check(hit(ww / 2, wh / 2) == HTCAPTION, "the middle doesn't move the pin")?;
            check(hit(1, wh / 2) == HTCLIENT && hit(ww - 2, wh - 2) == HTCLIENT, "edges don't take clicks")?;
            let (_, _, scale) = metrics(hwnd);
            let close = state().pin.buttons(ww, wh, scale)[0];
            check(close.0 == PinBtn::Close, "no close button in the toolbar")?;
            let (cx, cy) = close.1.center();
            check(hit(cx, cy) == HTCLIENT, "the close button isn't clickable")?;
            SendMessageW(hwnd, WM_LBUTTONDOWN, 1, xy(cx, cy));
            SendMessageW(hwnd, WM_LBUTTONUP, 0, xy(cx, cy));
            check(IsWindow(hwnd) == 0, "the close button didn't close the pin")?;
            Ok("wheel zoom, Ctrl+wheel opacity, arrow nudge, edge/corner resize, edit, replace, close button".to_string())
        })();
        if IsWindow(hwnd) != 0 {
            DestroyWindow(hwnd);
        }
        result
    }
}
