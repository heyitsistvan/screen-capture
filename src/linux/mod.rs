//! The Linux app: tray icon, PrtSc (an X11 key grab, or the Wayland
//! GlobalShortcuts portal), single instance, the editor window and GIF
//! recording. X11 and Wayland differ mainly in how pixels are captured:
//! directly on X11, through desktop portals on Wayland.

pub mod capture;
mod dialogs;
mod hotkey;
mod notify;
pub mod ocr;
mod portal;
mod record;
mod tray;
mod window;

#[cfg(debug_assertions)]
pub use capture::screen_for_test;

use std::borrow::Cow;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ksni::blocking::TrayMethods;
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalPosition;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::monitor::MonitorHandle;
use winit::platform::wayland::EventLoopExtWayland;
use winit::window::WindowId;

use self::capture::X11;
use self::record::{GifOutcome, Recording, Source};
use self::tray::TrayModel;
use self::window::EditorWindow;
use crate::annotate::Rgba;
use crate::editor::{self, Action, GifRequest, Screen};
use crate::gif_enc;
use crate::recbar::BarBtn;
use crate::settings::{self, Settings};
use crate::text::Fonts;
use crate::util::{self, Rect, APP_NAME};

#[derive(Clone, Copy, Debug)]
pub enum Cmd {
    Capture,
    Gif,
    Stop,
    OpenFolder,
    ToggleCursor,
    SetFps(u32),
    ToggleAutostart,
    /// A web page from the About menu.
    OpenUrl(&'static str),
    Exit,
}

pub enum UserEvent {
    /// PrtSc (with Shift for a GIF); Wayland may pass a focus token.
    Hotkey { gif: bool, activation: Option<String> },
    Command(Cmd),
    /// A Wayland screenshot from the portal.
    Screenshot { result: Result<(i32, i32, Vec<u8>), String>, gif: bool, activation: Option<String> },
    /// Save dialog result: `None` if no dialog program, `Some(None)` if cancelled.
    SaveTo(Option<Option<PathBuf>>),
    ColorChosen { color: Option<Option<Rgba>>, outline: bool },
    CastStarted(Result<portal::Cast, String>),
    RecordingDone(GifOutcome),
    ShortcutsUnavailable(String),
}

const BAR_TICK: Duration = Duration::from_millis(250);

struct App {
    proxy: EventLoopProxy<UserEvent>,
    wayland: bool,
    fonts: Arc<Fonts>,
    settings: Settings,
    editor: Option<EditorWindow>,
    /// The image waiting for the save dialog.
    pending_save: Option<(i32, i32, Vec<u8>)>,
    dialog_open: bool,
    rec: Option<Recording>,
    /// Wayland: a GIF waiting for the screen cast to start.
    pending_gif: Option<(GifRequest, (i32, i32))>,
    next_rec_id: u64,
    next_tick: Instant,
    tray: Option<ksni::blocking::Handle<TrayModel>>,
    /// Owns clipboard contents: on Linux they're served by this process.
    clipboard: Option<arboard::Clipboard>,
    capturing: bool,
    delayed_capture: Option<(Instant, bool)>,
    x11: Option<X11>,
}

pub fn run() {
    install_panic_hook();
    let args: Vec<String> = std::env::args().skip(1).collect();
    #[cfg(debug_assertions)]
    if args.first().map(String::as_str) == Some("--selftest") {
        let dir = args.get(1).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("selftest-out"));
        std::process::exit(crate::selftest::run(&dir, args.get(2).map(std::path::Path::new)));
    }
    // Debug aid for GUI tests: saves the X screen as a PNG.
    #[cfg(debug_assertions)]
    if args.first().map(String::as_str) == Some("--dump-screen") {
        let out = args.get(1).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("screen.png"));
        let png = capture::screen_for_test().and_then(|s| util::encode_png(s.w, s.h, &s.pixels).ok());
        std::process::exit(if png.is_some_and(|p| std::fs::write(&out, p).is_ok()) { 0 } else { 1 });
    }
    #[cfg(debug_assertions)]
    if args.first().map(String::as_str) == Some("--portal-test") {
        let dir = args.get(1).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("portal-test"));
        std::process::exit(portal_test(&dir));
    }
    // Debug aid: records from any PipeWire video node, as if a portal had
    // handed it over (for compositors whose screen cast can't run headless).
    #[cfg(debug_assertions)]
    if args.first().map(String::as_str) == Some("--pipewire-test") {
        let num = |i: usize| args.get(i).and_then(|a| a.parse().ok()).unwrap_or(0);
        std::process::exit(pipewire_test(&PathBuf::from(&args[1]), num(2) as u32, num(3), num(4)));
    }
    let request = args.iter().find_map(|a| match a.as_str() {
        "--capture" => Some("capture"),
        "--gif" => Some("gif"),
        "--stop" => Some("stop"),
        _ => None,
    });

    // One instance: hand the request to the one that's running.
    if let Ok(mut other) = UnixStream::connect(socket_path()) {
        let _ = writeln!(other, "{}", request.unwrap_or("hello"));
        if request.is_none() {
            eprintln!("{APP_NAME} is already running (see the tray icon).");
        }
        return;
    }

    let fonts = match Fonts::load() {
        Ok(f) => Arc::new(f),
        Err(e) => {
            eprintln!("{e}");
            notify::show(APP_NAME, &e);
            return;
        }
    };
    let first_run = !Settings::exists();
    let settings = Settings::load();
    if first_run {
        settings.save();
    }
    editor::apply_outline_setting(&settings.outline);

    let event_loop = match EventLoop::<UserEvent>::with_user_event().build() {
        Ok(el) => el,
        Err(e) => {
            eprintln!("{APP_NAME} needs a graphical session: {e}");
            return;
        }
    };
    let proxy = event_loop.create_proxy();
    let wayland = event_loop.is_wayland();
    listen(proxy.clone());

    let model = TrayModel {
        proxy: proxy.clone(),
        recording: false,
        fps: settings.fps,
        cursor: settings.cursor,
        autostart: settings::autostart_enabled(),
    };
    let tray = match model.spawn() {
        Ok(handle) => Some(handle),
        Err(e) => {
            eprintln!("no tray icon: {e}");
            None
        }
    };

    if wayland {
        let p = proxy.clone();
        portal::runtime().spawn(async move {
            let presses = p.clone();
            let on_press = move |gif, activation| {
                let _ = presses.send_event(UserEvent::Hotkey { gif, activation });
            };
            if let Err(e) = portal::global_shortcuts(on_press).await {
                let _ = p.send_event(UserEvent::ShortcutsUnavailable(e));
            }
        });
    } else {
        let p = proxy.clone();
        let grabbed = hotkey::grab_print(move |gif| {
            let _ = p.send_event(UserEvent::Hotkey { gif, activation: None });
        });
        if let Err(e) = grabbed {
            notify::show("PrtSc isn't available", &format!("{e}. Capture from the tray icon, or bind a key to “{} --capture”.", exe()));
        }
    }

    match request {
        Some("gif") => {
            let _ = proxy.send_event(UserEvent::Command(Cmd::Gif));
        }
        Some(_) => {
            let _ = proxy.send_event(UserEvent::Command(Cmd::Capture));
        }
        None if first_run => notify::show(
            &format!("{APP_NAME} is running"),
            "Press PrtSc to capture an area, Shift+PrtSc to record a GIF.",
        ),
        None => {}
    }

    let mut app = App {
        proxy,
        wayland,
        fonts,
        settings,
        editor: None,
        pending_save: None,
        dialog_open: false,
        rec: None,
        pending_gif: None,
        next_rec_id: 0,
        next_tick: Instant::now(),
        tray,
        clipboard: None,
        capturing: false,
        delayed_capture: None,
        x11: None,
    };
    if let Err(e) = event_loop.run_app(&mut app) {
        eprintln!("{e}");
    }
}

