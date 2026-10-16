//! Screen Capture — a Lightshot-style screenshot tool for Windows and Linux.
//!
//! PrtSc opens an overlay to select an area, annotate it and copy/save it.
//! Shift+PrtSc (or the overlay's GIF button) records the area as an animated
//! GIF; PrtSc again stops. The app lives in the notification area / tray.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod annotate;
mod editor;
mod gif_enc;
mod icon;
mod ocr;
mod recbar;
#[cfg(debug_assertions)]
mod selftest;
mod settings;
mod text;
mod ui;
mod util;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod win;

fn main() {
    #[cfg(windows)]
    win::run();
    #[cfg(target_os = "linux")]
    linux::run();
}
