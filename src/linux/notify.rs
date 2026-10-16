//! Desktop notifications (org.freedesktop.Notifications).

use std::path::{Path, PathBuf};
use std::process::Command;

use notify_rust::Notification;

use crate::util::APP_NAME;

pub fn show(title: &str, body: &str) {
    let (title, body) = (title.to_string(), body.to_string());
    std::thread::spawn(move || {
        let shown = Notification::new().appname(APP_NAME).summary(&title).body(&body).icon("camera-photo").show();
        if shown.is_err() {
            // No notification service: at least a terminal will see it.
            eprintln!("{title}: {body}");
        }
    });
}

/// A notification that shows `file` in its folder when clicked.
pub fn show_file(title: &str, body: &str, file: &Path) {
    let (title, body, file) = (title.to_string(), body.to_string(), file.to_path_buf());
    std::thread::spawn(move || {
        let shown = Notification::new()
            .appname(APP_NAME)
            .summary(&title)
            .body(&body)
            .icon("camera-photo")
            .action("default", "Show in folder")
            .show();
        if let Ok(handle) = shown {
            handle.wait_for_action(|action| {
                if action == "default" {
                    show_in_folder(&file);
                }
            });
        }
    });
}

/// Opens the file manager with `file` selected (falls back to the folder).
pub fn show_in_folder(file: &Path) {
    let uri = format!("file://{}", file.display());
    let selected = Command::new("dbus-send")
        .args([
            "--session",
            "--dest=org.freedesktop.FileManager1",
            "--type=method_call",
            "/org/freedesktop/FileManager1",
            "org.freedesktop.FileManager1.ShowItems",
        ])
        .arg(format!("array:string:{uri}"))
        .arg("string:")
        .status()
        .is_ok_and(|s| s.success());
    if !selected {
        let dir: PathBuf = file.parent().map(Path::to_path_buf).unwrap_or_else(|| file.to_path_buf());
        open(&dir);
    }
}

pub fn open(path: &Path) {
    let _ = Command::new("xdg-open").arg(path).spawn();
}
