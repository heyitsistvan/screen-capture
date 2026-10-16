//! A GIF recording session: the capture thread, the encoder thread, the red
//! frame around the recorded area and the floating control bar.

use std::ffi::c_void;
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, TRUE, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateRoundRectRgn, CreateSolidBrush, EndPaint, InvalidateRect, ScreenToClient, SetWindowRgn,
    HDC, PAINTSTRUCT, SRCCOPY,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::WM_MOUSELEAVE;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetWindowLongPtrW, LoadCursorW, PostMessageW, RegisterClassExW,
    SetLayeredWindowAttributes, SetTimer, SetWindowDisplayAffinity, SetWindowLongPtrW, ShowWindow, CREATESTRUCTW,
    GWLP_USERDATA, HTCAPTION, HTCLIENT, IDC_ARROW, LWA_ALPHA, MA_NOACTIVATE, SW_SHOWNOACTIVATE,
    WDA_EXCLUDEFROMCAPTURE, WM_ERASEBKGND, WM_LBUTTONUP, WM_MOUSEACTIVATE, WM_MOUSEMOVE, WM_NCCREATE,
    WM_NCDESTROY, WM_NCHITTEST, WM_PAINT, WM_TIMER, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

use crate::annotate::{self, Rgba, Shape};
use crate::capture::{Dib, Grabber};
use crate::gif_enc::{self, GifInfo, Msg};
use crate::text::{self, Fonts};
use crate::ui::{Canvas, Icon};
use crate::util::{self, wide, Rect};
use crate::{WM_APP_CANCEL_GIF, WM_APP_GIF_DONE, WM_APP_STOP_GIF};

const BORDER_CLASS: &str = "ScreenCaptureRecBorder";
const BAR_CLASS: &str = "ScreenCaptureRecBar";
const RED: Rgba = Rgba(229, 57, 53, 255);

/// What the overlay hands over when the user starts a recording.
pub struct GifRequest {
    /// Area to record, in screen coordinates.
    pub region: Rect,
    /// Annotations to burn into every frame, relative to the region.
    pub shapes: Vec<Shape>,
}

/// Posted to the main window (boxed, in LPARAM) when encoding ends.
pub struct GifOutcome {
    pub id: u64,
    pub result: Result<GifInfo, String>,
}

struct Shared {
    stop: AtomicBool,
    cancel: AtomicBool,
    frames: AtomicUsize,
    processed: AtomicUsize,
    start: Instant,
    elapsed_ms: AtomicU64,
}

pub struct Recording {
    pub id: u64,
    shared: Arc<Shared>,
    borders: Vec<HWND>,
    bar: HWND,
}

pub fn register_classes(hinst: HINSTANCE) {
    unsafe {
        let cls = wide(BORDER_CLASS);
        let mut wc: WNDCLASSEXW = std::mem::zeroed();
        wc.cbSize = size_of::<WNDCLASSEXW>() as u32;
        wc.lpfnWndProc = Some(DefWindowProcW);
        wc.hInstance = hinst;
        wc.hbrBackground = CreateSolidBrush(0x0035_39E5); // COLORREF is 0x00BBGGRR
        wc.lpszClassName = cls.as_ptr();
        RegisterClassExW(&wc);

        let cls = wide(BAR_CLASS);
        let mut wc: WNDCLASSEXW = std::mem::zeroed();
        wc.cbSize = size_of::<WNDCLASSEXW>() as u32;
        wc.lpfnWndProc = Some(bar_proc);
        wc.hInstance = hinst;
        wc.hCursor = LoadCursorW(null_mut(), IDC_ARROW);
        wc.lpszClassName = cls.as_ptr();
        RegisterClassExW(&wc);
    }
}

