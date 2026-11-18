//! The Windows app: tray icon, PrtSc keyboard hook, the editor window, pinned
//! captures, the recent-captures history and GIF recording.

pub mod capture;
pub mod clipboard;
pub mod gif_rec;
pub mod ocr;
pub mod overlay;
pub mod pin;
pub mod tray;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering::Relaxed};
use std::sync::Arc;

use windows_sys::Win32::Foundation::{
    GetLastError, ERROR_ALREADY_EXISTS, FALSE, HWND, LPARAM, LRESULT, POINT, WPARAM,
};
use windows_sys::Win32::Graphics::Dwm::DwmFlush;
use windows_sys::Win32::Graphics::Gdi::{DeleteObject, HBITMAP};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::HiDpi::{
    GetDpiForSystem, GetSystemMetricsForDpi, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT, VK_SNAPSHOT,
};
use windows_sys::Win32::UI::Shell::{ShellExecuteW, NINF_KEY, NIN_BALLOONUSERCLICK, NIN_SELECT};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CallNextHookEx, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, FindWindowW, GetCursorPos, GetMenuItemCount, GetMessageW, InsertMenuItemW, IsWindow, KillTimer,
    PostMessageW, PostQuitMessage, RegisterClassExW, RegisterWindowMessageW, SetForegroundWindow, SetMenuDefaultItem,
    SetTimer, SetWindowsHookExW, ShowWindow, TrackPopupMenu, TranslateMessage, UnhookWindowsHookEx, HC_ACTION, HICON,
    HMENU, KBDLLHOOKSTRUCT, LLKHF_ALTDOWN, MENUITEMINFOW, MIIM_BITMAP, MIIM_ID, MIIM_STRING, SW_HIDE,
    SW_SHOWNOACTIVATE,
    MB_ICONERROR, MB_ICONINFORMATION, MB_ICONWARNING, MF_CHECKED, MF_GRAYED, MF_POPUP, MF_SEPARATOR, MF_STRING, MF_UNCHECKED,
    MSG, SM_CXSMICON, SW_SHOWNORMAL, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, WH_KEYBOARD_LL, WM_APP,
    WM_CONTEXTMENU, WM_DESTROY, WM_KEYDOWN, WM_KEYUP, WM_NULL, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_TIMER, WNDCLASSEXW,
    WS_POPUP,
};

use crate::annotate::Shape;
use crate::editor::{GifRequest, Mode};
use crate::history::{self, History};
use self::gif_rec::{GifOutcome, Recording};
use crate::settings::{Settings, FPS_CHOICES};
use crate::text::Fonts;
use crate::util::wide;
use crate::{editor, gif_enc, settings, util};
#[cfg(debug_assertions)]
use crate::selftest;

pub const WM_APP_TRAY: u32 = WM_APP + 1;
pub const WM_APP_HOTKEY: u32 = WM_APP + 2;
pub const WM_APP_OVERLAY_DONE: u32 = WM_APP + 3;
pub const WM_APP_START_GIF: u32 = WM_APP + 4;
pub const WM_APP_STOP_GIF: u32 = WM_APP + 5;
pub const WM_APP_CANCEL_GIF: u32 = WM_APP + 6;
pub const WM_APP_GIF_DONE: u32 = WM_APP + 7;
/// A finished capture (`lparam`: `Box<Captured>`): goes into the history,
/// and is pinned if asked.
pub const WM_APP_CAPTURED: u32 = WM_APP + 8;
/// A compressed history entry is ready (`lparam`: `Box<history::Entry>`).
pub const WM_APP_HISTORY_ADD: u32 = WM_APP + 9;
/// A pin window closed (`wparam`: its HWND).
pub const WM_APP_PIN_CLOSED: u32 = WM_APP + 10;
pub const WM_APP_CLOSE_PINS: u32 = WM_APP + 11;
/// A pin asks to be edited (`wparam`: its HWND).
pub const WM_APP_EDIT_PIN: u32 = WM_APP + 12;

