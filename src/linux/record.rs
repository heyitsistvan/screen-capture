//! A GIF recording session on Linux: frames from X11 or from a Wayland
//! screen cast (PipeWire), the shared encoder, and the control bar. On X11,
//! where windows can be placed exactly, a red frame also marks the area.

use std::num::NonZeroU32;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use pipewire as pw;
use pw::spa;
use tokio::sync::oneshot;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoopProxy};
use winit::platform::x11::{WindowAttributesExtX11, WindowType};
use winit::window::{Window, WindowAttributes, WindowId, WindowLevel};

use super::capture::Grabber;
use super::portal::Cast;
use super::UserEvent;
use crate::annotate;
use crate::editor::GifRequest;
use crate::gif_enc::{self, GifInfo, Msg};
use crate::recbar::{self, BarBtn, BarState};
use crate::text::Fonts;
use crate::ui::Canvas;
use crate::util::Rect;

/// Sent through the event loop when encoding ends.
pub struct GifOutcome {
    pub id: u64,
    pub result: Result<GifInfo, String>,
}

pub enum Source {
    X11,
    /// A Wayland screen cast. `screen` is the size of the screenshot the area
    /// was picked on, to map it onto the stream.
    Cast { cast: Cast, screen: (i32, i32) },
}

struct Shared {
    stop: AtomicBool,
    cancel: AtomicBool,
    frames: AtomicUsize,
    processed: AtomicUsize,
    start: Instant,
}

/// A small undecorated window drawn with softbuffer.
struct Panel {
    window: Rc<Window>,
    _context: softbuffer::Context<Rc<Window>>,
    surface: softbuffer::Surface<Rc<Window>, Rc<Window>>,
}

impl Panel {
    fn open(el: &ActiveEventLoop, attrs: WindowAttributes) -> Option<Panel> {
        let window = Rc::new(el.create_window(attrs).ok()?);
        let context = softbuffer::Context::new(window.clone()).ok()?;
        let surface = softbuffer::Surface::new(&context, window.clone()).ok()?;
        Some(Panel { window, _context: context, surface })
    }

    /// Presents BGRA pixels the size of the window.
    fn present(&mut self, draw: impl FnOnce(&mut [u8], i32, i32)) {
        let size = self.window.inner_size();
        let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else { return };
        if self.surface.resize(w, h).is_err() {
            return;
        }
        let mut px = vec![0u8; size.width as usize * size.height as usize * 4];
        draw(&mut px, size.width as i32, size.height as i32);
        let Ok(mut buf) = self.surface.buffer_mut() else { return };
        for (d, s) in buf.iter_mut().zip(px.chunks_exact(4)) {
            *d = u32::from_le_bytes([s[0], s[1], s[2], 0]);
        }
        let _ = buf.present();
    }
}

pub struct Recording {
    pub id: u64,
    shared: Arc<Shared>,
    bar: Option<Panel>,
    borders: Vec<Panel>,
    stop_cast: Option<oneshot::Sender<()>>,
    fonts: Arc<Fonts>,
    scale: f32,
    hover: Option<BarBtn>,
}

