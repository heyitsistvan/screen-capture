//! The Windows app: tray icon, PrtSc keyboard hook, the editor window and
//! GIF recording.

pub mod capture;
pub mod clipboard;
pub mod gif_rec;
pub mod ocr;
pub mod overlay;
pub mod tray;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering::Relaxed};
use std::sync::Arc;

use windows_sys::Win32::Foundation::{
    GetLastError, ERROR_ALREADY_EXISTS, FALSE, HWND, LPARAM, LRESULT, POINT, WPARAM,
};
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
    DispatchMessageW, FindWindowW, GetCursorPos, GetMessageW, IsWindow, KillTimer, PostMessageW, PostQuitMessage,
    RegisterClassExW, RegisterWindowMessageW, SetForegroundWindow, SetMenuDefaultItem, SetTimer, SetWindowsHookExW,
    TrackPopupMenu, TranslateMessage, UnhookWindowsHookEx, HC_ACTION, HICON, HMENU, KBDLLHOOKSTRUCT, LLKHF_ALTDOWN,
    MB_ICONERROR, MB_ICONINFORMATION, MB_ICONWARNING, MF_CHECKED, MF_GRAYED, MF_POPUP, MF_SEPARATOR, MF_STRING, MF_UNCHECKED,
    MSG, SM_CXSMICON, SW_SHOWNORMAL, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, WH_KEYBOARD_LL, WM_APP,
    WM_CONTEXTMENU, WM_DESTROY, WM_KEYDOWN, WM_KEYUP, WM_NULL, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_TIMER, WNDCLASSEXW,
    WS_POPUP,
};

use crate::editor::GifRequest;
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

const NIN_KEYSELECT: u32 = NIN_SELECT | NINF_KEY;

const MAIN_CLASS: &str = "ScreenCaptureMain";
const TIMER_DELAYED_CAPTURE: usize = 1;
const TIP_IDLE: &str = "Screen Capture\nPrtSc: capture · Shift+PrtSc: record GIF";

const ID_CAPTURE: usize = 1;
const ID_GIF: usize = 2;
const ID_STOP: usize = 3;
const ID_OPEN_FOLDER: usize = 4;
const ID_AUTOSTART: usize = 5;
const ID_CURSOR: usize = 6;
const ID_HOMEPAGE: usize = 7;
const ID_REPOSITORY: usize = 8;
const ID_EXIT: usize = 9;
const ID_FPS: usize = 100;

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
    pending_gif: bool,
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
        Some(1)
    } else if args.iter().any(|a| a == "--capture") {
        Some(0)
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
                pending_gif: false,
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
                "Press PrtSc to capture an area, Shift+PrtSc to record a GIF.",
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
                // Alt/Ctrl/Win+PrtSc stay with Windows.
                let modifiers = kb.flags & LLKHF_ALTDOWN != 0
                    || held(VK_MENU)
                    || held(VK_CONTROL)
                    || held(VK_LWIN)
                    || held(VK_RWIN);
                if !modifiers {
                    if !PRTSC_DOWN.swap(true, Relaxed) {
                        let main = MAIN_HWND.load(Relaxed) as HWND;
                        PostMessageW(main, WM_APP_HOTKEY, held(VK_SHIFT) as usize, 0);
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
        WM_APP_HOTKEY => on_hotkey(wp == 1),
        WM_APP_TRAY => on_tray(hwnd, lp),
        WM_TIMER if wp == TIMER_DELAYED_CAPTURE => {
            KillTimer(hwnd, TIMER_DELAYED_CAPTURE);
            let gif = with_app(|a| a.pending_gif);
            on_hotkey(gif);
        }
        WM_APP_OVERLAY_DONE => with_app(|a| {
            if a.overlay as usize == wp {
                a.overlay = null_mut();
            }
            // Remember the outline choice across restarts.
            let outline = editor::outline_setting();
            if outline != a.settings.outline {
                a.settings.outline = outline;
                a.settings.save();
            }
        }),
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

fn on_hotkey(gif: bool) {
    if is_capturing() {
        stop_gif();
        return;
    }
    let overlay = with_app(|a| a.overlay);
    if !overlay.is_null() && unsafe { IsWindow(overlay) } != 0 {
        return;
    }
    if gif && with_app(|a| a.rec.is_some()) {
        notify_busy();
        return;
    }
    let (main, fonts) = with_app(|a| (a.hwnd, a.fonts.clone()));
    let h = overlay::open(main, gif, fonts);
    with_app(|a| a.overlay = h);
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
                schedule_capture(hwnd, false);
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
fn schedule_capture(hwnd: HWND, gif: bool) {
    with_app(|a| a.pending_gif = gif);
    unsafe { SetTimer(hwnd, TIMER_DELAYED_CAPTURE, 250, None) };
}

unsafe fn show_menu(hwnd: HWND) {
    let (capturing, fps, cursor) =
        with_app(|a| (a.rec.as_ref().is_some_and(|r| r.is_capturing()), a.settings.fps, a.settings.cursor));
    let autostart = settings::autostart_enabled();

    let menu = CreatePopupMenu();
    let item = |m: HMENU, id: usize, text: &str, checked: bool| {
        let w = wide(text);
        AppendMenuW(m, MF_STRING | if checked { MF_CHECKED } else { MF_UNCHECKED }, id, w.as_ptr());
    };
    item(menu, ID_CAPTURE, "Capture area\tPrtSc", false);
    if capturing {
        item(menu, ID_STOP, "Stop GIF recording\tPrtSc", false);
    } else {
        item(menu, ID_GIF, "Record GIF\tShift+PrtSc", false);
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
    let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON, pt.x, pt.y, 0, hwnd, null());
    PostMessageW(hwnd, WM_NULL, 0, 0);
    DestroyMenu(menu);
    on_command(hwnd, cmd as usize);
}

unsafe fn on_command(hwnd: HWND, cmd: usize) {
    match cmd {
        ID_CAPTURE => schedule_capture(hwnd, false),
        ID_GIF => schedule_capture(hwnd, true),
        ID_STOP => stop_gif(),
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