const NIN_KEYSELECT: u32 = NIN_SELECT | NINF_KEY;

const MAIN_CLASS: &str = "ScreenCaptureMain";
const TIMER_DELAYED_CAPTURE: usize = 1;
const TIP_IDLE: &str = "Screen Capture\nPrtSc: capture · Ctrl+PrtSc: pin · Shift+PrtSc: GIF";

const ID_CAPTURE: usize = 1;
const ID_GIF: usize = 2;
const ID_STOP: usize = 3;
const ID_OPEN_FOLDER: usize = 4;
const ID_AUTOSTART: usize = 5;
const ID_CURSOR: usize = 6;
const ID_HOMEPAGE: usize = 7;
const ID_REPOSITORY: usize = 8;
const ID_EXIT: usize = 9;
const ID_PIN: usize = 10;
const ID_CLEAR_HISTORY: usize = 11;
const ID_CLOSE_PINS: usize = 12;
const ID_FPS: usize = 100;
/// Recent captures, by their position in the menu.
const ID_HISTORY: usize = 200;

/// A capture the editor finished, sent to the main window.
pub struct Captured {
    pub w: i32,
    pub h: i32,
    pub pixels: Vec<u8>,
    /// The picture without drawings and the drawings, kept by pins so they
    /// can be edited again.
    pub layers: Option<(Vec<u8>, Vec<Shape>)>,
    /// Screen position of its top-left corner.
    pub at: (i32, i32),
    pub pin: bool,
    /// The pin this was an edit of: it shows the result.
    pub replace: Option<HWND>,
}

/// A pin to edit: its picture without drawings, the drawings, and where it
/// is on screen.
pub struct EditPin {
    pub target: HWND,
    pub w: i32,
    pub h: i32,
    pub base: Vec<u8>,
    pub shapes: Vec<Shape>,
    pub at: (i32, i32),
}

/// `WM_APP_HOTKEY`'s `wparam` (also sent by `--capture`, `--gif`, `--pin`).
fn mode_to_wparam(mode: Mode) -> usize {
    match mode {
        Mode::Capture => 0,
        Mode::Gif => 1,
        Mode::Pin => 2,
    }
}

fn mode_from_wparam(wp: usize) -> Mode {
    match wp {
        1 => Mode::Gif,
        2 => Mode::Pin,
        _ => Mode::Capture,
    }
}

static MAIN_HWND: AtomicUsize = AtomicUsize::new(0);
static PRTSC_DOWN: AtomicBool = AtomicBool::new(false);
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

struct App {
    hwnd: HWND,
    fonts: Arc<Fonts>,
    settings: Settings,
    overlay: HWND,
    rec: Option<Recording>,
    next_rec_id: u64,
    icon: HICON,
    icon_rec: HICON,
    last_gif: Option<PathBuf>,
    pending_mode: Mode,
    pins: Vec<HWND>,
    /// Pins are hidden while the editor is open, so new captures don't
    /// include them.
    pins_hidden: bool,
    history: History,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// Borrows the app state. Never call anything that can pump messages (modal
/// loops, window creation that reaches our own procs) inside `f`.
fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> R {
    APP.with(|a| f(a.borrow_mut().as_mut().expect("app not initialized")))
}

pub fn run() {
    install_panic_hook();
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };

    let args: Vec<String> = std::env::args().skip(1).collect();
    #[cfg(debug_assertions)]
    if args.first().map(String::as_str) == Some("--selftest") {
        let dir = args.get(1).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("selftest-out"));
        std::process::exit(selftest::run(&dir, args.get(2).map(Path::new)));
    }
    let request: Option<usize> = if args.iter().any(|a| a == "--gif") {
        Some(mode_to_wparam(Mode::Gif))
    } else if args.iter().any(|a| a == "--pin") {
        Some(mode_to_wparam(Mode::Pin))
    } else if args.iter().any(|a| a == "--capture") {
        Some(mode_to_wparam(Mode::Capture))
    } else {
        None
    };

    unsafe {
        let name = wide("Local\\ScreenCapture.SingleInstance");
        let _mutex = CreateMutexW(null(), FALSE, name.as_ptr());
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let cls = wide(MAIN_CLASS);
            let other = FindWindowW(cls.as_ptr(), null());
            match request {
                Some(r) if !other.is_null() => {
                    PostMessageW(other, WM_APP_HOTKEY, r, 0);
                }
                _ => util::message_box(
                    null_mut(),
                    "Screen Capture is already running.\nLook for its icon in the notification area.",
                    MB_ICONINFORMATION,
                ),
            }
            return;
        }
    }

    let fonts = match Fonts::load() {
        Ok(f) => Arc::new(f),
        Err(e) => {
            util::message_box(null_mut(), &e, MB_ICONERROR);
            return;
        }
    };
    let first_run = !Settings::exists();
    let settings = Settings::load();
    editor::apply_outline_setting(&settings.outline);
    if first_run {
        settings.save();
    }

    unsafe {
        let hinst = GetModuleHandleW(null());
        let cls = wide(MAIN_CLASS);
        let mut wc: WNDCLASSEXW = std::mem::zeroed();
        wc.cbSize = size_of::<WNDCLASSEXW>() as u32;
        wc.lpfnWndProc = Some(main_proc);
        wc.hInstance = hinst;
        wc.lpszClassName = cls.as_ptr();
        RegisterClassExW(&wc);
        overlay::register_class(hinst);
        pin::register_class(hinst);
        gif_rec::register_classes(hinst);

        // A hidden top-level window (not message-only) so it receives the
        // TaskbarCreated broadcast and can own the tray menu.
        let title = wide(util::APP_NAME);
        let hwnd = CreateWindowExW(
            0,
            cls.as_ptr(),
            title.as_ptr(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            null_mut(),
            null_mut(),
            hinst,
            null(),
        );
        if hwnd.is_null() {
            util::message_box(null_mut(), "Could not create the main window.", MB_ICONERROR);
            return;
        }
        MAIN_HWND.store(hwnd as usize, Relaxed);
        let tb = wide("TaskbarCreated");
        TASKBAR_CREATED.store(RegisterWindowMessageW(tb.as_ptr()), Relaxed);

        let size = GetSystemMetricsForDpi(SM_CXSMICON, GetDpiForSystem());
        let icon = tray::make_icon(size, false);
        let icon_rec = tray::make_icon(size, true);
        APP.with(|a| {
            *a.borrow_mut() = Some(App {
                hwnd,
                fonts,
                settings,
                overlay: null_mut(),
                rec: None,
                next_rec_id: 0,
                icon,
                icon_rec,
                last_gif: None,
                pending_mode: Mode::Capture,
                pins: Vec::new(),
                pins_hidden: false,
                history: History::default(),
            })
        });
        tray::add(hwnd, WM_APP_TRAY, icon, TIP_IDLE);
        spawn_hook_thread();

        if let Some(r) = request {
            PostMessageW(hwnd, WM_APP_HOTKEY, r, 0);
        } else if first_run {
            tray::balloon(
                hwnd,
                "Screen Capture is running",
                "Press PrtSc to capture an area, Ctrl+PrtSc to pin it on screen, Shift+PrtSc to record a GIF.",
                false,
            );
        }

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let msg = info.to_string();
        let dir = util::app_data_dir();
        let _ = std::fs::create_dir_all(&dir);
        let log = dir.join("crash.log");
        let previous = std::fs::read_to_string(&log).unwrap_or_default();
        let _ = std::fs::write(&log, format!("{previous}{} {msg}\n", util::timestamp()));
        util::message_box(
            null_mut(),
            &format!("Screen Capture hit an unexpected error:\n\n{msg}\n\nDetails were written to {}", log.display()),
            MB_ICONERROR,
        );
    }));
}