impl Recording {
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        el: &ActiveEventLoop,
        proxy: EventLoopProxy<UserEvent>,
        id: u64,
        req: GifRequest,
        fps: u32,
        cursor: bool,
        fonts: Arc<Fonts>,
        out: PathBuf,
        source: Source,
    ) -> Recording {
        let r = req.region;
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            cancel: AtomicBool::new(false),
            frames: AtomicUsize::new(0),
            processed: AtomicUsize::new(0),
            start: Instant::now(),
        });
        let x11 = matches!(source, Source::X11);
        let monitor = el
            .available_monitors()
            .find(|m| {
                let (p, s) = (m.position(), m.size());
                Rect::xywh(p.x, p.y, s.width as i32, s.height as i32).contains(r.center().0, r.center().1)
            })
            .or_else(|| el.primary_monitor());
        let scale = monitor.as_ref().map_or(1.0, |m| m.scale_factor() as f32);
        let mon = monitor.map_or(Rect::xywh(0, 0, 1920, 1080), |m| {
            let (p, s) = (m.position(), m.size());
            Rect::xywh(p.x, p.y, s.width as i32, s.height as i32)
        });

        let (bw, bh) = recbar::size(scale, &fonts);
        let mut bar_attrs = Window::default_attributes()
            .with_title("Recording GIF")
            .with_decorations(false)
            .with_resizable(false)
            .with_window_level(WindowLevel::AlwaysOnTop)
            .with_inner_size(PhysicalSize::new(bw as u32, bh as u32));
        bar_attrs = WindowAttributesExtX11::with_name(bar_attrs, "screen-capture", "screen-capture");
        let mut borders = Vec::new();
        if x11 {
            // Exact placement is only possible on X11: frame the area and put
            // the bar just outside it.
            let th = ((2.0 * scale).round() as i32).max(2);
            for b in [
                Rect::new(r.l - th, r.t - th, r.r + th, r.t),
                Rect::new(r.l - th, r.b, r.r + th, r.b + th),
                Rect::new(r.l - th, r.t, r.l, r.b),
                Rect::new(r.r, r.t, r.r + th, r.b),
            ] {
                let attrs = Window::default_attributes()
                    .with_decorations(false)
                    .with_override_redirect(true)
                    .with_x11_window_type(vec![WindowType::Notification])
                    .with_position(PhysicalPosition::new(b.l, b.t))
                    .with_inner_size(PhysicalSize::new(b.w().max(1) as u32, b.h().max(1) as u32));
                if let Some(p) = Panel::open(el, attrs) {
                    let _ = p.window.set_cursor_hittest(false);
                    p.window.request_redraw();
                    borders.push(p);
                }
            }
            let gap = th + (6.0 * scale) as i32;
            let mut y = r.b + gap;
            if y + bh > mon.b {
                y = if r.t - gap - bh >= mon.t { r.t - gap - bh } else { r.b - bh - gap };
            }
            let x = (r.r - bw).min(mon.r - bw).max(mon.l);
            bar_attrs = bar_attrs
                .with_override_redirect(true)
                .with_x11_window_type(vec![WindowType::Notification])
                .with_position(PhysicalPosition::new(x, y));
        }
        let bar = Panel::open(el, bar_attrs);
        if let Some(b) = &bar {
            b.window.request_redraw();
        }

        let (tx, rx) = mpsc::channel();
        let mut stop_cast = None;
        match source {
            Source::X11 => {
                let sh = shared.clone();
                thread::spawn(move || x11_loop(r, fps, cursor, &sh, tx));
            }
            Source::Cast { cast, screen } => {
                stop_cast = Some(cast.stop);
                let sh = shared.clone();
                let local = r; // Wayland screens have their origin at (0, 0)
                thread::spawn(move || cast_loop(cast.fd, cast.node, local, screen, fps, sh, tx));
            }
        }

        let sh = shared.clone();
        let enc_fonts = fonts.clone();
        thread::spawn(move || {
            let (w, h) = (r.w(), r.h());
            let shapes = req.shapes;
            let overlay = |px: &mut [u32]| {
                if !shapes.is_empty() {
                    annotate::render(bytemuck::cast_slice_mut(px), w, h, &shapes, 0.0, 0.0, &enc_fonts);
                }
            };
            let result = gif_enc::encode(rx, w, h, overlay, &out, &sh.cancel, &sh.processed);
            let _ = proxy.send_event(UserEvent::RecordingDone(GifOutcome { id, result }));
        });

        Recording { id, shared, bar, borders, stop_cast, fonts, scale, hover: None }
    }

    pub fn is_capturing(&self) -> bool {
        !self.shared.stop.load(Relaxed)
    }

    /// Stops capturing; the frames already captured keep encoding.
    pub fn stop(&mut self) {
        self.shared.stop.store(true, Relaxed);
        self.borders.clear();
        if let Some(stop) = self.stop_cast.take() {
            let _ = stop.send(());
        }
        self.tick();
    }

    /// Stops and discards the recording.
    pub fn cancel(&mut self) {
        self.shared.cancel.store(true, Relaxed);
        self.stop();
    }

    /// Repaints the bar (time and progress move on).
    pub fn tick(&self) {
        if let Some(b) = &self.bar {
            b.window.request_redraw();
        }
    }

    pub fn owns(&self, id: WindowId) -> bool {
        self.bar.as_ref().is_some_and(|b| b.window.id() == id) || self.borders.iter().any(|b| b.window.id() == id)
    }

    /// Handles an event for one of the session's windows; returns a clicked
    /// bar button.
    pub fn window_event(&mut self, id: WindowId, ev: WindowEvent) -> Option<BarBtn> {
        if let Some(border) = self.borders.iter_mut().find(|b| b.window.id() == id) {
            if matches!(ev, WindowEvent::RedrawRequested) {
                border.present(|px, _, _| {
                    for p in px.chunks_exact_mut(4) {
                        p.copy_from_slice(&[recbar::RED.2, recbar::RED.1, recbar::RED.0, 255]);
                    }
                });
            }
            return None;
        }
        let encoding = !self.is_capturing();
        let (scale, hover) = (self.scale, self.hover);
        let state = BarState {
            encoding,
            elapsed_ms: self.shared.start.elapsed().as_millis() as u64,
            frames: self.shared.frames.load(Relaxed),
            processed: self.shared.processed.load(Relaxed),
        };
        let fonts = self.fonts.clone();
        let bar = self.bar.as_mut()?;
        let size = bar.window.inner_size();
        let (w, h) = (size.width as i32, size.height as i32);
        match ev {
            WindowEvent::RedrawRequested => {
                bar.present(|px, w, h| recbar::draw(&mut Canvas::new(px, w, h), scale, &state, hover, &fonts));
                None
            }
            WindowEvent::CursorMoved { position, .. } => {
                let hv = recbar::hit(w, h, scale, encoding, position.x as i32, position.y as i32);
                if hv != self.hover {
                    self.hover = hv;
                    bar.window.request_redraw();
                }
                None
            }
            WindowEvent::CursorLeft { .. } => {
                self.hover = None;
                bar.window.request_redraw();
                None
            }
            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Left, .. } => {
                let clicked = self.hover;
                if clicked.is_none() {
                    // The background moves the bar out of the way (Wayland and
                    // window-managed X11 only).
                    let _ = bar.window.drag_window();
                }
                clicked
            }
            _ => None,
        }
    }
}