/// Debug aid for testing the Wayland portals headlessly: a portal screenshot
/// and a few seconds of screen cast, saved to `dir`.
#[cfg(debug_assertions)]
fn portal_test(dir: &std::path::Path) -> i32 {
    let _ = std::fs::create_dir_all(dir);
    let rt = portal::runtime();
    let shot = rt.block_on(portal::screenshot());
    let (w, h) = match &shot {
        Ok((w, h, px)) => {
            let saved = util::encode_png(*w, *h, px).map(|png| std::fs::write(dir.join("portal_screen.png"), png));
            println!("ok   portal screenshot: {w}x{h} (saved: {})", matches!(saved, Ok(Ok(()))));
            (*w, *h)
        }
        Err(e) => {
            println!("FAIL portal screenshot: {e}");
            return 1;
        }
    };
    let token = Some(Settings::load().screencast_token).filter(|t| !t.is_empty());
    let cast = match rt.block_on(portal::start_cast(token, true)) {
        Ok(c) => c,
        Err(e) => {
            println!("FAIL screen cast: {e}");
            return 1;
        }
    };
    println!("ok   screen cast: node {}, restore token {}", cast.node, if cast.restore_token.is_some() { "given" } else { "none" });
    let region = Rect::new(w / 4, h / 4, w / 4 + 400, h / 4 + 300).intersect(&Rect::new(0, 0, w, h));
    record_test(cast, region, (w, h), &dir.join("portal_cast.gif"))
}