/// The low-level keyboard hook lives on its own thread with a bare message
/// loop, so a busy UI thread can never stall system-wide keyboard input.
fn spawn_hook_thread() {
    std::thread::spawn(|| unsafe {
        let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), GetModuleHandleW(null()), 0);
        if hook.is_null() {
            util::message_box(
                null_mut(),
                "Could not install the keyboard hook, so the PrtSc key won't work.\n\
                 You can still capture from the tray icon.",
                MB_ICONWARNING,
            );
            return;
        }
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {}
        UnhookWindowsHookEx(hook);
    });
}

unsafe extern "system" fn keyboard_hook(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let kb = &*(lp as *const KBDLLHOOKSTRUCT);
        if kb.vkCode == VK_SNAPSHOT as u32 {
            let msg = wp as u32;
            if msg == WM_KEYUP || msg == WM_SYSKEYUP {
                // Swallow the release only if we swallowed the press.
                if PRTSC_DOWN.swap(false, Relaxed) {
                    return 1;
                }
            } else if msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN {
                let held = |vk: u16| GetAsyncKeyState(vk as i32) < 0;
                // Alt/Win+PrtSc stay with Windows.
                let modifiers = kb.flags & LLKHF_ALTDOWN != 0 || held(VK_MENU) || held(VK_LWIN) || held(VK_RWIN);
                if !modifiers {
                    if !PRTSC_DOWN.swap(true, Relaxed) {
                        let mode = if held(VK_CONTROL) {
                            Mode::Pin
                        } else if held(VK_SHIFT) {
                            Mode::Gif
                        } else {
                            Mode::Capture
                        };
                        let main = MAIN_HWND.load(Relaxed) as HWND;
                        PostMessageW(main, WM_APP_HOTKEY, mode_to_wparam(mode), 0);
                    }
                    return 1;
                }
            }
        }
    }
    CallNextHookEx(null_mut(), code, wp, lp)
}

unsafe extern "system" fn main_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_APP_HOTKEY => on_hotkey(mode_from_wparam(wp)),
        WM_APP_TRAY => on_tray(hwnd, lp),
        WM_TIMER if wp == TIMER_DELAYED_CAPTURE => {
            KillTimer(hwnd, TIMER_DELAYED_CAPTURE);
            let mode = with_app(|a| a.pending_mode);
            on_hotkey(mode);
        }
        WM_APP_OVERLAY_DONE => {
            with_app(|a| {
                if a.overlay as usize == wp {
                    a.overlay = null_mut();
                }
                // Remember the outline choice across restarts.
                let outline = editor::outline_setting();
                if outline != a.settings.outline {
                    a.settings.outline = outline;
                    a.settings.save();
                }
            });
            show_pins(true);
        }
        WM_APP_CAPTURED => captured(*Box::from_raw(lp as *mut Captured)),
        WM_APP_HISTORY_ADD => {
            let entry = *Box::from_raw(lp as *mut history::Entry);
            with_app(|a| a.history.push(entry));
        }
        WM_APP_PIN_CLOSED => with_app(|a| a.pins.retain(|&p| p as WPARAM != wp)),
        WM_APP_CLOSE_PINS => close_pins(),
        WM_APP_EDIT_PIN => edit_pin(wp as HWND),
        WM_APP_START_GIF => start_gif(*Box::from_raw(lp as *mut GifRequest)),
        WM_APP_STOP_GIF => stop_gif(),
        WM_APP_CANCEL_GIF => cancel_gif(),
        WM_APP_GIF_DONE => gif_done(*Box::from_raw(lp as *mut GifOutcome)),
        WM_DESTROY => {
            let (overlay, rec) = with_app(|a| (a.overlay, a.rec.take()));
            if let Some(mut r) = rec {
                r.cancel();
            }
            if !overlay.is_null() && IsWindow(overlay) != 0 {
                DestroyWindow(overlay);
            }
            close_pins();
            tray::remove(hwnd);
            PostQuitMessage(0);
        }
        m if m != 0 && m == TASKBAR_CREATED.load(Relaxed) => {
            // Explorer restarted: the tray icon must be re-added.
            let (icon, recording) = with_app(|a| (a.icon, a.rec.is_some()));
            let icon = if recording { with_app(|a| a.icon_rec) } else { icon };
            tray::add(hwnd, WM_APP_TRAY, icon, TIP_IDLE);
        }
        _ => return DefWindowProcW(hwnd, msg, wp, lp),
    }
    0
}