impl Recording {
    pub fn start(
        main: HWND,
        id: u64,
        req: GifRequest,
        fps: u32,
        cursor: bool,
        fonts: Arc<Fonts>,
        out: PathBuf,
    ) -> Recording {
        let r = req.region;
        let (cx, cy) = r.center();
        let scale = util::dpi_scale_at(cx, cy);
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            cancel: AtomicBool::new(false),
            frames: AtomicUsize::new(0),
            processed: AtomicUsize::new(0),
            start: Instant::now(),
            elapsed_ms: AtomicU64::new(0),
        });
        let th = ((2.0 * scale).round() as i32).max(2);
        let borders = unsafe { create_borders(r, th) };
        let bar = unsafe { create_bar(main, r, th, scale, shared.clone(), fonts.clone()) };

        let (tx, rx) = mpsc::channel();
        let sh = shared.clone();
        thread::spawn(move || capture_loop(r, fps, cursor, &sh, tx));

        let sh = shared.clone();
        let main_addr = main as usize;
        thread::spawn(move || {
            let (w, h) = (r.w(), r.h());
            let shapes = req.shapes;
            let overlay = |px: &mut [u32]| {
                if !shapes.is_empty() {
                    annotate::render(bytemuck::cast_slice_mut(px), w, h, &shapes, 0.0, 0.0, &fonts);
                }
            };
            let result = gif_enc::encode(rx, w, h, overlay, &out, &sh.cancel, &sh.processed);
            let outcome = Box::into_raw(Box::new(GifOutcome { id, result }));
            unsafe {
                if PostMessageW(main_addr as HWND, WM_APP_GIF_DONE, 0, outcome as LPARAM) == 0 {
                    drop(Box::from_raw(outcome));
                }
            }
        });

        Recording { id, shared, borders, bar }
    }

    pub fn is_capturing(&self) -> bool {
        !self.shared.stop.load(Relaxed)
    }

    /// Stops capturing; the frames already captured keep encoding.
    pub fn stop(&mut self) {
        if self.shared.stop.swap(true, Relaxed) {
            return;
        }
        self.shared.elapsed_ms.store(self.shared.start.elapsed().as_millis() as u64, Relaxed);
        self.destroy_borders();
        unsafe { InvalidateRect(self.bar, null(), 0) };
    }

    /// Stops and discards the recording.
    pub fn cancel(&mut self) {
        self.shared.cancel.store(true, Relaxed);
        self.shared.stop.store(true, Relaxed);
    }

    fn destroy_borders(&mut self) {
        for h in self.borders.drain(..) {
            unsafe { DestroyWindow(h) };
        }
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        self.destroy_borders();
        if !self.bar.is_null() {
            unsafe { DestroyWindow(self.bar) };
            self.bar = null_mut();
        }
    }
}

fn capture_loop(r: Rect, fps: u32, cursor: bool, sh: &Shared, tx: mpsc::Sender<Msg>) {
    let Some(mut grabber) = Grabber::new(r.w(), r.h()) else {
        let _ = tx.send(Msg::End(0));
        return;
    };
    // Give the overlay a moment to disappear from the screen.
    thread::sleep(Duration::from_millis(150));
    let start = Instant::now();
    let interval = Duration::from_secs_f64(1.0 / fps.max(1) as f64);
    let mut next = start;
    while !sh.stop.load(Relaxed) {
        let t = start.elapsed().as_millis() as u64;
        let px = grabber.grab(r.l, r.t, cursor);
        if tx.send(Msg::Frame(px, t)).is_err() {
            return; // encoder gave up
        }
        sh.frames.fetch_add(1, Relaxed);
        next += interval;
        let now = Instant::now();
        if next > now {
            thread::sleep(next - now);
        } else {
            next = now; // fell behind; don't try to catch up with a burst
        }
    }
    let _ = tx.send(Msg::End(start.elapsed().as_millis() as u64));
}

/// Four thin click-through windows just outside the recorded area.
unsafe fn create_borders(r: Rect, th: i32) -> Vec<HWND> {
    let hinst = GetModuleHandleW(null());
    let cls = wide(BORDER_CLASS);
    [
        Rect::new(r.l - th, r.t - th, r.r + th, r.t),
        Rect::new(r.l - th, r.b, r.r + th, r.b + th),
        Rect::new(r.l - th, r.t, r.l, r.b),
        Rect::new(r.r, r.t, r.r + th, r.b),
    ]
    .iter()
    .filter_map(|b| {
        let h = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED | WS_EX_TRANSPARENT,
            cls.as_ptr(),
            null(),
            WS_POPUP,
            b.l,
            b.t,
            b.w(),
            b.h(),
            null_mut(),
            null_mut(),
            hinst,
            null(),
        );
        if h.is_null() {
            return None;
        }
        SetLayeredWindowAttributes(h, 0, 235, LWA_ALPHA);
        SetWindowDisplayAffinity(h, WDA_EXCLUDEFROMCAPTURE);
        ShowWindow(h, SW_SHOWNOACTIVATE);
        Some(h)
    })
    .collect()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BarBtn {
    Stop,
    Cancel,
}