#[cfg(debug_assertions)]
fn pipewire_test(dir: &std::path::Path, node: u32, w: i32, h: i32) -> i32 {
    let _ = std::fs::create_dir_all(dir);
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_default();
    let fd = match UnixStream::connect(runtime.join("pipewire-0")) {
        Ok(s) => std::os::fd::OwnedFd::from(s),
        Err(e) => {
            println!("FAIL pipewire connection: {e}");
            return 1;
        }
    };
    let (stop, _stopped) = tokio::sync::oneshot::channel();
    let cast = portal::Cast { fd, node, restore_token: None, stop };
    record_test(cast, Rect::new(w / 4, h / 4, w * 3 / 4, h * 3 / 4), (w, h), &dir.join("pipewire_cast.gif"))
}

#[cfg(debug_assertions)]
fn record_test(cast: portal::Cast, region: Rect, screen: (i32, i32), out: &std::path::Path) -> i32 {
    let t = Instant::now();
    match record::record_cast_for_test(cast, region, screen, 3.0, out) {
        Ok(info) if info.frames > 0 => {
            println!("ok   cast recording: {}x{}, {} frames, {} bytes, in {:?}", info.w, info.h, info.frames, info.bytes, t.elapsed());
            0
        }
        Ok(_) => {
            println!("FAIL cast recording: no frames arrived");
            1
        }
        Err(e) => {
            println!("FAIL cast recording: {e}");
            1
        }
    }
}

fn exe() -> String {
    std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "screen-capture".into())
}

fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let msg = info.to_string();
        let dir = util::app_data_dir();
        let _ = std::fs::create_dir_all(&dir);
        let log = dir.join("crash.log");
        let previous = std::fs::read_to_string(&log).unwrap_or_default();
        let _ = std::fs::write(&log, format!("{previous}{} {msg}\n", util::timestamp()));
        eprintln!("{APP_NAME} hit an unexpected error: {msg} (details in {})", log.display());
    }));
}

fn socket_path() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => PathBuf::from(dir).join("screen-capture.sock"),
        None => std::env::temp_dir().join(format!("screen-capture-{}.sock", unsafe { libc::getuid() })),
    }
}

/// Accepts commands from later instances (`--capture`, `--gif`, `--stop`).
fn listen(proxy: EventLoopProxy<UserEvent>) {
    let path = socket_path();
    let _ = std::fs::remove_file(&path); // a stale socket from a crash
    let Ok(listener) = UnixListener::bind(&path) else { return };
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut line = String::new();
            if BufReader::new(stream).read_line(&mut line).is_err() {
                continue;
            }
            let cmd = match line.trim() {
                "capture" => Cmd::Capture,
                "gif" => Cmd::Gif,
                "stop" => Cmd::Stop,
                _ => continue,
            };
            let _ = proxy.send_event(UserEvent::Command(cmd));
        }
    });
}