fn is_capturing() -> bool {
    with_app(|a| a.rec.as_ref().is_some_and(|r| r.is_capturing()))
}

fn on_hotkey(mode: Mode) {
    if is_capturing() {
        stop_gif();
        return;
    }
    let overlay = with_app(|a| a.overlay);
    if !overlay.is_null() && unsafe { IsWindow(overlay) } != 0 {
        return;
    }
    if mode == Mode::Gif && with_app(|a| a.rec.is_some()) {
        notify_busy();
        return;
    }
    // Keep pins out of the new screenshot; they're back when the editor closes.
    show_pins(false);
    let (main, fonts) = with_app(|a| (a.hwnd, a.fonts.clone()));
    let h = overlay::open(main, mode, fonts, None);
    with_app(|a| a.overlay = h);
    if h.is_null() {
        show_pins(true);
    }
}

/// Reopens a pin in the editor, over the screen where it is, with its
/// drawings still editable; finishing updates the pin.
fn edit_pin(target: HWND) {
    let overlay = with_app(|a| a.overlay);
    if (!overlay.is_null() && unsafe { IsWindow(overlay) } != 0) || is_capturing() {
        return;
    }
    let Some(edit) = pin::content(target) else { return };
    show_pins(false);
    let (main, fonts) = with_app(|a| (a.hwnd, a.fonts.clone()));
    let h = overlay::open(main, Mode::Pin, fonts, Some(edit));
    with_app(|a| a.overlay = h);
    if h.is_null() {
        show_pins(true);
    }
}

/// Hides or re-shows all pins. They're windows of this thread, so this runs
/// outside any app borrow.
fn show_pins(show: bool) {
    let pins = with_app(|a| {
        // Only a change: hidden → shown, or shown → hidden.
        if a.pins_hidden == show {
            a.pins_hidden = !show;
            a.pins.clone()
        } else {
            Vec::new()
        }
    });
    if pins.is_empty() {
        return;
    }
    unsafe {
        for p in pins {
            ShowWindow(p, if show { SW_SHOWNOACTIVATE } else { SW_HIDE });
        }
        if !show {
            // Let the desktop redraw without them before the screenshot.
            DwmFlush();
        }
    }
}

fn close_pins() {
    let pins = with_app(|a| std::mem::take(&mut a.pins));
    for p in pins {
        unsafe { DestroyWindow(p) };
    }
}

/// Opens a pin window and keeps track of it.
fn open_pin(w: i32, h: i32, pixels: Vec<u8>, layers: Option<(Vec<u8>, Vec<Shape>)>, at: (i32, i32)) {
    let (main, fonts) = with_app(|a| (a.hwnd, a.fonts.clone()));
    // A capture from a monitor that's gone would open off-screen.
    let at = if capture::virtual_screen().contains(at.0, at.1) {
        at
    } else {
        let mut pt = POINT { x: 0, y: 0 };
        unsafe { GetCursorPos(&mut pt) };
        (pt.x, pt.y)
    };
    let hwnd = pin::open(main, w, h, pixels, layers, at, fonts);
    if !hwnd.is_null() {
        with_app(|a| a.pins.push(hwnd));
    }
}

/// A capture the editor finished: pin it if asked, and keep it in the
/// history (compressed on a worker thread).
fn captured(c: Captured) {
    let main = with_app(|a| a.hwnd) as usize;
    let Captured { w, h, pixels, layers, at, pin, replace } = c;
    match replace.filter(|&t| unsafe { IsWindow(t) } != 0) {
        Some(target) => pin::replace(target, w, h, pixels.clone(), layers),
        // An edited pin that was closed meanwhile comes back.
        None if pin || replace.is_some() => open_pin(w, h, pixels.clone(), layers, at),
        None => {}
    }
    std::thread::spawn(move || {
        if let Some(entry) = history::Entry::new(w, h, &pixels, at) {
            let lp = Box::into_raw(Box::new(entry)) as LPARAM;
            if unsafe { PostMessageW(main as HWND, WM_APP_HISTORY_ADD, 0, lp) } == 0 {
                drop(unsafe { Box::from_raw(lp as *mut history::Entry) });
            }
        }
    });
}