struct Bar {
    main: HWND,
    shared: Arc<Shared>,
    fonts: Arc<Fonts>,
    scale: f32,
    w: i32,
    h: i32,
    dib: Option<Dib>,
    hover: Option<BarBtn>,
    tracking: bool,
}

unsafe fn create_bar(main: HWND, r: Rect, th: i32, scale: f32, shared: Arc<Shared>, fonts: Arc<Fonts>) -> HWND {
    let px = |v: f32| (v * scale).round() as i32;
    let (bw, bh) = (px(260.0), px(40.0));
    let (cx, cy) = r.center();
    let mon = util::monitor_rect_at(cx, cy);
    let gap = th + px(6.0);
    let mut y = r.b + gap;
    if y + bh > mon.b {
        y = r.t - gap - bh;
        if y < mon.t {
            // No room outside; sit inside (the bar is excluded from capture).
            y = r.b - bh - px(8.0);
        }
    }
    let x = (r.r - bw).min(mon.r - bw).max(mon.l);

    let bar = Box::into_raw(Box::new(Bar {
        main,
        shared,
        fonts,
        scale,
        w: bw,
        h: bh,
        dib: None,
        hover: None,
        tracking: false,
    }));
    let cls = wide(BAR_CLASS);
    let title = wide("Recording GIF");
    let h = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
        cls.as_ptr(),
        title.as_ptr(),
        WS_POPUP,
        x,
        y,
        bw,
        bh,
        null_mut(),
        null_mut(),
        GetModuleHandleW(null()),
        bar as *const c_void,
    );
    if h.is_null() {
        return null_mut();
    }
    SetWindowDisplayAffinity(h, WDA_EXCLUDEFROMCAPTURE);
    SetWindowRgn(h, CreateRoundRectRgn(0, 0, bw + 1, bh + 1, px(14.0), px(14.0)), TRUE);
    SetTimer(h, 1, 250, None);
    ShowWindow(h, SW_SHOWNOACTIVATE);
    h
}

impl Bar {
    fn px(&self, v: f32) -> i32 {
        (v * self.scale).round() as i32
    }

    fn encoding(&self) -> bool {
        self.shared.stop.load(Relaxed)
    }

    fn buttons(&self) -> Vec<(BarBtn, Rect)> {
        let b = self.px(28.0);
        let pad = (self.h - b) / 2;
        let cancel = Rect::xywh(self.w - pad - b, pad, b, b);
        if self.encoding() {
            vec![(BarBtn::Cancel, cancel)]
        } else {
            let sw = self.px(72.0);
            vec![(BarBtn::Stop, Rect::xywh(cancel.l - self.px(6.0) - sw, pad, sw, b)), (BarBtn::Cancel, cancel)]
        }
    }

    fn hit(&self, x: i32, y: i32) -> Option<BarBtn> {
        self.buttons().into_iter().find(|(_, r)| r.contains(x, y)).map(|(b, _)| b)
    }