fn monitor_rect(m: &MonitorHandle) -> Rect {
    let (p, s) = (m.position(), m.size());
    Rect::xywh(p.x, p.y, s.width as i32, s.height as i32)
}

impl App {
    fn update_tray(&self) {
        let (recording, fps, cursor) = (self.rec.as_ref().is_some_and(Recording::is_capturing), self.settings.fps, self.settings.cursor);
        if let Some(t) = &self.tray {
            t.update(|m| {
                m.recording = recording;
                m.fps = fps;
                m.cursor = cursor;
                m.autostart = settings::autostart_enabled();
            });
        }
    }

    /// Closes the editor; its color, sizes and outline carry over, and the
    /// outline choice is remembered across restarts.
    fn close_editor(&mut self) {
        self.editor = None;
        let outline = editor::outline_setting();
        if outline != self.settings.outline {
            self.settings.outline = outline;
            self.settings.save();
        }
    }

    fn clipboard(&mut self) -> Option<&mut arboard::Clipboard> {
        if self.clipboard.is_none() {
            self.clipboard = arboard::Clipboard::new().ok();
        }
        self.clipboard.as_mut()
    }

    /// PrtSc, the tray or `--capture`: open the editor (or stop a recording).
    fn begin_capture(&mut self, el: &ActiveEventLoop, gif: bool, activation: Option<String>) {
        if self.rec.as_ref().is_some_and(Recording::is_capturing) {
            self.stop_gif();
            return;
        }
        if self.editor.is_some() || self.capturing {
            return;
        }
        if gif && self.rec.is_some() {
            notify::show("Still encoding the previous GIF", "Try again in a moment.");
            return;
        }
        if self.wayland {
            self.capturing = true;
            let proxy = self.proxy.clone();
            portal::runtime().spawn(async move {
                let result = portal::screenshot().await;
                let _ = proxy.send_event(UserEvent::Screenshot { result, gif, activation });
            });
            return;
        }
        if self.x11.is_none() {
            self.x11 = X11::connect();
        }
        let Some(screen) = self.x11.as_ref().and_then(capture::monitor_screen) else {
            notify::show("Couldn't capture the screen", "The X server refused the screenshot.");
            return;
        };
        let origin = PhysicalPosition::new(screen.origin.0, screen.origin.1);
        let monitor = el.available_monitors().find(|m| m.position() == origin);
        self.open_editor(el, screen, monitor, gif, None);
    }

    fn open_editor(&mut self, el: &ActiveEventLoop, screen: Screen, monitor: Option<MonitorHandle>, gif: bool, activation: Option<String>) {
        match EditorWindow::open(el, screen, monitor, self.fonts.clone(), gif, activation) {
            Ok(w) => self.editor = Some(w),
            Err(e) => notify::show("Couldn't open the editor", &e),
        }
    }

    /// Wayland: the portal's screenshot covers the whole desktop; cut out the
    /// monitor the editor goes on.
    fn screenshot_arrived(&mut self, el: &ActiveEventLoop, w: i32, h: i32, px: Vec<u8>, gif: bool, activation: Option<String>) {
        let monitors: Vec<MonitorHandle> = el.available_monitors().collect();
        let target = el.primary_monitor().or_else(|| monitors.first().cloned());
        let desktop = monitors.iter().map(monitor_rect).fold(Rect::default(), |a, r| a.union(&r));
        let crop = match (&target, monitors.len()) {
            (Some(m), n) if n > 1 && !desktop.is_empty() => {
                let r = monitor_rect(m).offset(-desktop.l, -desktop.t);
                let (sx, sy) = (w as f32 / desktop.w() as f32, h as f32 / desktop.h() as f32);
                Rect::new((r.l as f32 * sx) as i32, (r.t as f32 * sy) as i32, (r.r as f32 * sx) as i32, (r.b as f32 * sy) as i32)
                    .intersect(&Rect::new(0, 0, w, h))
            }
            _ => Rect::new(0, 0, w, h),
        };
        if crop.is_empty() {
            notify::show("Couldn't capture the screen", "The screenshot didn't match the monitors.");
            return;
        }
        let mut pixels = Vec::with_capacity(crop.w() as usize * crop.h() as usize * 4);
        for y in crop.t..crop.b {
            let i = ((y * w + crop.l) * 4) as usize;
            pixels.extend_from_slice(&px[i..i + crop.w() as usize * 4]);
        }
        let local = Rect::new(0, 0, crop.w(), crop.h());
        let screen = Screen {
            w: crop.w(),
            h: crop.h(),
            pixels,
            monitors: vec![local],
            // Wayland doesn't reveal window positions, so a click selects the monitor.
            windows: Vec::new(),
            mouse: local.center(),
            origin: (0, 0),
        };
        self.open_editor(el, screen, target, gif, activation);
    }