fn pin_history(id: u64) {
    let image = with_app(|a| a.history.get(id).and_then(|e| Some((e.image()?, e.at))));
    if let Some(((w, h, px), at)) = image {
        open_pin(w, h, px, None, at);
    }
}

fn notify_busy() {
    let main = with_app(|a| a.hwnd);
    tray::balloon(main, "Still encoding the previous GIF", "Try again in a moment.", false);
}

fn on_tray(hwnd: HWND, lp: LPARAM) {
    match (lp & 0xFFFF) as u32 {
        NIN_SELECT | NIN_KEYSELECT => {
            if is_capturing() {
                stop_gif();
            } else {
                schedule_capture(hwnd, Mode::Capture);
            }
        }
        WM_CONTEXTMENU => unsafe { show_menu(hwnd) },
        NIN_BALLOONUSERCLICK => {
            if let Some(p) = with_app(|a| a.last_gif.clone()) {
                show_in_folder(&p);
            }
        }
        _ => {}
    }
}

/// Captures after a short delay so the tray menu has faded out.
fn schedule_capture(hwnd: HWND, mode: Mode) {
    with_app(|a| a.pending_mode = mode);
    unsafe { SetTimer(hwnd, TIMER_DELAYED_CAPTURE, 250, None) };
}

/// The "Recent captures" submenu, newest first, with thumbnails. Returns the
/// menu, the entry ids by position, and the bitmaps to delete afterwards.
unsafe fn history_menu() -> (HMENU, Vec<u64>, Vec<HBITMAP>) {
    let scale = GetDpiForSystem() as f32 / 96.0;
    let (max_w, max_h) = ((56.0 * scale) as i32, (36.0 * scale) as i32);
    let items: Vec<(u64, String, (i32, i32, Vec<u8>))> = with_app(|a| {
        a.history
            .iter()
            .map(|e| {
                let (tw, th, px) = &e.thumb;
                let (w, h) = util::fit(*tw, *th, max_w, max_h);
                (e.id, e.label(), (w, h, util::resize(px, *tw, *th, w, h)))
            })
            .collect()
    });
    let menu = CreatePopupMenu();
    let mut ids = Vec::new();
    let mut bitmaps = Vec::new();
    for (i, (id, label, (w, h, px))) in items.into_iter().enumerate() {
        let bmp = tray::make_bitmap(w, h, &px);
        let mut text = wide(&label);
        let mut mi: MENUITEMINFOW = std::mem::zeroed();
        mi.cbSize = size_of::<MENUITEMINFOW>() as u32;
        mi.fMask = MIIM_STRING | MIIM_ID | if bmp.is_null() { 0 } else { MIIM_BITMAP };
        mi.wID = (ID_HISTORY + i) as u32;
        mi.dwTypeData = text.as_mut_ptr();
        mi.hbmpItem = bmp;
        InsertMenuItemW(menu, GetMenuItemCount(menu) as u32, 1, &mi);
        ids.push(id);
        if !bmp.is_null() {
            bitmaps.push(bmp);
        }
    }
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    let clear = wide("Clear history");
    AppendMenuW(menu, MF_STRING, ID_CLEAR_HISTORY, clear.as_ptr());
    (menu, ids, bitmaps)
}

