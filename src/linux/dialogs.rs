//! Save and color dialogs through zenity (GNOME and most desktops) or
//! kdialog (KDE). They block, so callers run them on a background thread.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::annotate::Rgba;

fn has(cmd: &str) -> bool {
    Command::new("sh").args(["-c", &format!("command -v {cmd}")]).output().is_ok_and(|o| o.status.success())
}

/// Asks where to save a PNG. `None`: no dialog program is installed;
/// `Some(None)`: cancelled. On X11, `parent` keeps the dialog above our
/// full-screen window.
pub fn save_png(default: &Path, parent: Option<u32>) -> Option<Option<PathBuf>> {
    let attach = parent.map(|id| id.to_string());
    let out = if has("zenity") {
        let mut c = Command::new("zenity");
        c.args(["--file-selection", "--save", "--confirm-overwrite", "--title=Save screenshot"]);
        c.arg(format!("--filename={}", default.display())).arg("--file-filter=PNG image | *.png");
        if let Some(id) = &attach {
            c.arg(format!("--attach={id}"));
        }
        c.output().ok()?
    } else if has("kdialog") {
        let mut c = Command::new("kdialog");
        if let Some(id) = &attach {
            c.args(["--attach", id]);
        }
        c.args(["--title", "Save screenshot", "--getsavefilename"]).arg(default).arg("*.png|PNG image");
        c.output().ok()?
    } else {
        return None;
    };
    if !out.status.success() {
        return Some(None);
    }
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if path.is_empty() {
        return Some(None);
    }
    let mut path = PathBuf::from(path);
    if path.extension().is_none() {
        path.set_extension("png");
    }
    Some(Some(path))
}

/// Asks for a color. `None`: no dialog program; `Some(None)`: cancelled.
pub fn color(current: Rgba, parent: Option<u32>) -> Option<Option<Rgba>> {
    let hex = format!("#{:02x}{:02x}{:02x}", current.0, current.1, current.2);
    let attach = parent.map(|id| id.to_string());
    let out = if has("zenity") {
        let mut c = Command::new("zenity");
        c.args(["--color-selection", "--show-palette", "--title=Choose a color"]).arg(format!("--color={hex}"));
        if let Some(id) = &attach {
            c.arg(format!("--attach={id}"));
        }
        c.output().ok()?
    } else if has("kdialog") {
        let mut c = Command::new("kdialog");
        if let Some(id) = &attach {
            c.args(["--attach", id]);
        }
        c.args(["--getcolor", "--default", &hex]);
        c.output().ok()?
    } else {
        return None;
    };
    if !out.status.success() {
        return Some(None);
    }
    Some(parse_color(String::from_utf8_lossy(&out.stdout).trim()))
}

/// Parses `#rrggbb`, `rgb(r,g,b)` or `rgba(r,g,b,a)`.
fn parse_color(s: &str) -> Option<Rgba> {
    if let Some(hex) = s.strip_prefix('#') {
        let v = u32::from_str_radix(hex.get(..6)?, 16).ok()?;
        return Some(Rgba((v >> 16) as u8, (v >> 8) as u8, v as u8, 255));
    }
    let inner = s.trim_start_matches("rgba").trim_start_matches("rgb").trim_matches(|c| c == '(' || c == ')');
    let mut parts = inner.split(',').map(|p| p.trim().parse::<f32>().ok());
    let (r, g, b) = (parts.next()??, parts.next()??, parts.next()??);
    Some(Rgba(r as u8, g as u8, b as u8, 255))
}