    fn paint(&mut self, hdc: HDC) {
        if self.dib.is_none() {
            self.dib = Dib::new(self.w, self.h);
        }
        let buttons = self.buttons();
        let encoding = self.encoding();
        let (w, h, s) = (self.w, self.h, self.scale);
        let fonts = self.fonts.clone();
        let hover = self.hover;
        let frames = self.shared.frames.load(Relaxed);
        let processed = self.shared.processed.load(Relaxed);
        let ms = self.shared.start.elapsed().as_millis() as u64;
        let Some(dib) = self.dib.as_mut() else { return };
        let dc = dib.dc;
        let mut cv = Canvas::new(dib.bytes_mut(), w, h);

        cv.fill_px(Rect::new(0, 0, w, h), Rgba(28, 29, 32, 255));
        let fpx = 13.0 * s;
        let (_, lh) = text::line_metrics(&fonts.ui, fpx);
        let cy = h as f32 / 2.0;
        let ty = cy - lh / 2.0;
        let mut x = 14.0 * s;
        if !encoding {
            let on = (ms / 500) % 2 == 0;
            cv.fill_circle(x + 5.0 * s, cy, 5.0 * s, if on { RED } else { RED.alpha(70) });
            x += 18.0 * s;
            let secs = ms / 1000;
            let label = format!("REC {:02}:{:02}", secs / 60, secs % 60);
            cv.text(x, ty, &label, fpx, Rgba(255, 255, 255, 255), &fonts.bold);
            x += text::line_width(&fonts.bold, &label, fpx) + 10.0 * s;
            let small = 12.0 * s;
            let (_, slh) = text::line_metrics(&fonts.ui, small);
            cv.text(x, cy - slh / 2.0, &format!("{frames} frames"), small, Rgba(150, 152, 158, 255), &fonts.ui);
        } else {
            let total = frames.max(1);
            let pct = processed.min(total) * 100 / total;
            cv.text(x, ty, &format!("Encoding GIF…  {pct}%"), fpx, Rgba(255, 255, 255, 255), &fonts.ui);
            let px = |v: f32| (v * s).round() as i32;
            let track = Rect::new(px(14.0), h - px(6.0), w - px(54.0), h - px(3.0));
            cv.fill_rrect(track, 1.5 * s, Rgba(255, 255, 255, 35));
            let fill = Rect::new(track.l, track.t, track.l + (track.w() as usize * pct / 100) as i32, track.b);
            cv.fill_rrect(fill, 1.5 * s, Rgba(10, 132, 255, 255));
        }

        for (b, r) in buttons {
            let hovered = hover == Some(b);
            match b {
                BarBtn::Stop => {
                    cv.fill_rrect(r, 6.0 * s, if hovered { Rgba(244, 81, 72, 255) } else { RED });
                    let ic = r.l as f32 + 16.0 * s;
                    let mid = (r.t + r.b) as f32 / 2.0;
                    cv.icon(Icon::Stop, ic, mid, 16.0 * s, Rgba(255, 255, 255, 255), &fonts);
                    cv.text(ic + 11.0 * s, mid - lh / 2.0, "Stop", fpx, Rgba(255, 255, 255, 255), &fonts.bold);
                }
                BarBtn::Cancel => {
                    if hovered {
                        cv.fill_rrect(r, 6.0 * s, Rgba(255, 255, 255, 30));
                    }
                    let (cx, cy) = ((r.l + r.r) as f32 / 2.0, (r.t + r.b) as f32 / 2.0);
                    cv.icon(Icon::Close, cx, cy, 16.0 * s, Rgba(200, 202, 208, 255), &fonts);
                }
            }
        }
        unsafe { BitBlt(hdc, 0, 0, w, h, dc, 0, 0, SRCCOPY) };
    }
}

unsafe extern "system" fn bar_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_NCCREATE {
        let cs = &*(lp as *const CREATESTRUCTW);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
        return DefWindowProcW(hwnd, msg, wp, lp);
    }
    let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Bar;
    if p.is_null() {
        return DefWindowProcW(hwnd, msg, wp, lp);
    }
    if msg == WM_NCDESTROY {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        drop(Box::from_raw(p));
        return DefWindowProcW(hwnd, msg, wp, lp);
    }
    let bar = &mut *p;
    let (x, y) = ((lp & 0xFFFF) as i16 as i32, ((lp >> 16) & 0xFFFF) as i16 as i32);
    match msg {
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let hdc = BeginPaint(hwnd, &mut ps);
            bar.paint(hdc);
            EndPaint(hwnd, &ps);
            0
        }
        WM_ERASEBKGND => 1,
        WM_TIMER => {
            InvalidateRect(hwnd, null(), 0);
            0
        }
        WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
        WM_NCHITTEST => {
            let mut pt = POINT { x, y };
            ScreenToClient(hwnd, &mut pt);
            // Anything that isn't a button drags the bar around.
            if bar.hit(pt.x, pt.y).is_some() {
                HTCLIENT as LRESULT
            } else {
                HTCAPTION as LRESULT
            }
        }
        WM_MOUSEMOVE => {
            if !bar.tracking {
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                TrackMouseEvent(&mut tme);
                bar.tracking = true;
            }
            let hv = bar.hit(x, y);
            if hv != bar.hover {
                bar.hover = hv;
                InvalidateRect(hwnd, null(), 0);
            }
            0
        }
        WM_MOUSELEAVE => {
            bar.tracking = false;
            bar.hover = None;
            InvalidateRect(hwnd, null(), 0);
            0
        }
        WM_LBUTTONUP => {
            if let Some(b) = bar.hit(x, y) {
                let m = match b {
                    BarBtn::Stop => WM_APP_STOP_GIF,
                    BarBtn::Cancel => WM_APP_CANCEL_GIF,
                };
                PostMessageW(bar.main, m, 0, 0);
            }
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}