unsafe fn show_menu(hwnd: HWND) {
    let (capturing, fps, cursor, has_history, pins) = with_app(|a| {
        (a.rec.as_ref().is_some_and(|r| r.is_capturing()), a.settings.fps, a.settings.cursor, !a.history.is_empty(), a.pins.len())
    });
    let autostart = settings::autostart_enabled();

    let menu = CreatePopupMenu();
    let item = |m: HMENU, id: usize, text: &str, checked: bool| {
        let w = wide(text);
        AppendMenuW(m, MF_STRING | if checked { MF_CHECKED } else { MF_UNCHECKED }, id, w.as_ptr());
    };
    item(menu, ID_CAPTURE, "Capture area\tPrtSc", false);
    item(menu, ID_PIN, "Capture and pin\tCtrl+PrtSc", false);
    if capturing {
        item(menu, ID_STOP, "Stop GIF recording\tPrtSc", false);
    } else {
        item(menu, ID_GIF, "Record GIF\tShift+PrtSc", false);
    }
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    let mut history_ids = Vec::new();
    let mut bitmaps = Vec::new();
    let recent = wide("Recent captures");
    if has_history {
        let (sub, ids, bmps) = history_menu();
        history_ids = ids;
        bitmaps = bmps;
        AppendMenuW(menu, MF_POPUP, sub as usize, recent.as_ptr());
    } else {
        AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, recent.as_ptr());
    }
    if pins > 0 {
        item(menu, ID_CLOSE_PINS, &if pins == 1 { "Close the pin".to_string() } else { format!("Close all {pins} pins") }, false);
    }
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    let fps_menu = CreatePopupMenu();
    for (i, f) in FPS_CHOICES.iter().enumerate() {
        item(fps_menu, ID_FPS + i, &format!("{f} fps"), *f == fps);
    }
    let label = wide("GIF frame rate");
    AppendMenuW(menu, MF_POPUP, fps_menu as usize, label.as_ptr());
    item(menu, ID_CURSOR, "Show mouse cursor in GIFs", cursor);
    item(menu, ID_OPEN_FOLDER, "Open GIF folder", false);
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    item(menu, ID_AUTOSTART, "Start with Windows", autostart);
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    let about = CreatePopupMenu();
    let version = wide(&format!("{} {}", util::APP_NAME, util::VERSION));
    AppendMenuW(about, MF_STRING | MF_GRAYED, 0, version.as_ptr());
    item(about, ID_HOMEPAGE, &format!("Made by {} (pixobit.eu)", util::MAKER), false);
    item(about, ID_REPOSITORY, "Source code on GitHub", false);
    let label = wide("About");
    AppendMenuW(menu, MF_POPUP, about as usize, label.as_ptr());
    item(menu, ID_EXIT, "Exit", false);
    SetMenuDefaultItem(menu, ID_CAPTURE as u32, 0);

    let mut pt = POINT { x: 0, y: 0 };
    GetCursorPos(&mut pt);
    // Required for the menu to close when clicking elsewhere.
    SetForegroundWindow(hwnd);
    let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON, pt.x, pt.y, 0, hwnd, null()) as usize;
    PostMessageW(hwnd, WM_NULL, 0, 0);
    DestroyMenu(menu);
    for b in bitmaps {
        DeleteObject(b);
    }
    match cmd.checked_sub(ID_HISTORY).and_then(|i| history_ids.get(i)) {
        Some(&id) => pin_history(id),
        None => on_command(hwnd, cmd),
    }
}

unsafe fn on_command(hwnd: HWND, cmd: usize) {
    match cmd {
        ID_CAPTURE => schedule_capture(hwnd, Mode::Capture),
        ID_PIN => schedule_capture(hwnd, Mode::Pin),
        ID_GIF => schedule_capture(hwnd, Mode::Gif),
        ID_STOP => stop_gif(),
        ID_CLEAR_HISTORY => with_app(|a| a.history.clear()),
        ID_CLOSE_PINS => close_pins(),
        ID_CURSOR => with_app(|a| {
            a.settings.cursor = !a.settings.cursor;
            a.settings.save();
        }),
        ID_OPEN_FOLDER => {
            let dir = util::captures_dir();
            let _ = std::fs::create_dir_all(&dir);
            shell_open(&dir);
        }
        ID_AUTOSTART => {
            let on = !settings::autostart_enabled();
            if !settings::set_autostart(on) {
                util::message_box(hwnd, "Could not change the startup setting.", MB_ICONWARNING);
            }
        }
        ID_HOMEPAGE => open_url(util::HOMEPAGE),
        ID_REPOSITORY => open_url(util::REPOSITORY),
        ID_EXIT => {
            DestroyWindow(hwnd);
        }
        c if (ID_FPS..ID_FPS + FPS_CHOICES.len()).contains(&c) => with_app(|a| {
            a.settings.fps = FPS_CHOICES[c - ID_FPS];
            a.settings.save();
        }),
        _ => {}
    }
}