/// Debug aid for testing on Wayland: records `secs` of an area from a screen
/// cast, without any windows.
#[cfg(debug_assertions)]
pub fn record_cast_for_test(cast: Cast, region: Rect, screen: (i32, i32), secs: f32, out: &std::path::Path) -> Result<GifInfo, String> {
    let shared = Arc::new(Shared {
        stop: AtomicBool::new(false),
        cancel: AtomicBool::new(false),
        frames: AtomicUsize::new(0),
        processed: AtomicUsize::new(0),
        start: Instant::now(),
    });
    let (tx, rx) = mpsc::channel();
    let Cast { fd, node, stop, .. } = cast;
    let sh = shared.clone();
    thread::spawn(move || cast_loop(fd, node, region, screen, 10, sh, tx));
    let sh = shared.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_secs_f32(secs));
        sh.stop.store(true, Relaxed);
    });
    let result = gif_enc::encode(rx, region.w(), region.h(), |_| {}, out, &shared.cancel, &shared.processed);
    let _ = stop.send(());
    result
}

fn x11_loop(r: Rect, fps: u32, cursor: bool, sh: &Shared, tx: mpsc::Sender<Msg>) {
    let Some(mut grabber) = Grabber::new(r.w(), r.h()) else {
        let _ = tx.send(Msg::End(0));
        return;
    };
    // Give the editor window a moment to disappear from the screen.
    thread::sleep(Duration::from_millis(200));
    let start = Instant::now();
    let interval = Duration::from_secs_f64(1.0 / fps.max(1) as f64);
    let mut next = start;
    while !sh.stop.load(Relaxed) {
        let t = start.elapsed().as_millis() as u64;
        if tx.send(Msg::Frame(grabber.grab(r.l, r.t, cursor), t)).is_err() {
            return;
        }
        sh.frames.fetch_add(1, Relaxed);
        next += interval;
        let now = Instant::now();
        if next > now {
            thread::sleep(next - now);
        } else {
            next = now;
        }
    }
    let _ = tx.send(Msg::End(start.elapsed().as_millis() as u64));
}

/// The latest frame from the stream, already cropped to the recorded area.
type Latest = Arc<Mutex<Option<Vec<u32>>>>;

/// Receives the screen cast from PipeWire and turns it into frames at `fps`.
fn cast_loop(fd: OwnedFd, node: u32, region: Rect, screen: (i32, i32), fps: u32, sh: Arc<Shared>, tx: mpsc::Sender<Msg>) {
    let latest: Latest = Arc::new(Mutex::new(None));
    let tx_err = tx.clone();
    if let Err(e) = run_stream(fd, node, region, screen, fps, sh, tx, latest) {
        eprintln!("screen cast failed: {e}");
        let _ = tx_err.send(Msg::End(0));
    }
}

