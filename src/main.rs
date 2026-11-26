//! Screen Capture — a Lightshot-style screenshot tool for Windows and Linux.
//!
//! PrtSc opens an overlay to select an area, annotate it and copy/save it.
//! Ctrl+PrtSc (or the overlay's Pin button) keeps the result on screen in a
//! floating window, to compare captures. Shift+PrtSc (or the record button)
//! records the area as an animated GIF or an MP4 video; PrtSc again stops.
//! The app lives in the notification area / tray, with recent captures in its
//! menu.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod annotate;
mod editor;
mod gif_enc;
mod history;
mod icon;
mod mp4_enc;
mod ocr;
mod pin;
mod recbar;
mod recording;
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