fn shell_open(path: &Path) {
    let verb = wide("open");
    let p = util::wide_path(path);
    unsafe { ShellExecuteW(null_mut(), verb.as_ptr(), p.as_ptr(), null(), null(), SW_SHOWNORMAL) };
}

/// Opens a web page in the default browser.
fn open_url(url: &str) {
    let (verb, u) = (wide("open"), wide(url));
    unsafe { ShellExecuteW(null_mut(), verb.as_ptr(), u.as_ptr(), null(), null(), SW_SHOWNORMAL) };
}

fn show_in_folder(path: &Path) {
    let verb = wide("open");
    let exe = wide("explorer.exe");
    let args = wide(&format!("/select,\"{}\"", path.display()));
    unsafe { ShellExecuteW(null_mut(), verb.as_ptr(), exe.as_ptr(), args.as_ptr(), null(), SW_SHOWNORMAL) };
}

fn start_gif(req: GifRequest) {
    if req.region.w() < 2 || req.region.h() < 2 {
        return;
    }
    if with_app(|a| a.rec.is_some()) {
        notify_busy();
        return;
    }
    let (main, fonts, fps, cursor, id) = with_app(|a| {
        a.next_rec_id += 1;
        (a.hwnd, a.fonts.clone(), a.settings.fps, a.settings.cursor, a.next_rec_id)
    });
    let dir = util::captures_dir();
    let path = dir.join(format!("Recording_{}.gif", util::timestamp()));
    let rec = Recording::start(main, id, req, fps, cursor, fonts, path);
    let icon = with_app(|a| {
        a.rec = Some(rec);
        a.icon_rec
    });
    tray::set(main, icon, "Screen Capture\nRecording GIF — press PrtSc to stop");
}

fn stop_gif() {
    let (main, icon) = with_app(|a| {
        if let Some(r) = a.rec.as_mut() {
            r.stop();
        }
        (a.hwnd, a.icon)
    });
    tray::set(main, icon, "Screen Capture\nEncoding GIF…");
}

fn cancel_gif() {
    let (rec, main, icon) = with_app(|a| (a.rec.take(), a.hwnd, a.icon));
    if let Some(mut r) = rec {
        r.cancel();
    }
    tray::set(main, icon, TIP_IDLE);
}

fn gif_done(out: GifOutcome) {
    let (main, icon, current) = with_app(|a| (a.hwnd, a.icon, a.rec.as_ref().is_some_and(|r| r.id == out.id)));
    if !current {
        // Cancelled while encoding finished: the user doesn't want the file.
        if let Ok(info) = out.result {
            let _ = std::fs::remove_file(info.path);
        }
        return;
    }
    let rec = with_app(|a| a.rec.take());
    drop(rec);
    tray::set(main, icon, TIP_IDLE);
    match out.result {
        Ok(info) => {
            clipboard::set_file(main, &info.path);
            let name = info.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let text = format!(
                "{name}\n{} · {:.1} s · {} frames · {}×{} — copied to clipboard.\nClick to show it in its folder.",
                util::human_size(info.bytes),
                info.duration_ms as f64 / 1000.0,
                info.frames,
                info.w,
                info.h
            );
            tray::balloon(main, "GIF saved", &text, false);
            with_app(|a| a.last_gif = Some(info.path));
        }
        Err(e) if e == gif_enc::CANCELLED => {}
        Err(e) => tray::balloon(main, "GIF recording failed", &e, true),
    }
}