#[allow(clippy::too_many_arguments)]
fn run_stream(
    fd: OwnedFd,
    node: u32,
    region: Rect,
    screen: (i32, i32),
    fps: u32,
    sh: Arc<Shared>,
    tx: mpsc::Sender<Msg>,
    latest: Latest,
) -> Result<(), pw::Error> {
    pw::init();
    let mainloop = pw::main_loop::MainLoop::new(None)?;
    let context = pw::context::Context::new(&mainloop)?;
    let core = context.connect_fd(fd, None)?;
    let stream = pw::stream::Stream::new(
        &core,
        "screen-capture",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )?;

    let frame = latest.clone();
    let _listener = stream
        .add_local_listener_with_user_data(spa::param::video::VideoInfoRaw::default())
        .param_changed(|_, info, id, param| {
            if id == spa::param::ParamType::Format.as_raw() {
                if let Some(param) = param {
                    let _ = info.parse(param);
                }
            }
        })
        .process(move |stream, info| {
            let Some(mut buffer) = stream.dequeue_buffer() else { return };
            let datas = buffer.datas_mut();
            let Some(d) = datas.first_mut() else { return };
            let (offset, stride) = (d.chunk().offset() as usize, d.chunk().stride() as usize);
            let size = info.size();
            let rgb = matches!(info.format(), spa::param::video::VideoFormat::RGBx | spa::param::video::VideoFormat::RGBA);
            let Some(bytes) = d.data() else { return };
            if let Some(px) = crop(&bytes[offset.min(bytes.len())..], stride, size.width as i32, size.height as i32, rgb, region, screen) {
                *frame.lock().unwrap() = Some(px);
            }
        })
        .register()?;

    let format = spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(spa::param::format::FormatProperties::MediaType, Id, spa::param::format::MediaType::Video),
        spa::pod::property!(spa::param::format::FormatProperties::MediaSubtype, Id, spa::param::format::MediaSubtype::Raw),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            spa::param::video::VideoFormat::BGRx,
            spa::param::video::VideoFormat::BGRx,
            spa::param::video::VideoFormat::BGRA,
            spa::param::video::VideoFormat::RGBx,
            spa::param::video::VideoFormat::RGBA,
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            spa::utils::Rectangle { width: 1920, height: 1080 },
            spa::utils::Rectangle { width: 1, height: 1 },
            spa::utils::Rectangle { width: 16384, height: 16384 }
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            spa::utils::Fraction { num: 30, denom: 1 },
            spa::utils::Fraction { num: 0, denom: 1 },
            spa::utils::Fraction { num: 240, denom: 1 }
        ),
    );
    let bytes: Vec<u8> = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(format),
    )
    .map_err(|_| pw::Error::CreationFailed)?
    .0
    .into_inner();
    let mut params = [spa::pod::Pod::from_bytes(&bytes).ok_or(pw::Error::CreationFailed)?];
    stream.connect(
        spa::utils::Direction::Input,
        Some(node),
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    )?;

    // The sampler turns the latest frame into GIF frames on a steady clock
    // (the compositor only sends frames when the screen changes) and quits
    // the PipeWire loop when the recording stops.
    let (quit_tx, quit_rx) = pw::channel::channel::<()>();
    let ml = mainloop.clone();
    let _quit = quit_rx.attach(mainloop.loop_(), move |_| ml.quit());
    let (w, h) = (region.w(), region.h());
    let sampler = thread::spawn(move || {
        thread::sleep(Duration::from_millis(200));
        let start = Instant::now();
        let interval = Duration::from_secs_f64(1.0 / fps.max(1) as f64);
        let mut next = start;
        while !sh.stop.load(Relaxed) {
            let px = latest.lock().unwrap().clone();
            if let Some(px) = px.filter(|p| p.len() == w as usize * h as usize) {
                if tx.send(Msg::Frame(px, start.elapsed().as_millis() as u64)).is_err() {
                    break;
                }
                sh.frames.fetch_add(1, Relaxed);
            }
            next += interval;
            let now = Instant::now();
            if next > now {
                thread::sleep(next - now);
            } else {
                next = now;
            }
        }
        let _ = tx.send(Msg::End(start.elapsed().as_millis() as u64));
        let _ = quit_tx.send(());
    });
    mainloop.run();
    let _ = sampler.join();
    Ok(())
}

/// Cuts the recorded area out of a stream frame (scaling if the stream and
/// the screenshot differ in size) as BGRA words.
fn crop(bytes: &[u8], stride: usize, fw: i32, fh: i32, rgb: bool, region: Rect, screen: (i32, i32)) -> Option<Vec<u32>> {
    if fw <= 0 || fh <= 0 || stride < fw as usize * 4 || bytes.len() < stride * (fh as usize - 1) + fw as usize * 4 {
        return None;
    }
    let (sx, sy) = (fw as f32 / screen.0.max(1) as f32, fh as f32 / screen.1.max(1) as f32);
    let (w, h) = (region.w(), region.h());
    let mut out = Vec::with_capacity(w as usize * h as usize);
    for y in 0..h {
        let fy = (((region.t + y) as f32 * sy) as i32).clamp(0, fh - 1) as usize;
        for x in 0..w {
            let fx = (((region.l + x) as f32 * sx) as i32).clamp(0, fw - 1) as usize;
            let i = fy * stride + fx * 4;
            let (a, b, c) = (bytes[i], bytes[i + 1], bytes[i + 2]);
            let (bl, g, r) = if rgb { (c, b, a) } else { (a, b, c) };
            out.push(u32::from_le_bytes([bl, g, r, 255]));
        }
    }
    Some(out)
}