    fn perform(&mut self, el: &ActiveEventLoop, action: Action) {
        let Some(ed) = self.editor.as_mut() else { return };
        match action {
            Action::Close => self.close_editor(),
            Action::Copy => {
                let Some((w, h, px)) = ed.editor.final_image() else { return };
                let rgba: Vec<u8> = px.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0], 255]).collect();
                let image = arboard::ImageData { width: w as usize, height: h as usize, bytes: Cow::Owned(rgba) };
                if self.clipboard().is_some_and(|c| c.set_image(image).is_ok()) {
                    self.close_editor();
                } else if let Some(ed) = self.editor.as_mut() {
                    ed.editor.show_flash("Couldn't copy to the clipboard, try again".into());
                    ed.sync();
                }
            }
            Action::Save => {
                if self.dialog_open {
                    return;
                }
                let Some(image) = ed.editor.final_image() else { return };
                let parent = ed.x11_id();
                self.pending_save = Some(image);
                self.dialog_open = true;
                let proxy = self.proxy.clone();
                std::thread::spawn(move || {
                    let default = util::pictures_dir().join(format!("Screenshot_{}.png", util::timestamp()));
                    let _ = proxy.send_event(UserEvent::SaveTo(dialogs::save_png(&default, parent)));
                });
            }
            Action::Gif => {
                let req = ed.editor.gif_request();
                let size = ed.editor.size();
                self.close_editor();
                if let Some(req) = req {
                    self.start_gif(el, req, size);
                }
            }
            Action::ChooseColor | Action::ChooseOutlineColor => {
                if self.dialog_open {
                    return;
                }
                let outline = matches!(action, Action::ChooseOutlineColor);
                let current = if outline { ed.editor.outline_custom() } else { ed.editor.color() };
                let parent = ed.x11_id();
                self.dialog_open = true;
                let proxy = self.proxy.clone();
                std::thread::spawn(move || {
                    let _ = proxy.send_event(UserEvent::ColorChosen { color: dialogs::color(current, parent), outline });
                });
            }
            Action::CopyColor => {
                let hex = ed.editor.color_under_cursor();
                let ok = self.clipboard().is_some_and(|c| c.set_text(hex.clone()).is_ok());
                if let Some(ed) = self.editor.as_mut() {
                    ed.editor.show_flash(if ok { format!("Copied {hex}") } else { "Couldn't copy to the clipboard".into() });
                    ed.sync();
                }
            }
        }
    }

    fn save_chosen(&mut self, choice: Option<Option<PathBuf>>) {
        self.dialog_open = false;
        let Some((w, h, px)) = self.pending_save.take() else { return };
        let quick = choice.is_none();
        let path = match choice {
            Some(None) => return, // cancelled: keep editing
            Some(Some(path)) => path,
            None => {
                // No dialog program: save next to the GIFs and say where.
                let dir = util::captures_dir();
                let _ = std::fs::create_dir_all(&dir);
                dir.join(format!("Screenshot_{}.png", util::timestamp()))
            }
        };
        match util::encode_png(w, h, &px).and_then(|png| std::fs::write(&path, png).map_err(|e| e.to_string())) {
            Ok(()) => {
                if quick {
                    notify::show_file("Screenshot saved", &path.display().to_string(), &path);
                }
                self.close_editor();
            }
            Err(e) => notify::show("Couldn't save the screenshot", &format!("{}: {e}", path.display())),
        }
    }

    fn color_chosen(&mut self, color: Option<Option<Rgba>>, outline: bool) {
        self.dialog_open = false;
        let Some(ed) = self.editor.as_mut() else { return };
        match color {
            None => notify::show("No color picker", "Install zenity or kdialog to pick custom colors."),
            Some(None) => {}
            Some(Some(c)) => {
                if outline {
                    ed.editor.set_custom_outline(c);
                } else {
                    ed.editor.set_color(c);
                }
                ed.sync();
            }
        }
    }

    fn start_gif(&mut self, el: &ActiveEventLoop, req: GifRequest, screen: (i32, i32)) {
        if self.rec.is_some() || self.pending_gif.is_some() {
            notify::show("Still encoding the previous GIF", "Try again in a moment.");
            return;
        }
        if req.region.w() < 2 || req.region.h() < 2 {
            return;
        }
        if !self.wayland {
            self.launch(el, req, Source::X11);
            return;
        }
        self.pending_gif = Some((req, screen));
        let token = Some(self.settings.screencast_token.clone()).filter(|t| !t.is_empty());
        let (cursor, proxy) = (self.settings.cursor, self.proxy.clone());
        portal::runtime().spawn(async move {
            let _ = proxy.send_event(UserEvent::CastStarted(portal::start_cast(token, cursor).await));
        });
    }

    fn launch(&mut self, el: &ActiveEventLoop, req: GifRequest, source: Source) {
        self.next_rec_id += 1;
        let dir = util::captures_dir();
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("Recording_{}.gif", util::timestamp()));
        let (fps, cursor) = (self.settings.fps, self.settings.cursor);
        self.rec =
            Some(Recording::start(el, self.proxy.clone(), self.next_rec_id, req, fps, cursor, self.fonts.clone(), path, source));
        self.next_tick = Instant::now();
        self.update_tray();
    }

    fn stop_gif(&mut self) {
        if let Some(r) = self.rec.as_mut() {
            r.stop();
        }
        self.update_tray();
    }

    fn cancel_gif(&mut self) {
        if let Some(mut r) = self.rec.take() {
            r.cancel();
        }
        self.update_tray();
    }

    fn gif_done(&mut self, out: GifOutcome) {
        if self.rec.as_ref().is_none_or(|r| r.id != out.id) {
            // Cancelled while encoding finished: the user doesn't want the file.
            if let Ok(info) = out.result {
                let _ = std::fs::remove_file(info.path);
            }
            return;
        }
        self.rec = None;
        self.update_tray();
        match out.result {
            Ok(info) => {
                let copied = self.clipboard().is_some_and(|c| c.set().file_list(&[&info.path]).is_ok());
                let name = info.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let body = format!(
                    "{name}\n{} · {:.1} s · {} frames · {}×{}{}",
                    util::human_size(info.bytes),
                    info.duration_ms as f64 / 1000.0,
                    info.frames,
                    info.w,
                    info.h,
                    if copied { " — copied to the clipboard" } else { "" }
                );
                notify::show_file("GIF saved", &body, &info.path);
            }
            Err(e) if e == gif_enc::CANCELLED => {}
            Err(e) => notify::show("GIF recording failed", &e),
        }
    }

    fn command(&mut self, el: &ActiveEventLoop, cmd: Cmd) {
        match cmd {
            // A short delay lets the tray menu close before the screenshot.
            Cmd::Capture => self.delayed_capture = Some((Instant::now() + Duration::from_millis(250), false)),
            Cmd::Gif => self.delayed_capture = Some((Instant::now() + Duration::from_millis(250), true)),
            Cmd::Stop => self.stop_gif(),
            Cmd::OpenFolder => {
                let dir = util::captures_dir();
                let _ = std::fs::create_dir_all(&dir);
                notify::open(&dir);
            }
            Cmd::ToggleCursor => {
                self.settings.cursor = !self.settings.cursor;
                self.settings.save();
            }
            Cmd::SetFps(f) => {
                self.settings.fps = f;
                self.settings.save();
            }
            Cmd::ToggleAutostart => {
                if !settings::set_autostart(!settings::autostart_enabled()) {
                    notify::show("Couldn't change the startup setting", "");
                }
            }
            Cmd::OpenUrl(url) => notify::open(url),
            Cmd::Exit => {
                if let Some(mut r) = self.rec.take() {
                    r.cancel();
                }
                if let Some(t) = self.tray.take() {
                    t.shutdown();
                }
                let _ = std::fs::remove_file(socket_path());
                el.exit();
            }
        }
        self.update_tray();
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, _el: &ActiveEventLoop) {}

    fn new_events(&mut self, el: &ActiveEventLoop, _cause: winit::event::StartCause) {
        let now = Instant::now();
        if let Some(ed) = self.editor.as_mut() {
            if ed.next_timer().is_some_and(|t| t <= now) {
                ed.fire_timers();
            }
        }
        if let Some((at, gif)) = self.delayed_capture {
            if at <= now {
                self.delayed_capture = None;
                self.begin_capture(el, gif, None);
            }
        }
        if self.rec.is_some() && self.next_tick <= now {
            self.next_tick = now + BAR_TICK;
            if let Some(r) = &self.rec {
                r.tick();
            }
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        let deadlines = [
            self.editor.as_ref().and_then(EditorWindow::next_timer),
            self.delayed_capture.map(|d| d.0),
            self.rec.as_ref().map(|_| self.next_tick),
        ];
        match deadlines.into_iter().flatten().min() {
            Some(t) => el.set_control_flow(ControlFlow::WaitUntil(t)),
            None => el.set_control_flow(ControlFlow::Wait),
        }
    }

    fn user_event(&mut self, el: &ActiveEventLoop, ev: UserEvent) {
        match ev {
            UserEvent::Hotkey { gif, activation } => self.begin_capture(el, gif, activation),
            UserEvent::Command(cmd) => self.command(el, cmd),
            UserEvent::Screenshot { result, gif, activation } => {
                self.capturing = false;
                match result {
                    Ok((w, h, px)) => self.screenshot_arrived(el, w, h, px, gif, activation),
                    Err(e) => notify::show("Couldn't take a screenshot", &e),
                }
            }
            UserEvent::SaveTo(choice) => self.save_chosen(choice),
            UserEvent::ColorChosen { color, outline } => self.color_chosen(color, outline),
            UserEvent::CastStarted(result) => {
                let pending = self.pending_gif.take();
                match (result, pending) {
                    (Ok(cast), Some((req, screen))) => {
                        if let Some(token) = &cast.restore_token {
                            self.settings.screencast_token = token.clone();
                            self.settings.save();
                        }
                        self.launch(el, req, Source::Cast { cast, screen });
                    }
                    (Err(e), _) => notify::show("Couldn't start recording", &e),
                    (Ok(_), None) => {}
                }
            }
            UserEvent::RecordingDone(out) => self.gif_done(out),
            UserEvent::ShortcutsUnavailable(reason) => {
                eprintln!("global shortcuts unavailable: {reason}");
                if !self.settings.shortcut_hint_shown {
                    self.settings.shortcut_hint_shown = true;
                    self.settings.save();
                    notify::show(
                        "Set up the PrtSc shortcut",
                        &format!(
                            "Your desktop doesn't let apps register shortcuts. In Settings → Keyboard → Custom Shortcuts, \
                             add PrtSc running “{0} --capture” and Shift+PrtSc running “{0} --gif”.",
                            exe()
                        ),
                    );
                }
            }
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, id: WindowId, ev: WindowEvent) {
        if self.editor.as_ref().is_some_and(|e| e.window.id() == id) {
            let action = self.editor.as_mut().and_then(|e| e.event(ev));
            if let Some(a) = action {
                self.perform(el, a);
            }
            return;
        }
        let clicked = match self.rec.as_mut() {
            Some(r) if r.owns(id) => r.window_event(id, ev),
            _ => None,
        };
        match clicked {
            Some(BarBtn::Stop) => self.stop_gif(),
            Some(BarBtn::Cancel) => self.cancel_gif(),
            None => {}
        }
    }
}
