//! Persistent settings (a tiny ini file) and the "Start with Windows" switch.

use std::fs;
use std::path::PathBuf;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ,
    KEY_WRITE, REG_SZ,
};

use crate::util::{self, wide};

pub const FPS_CHOICES: [u32; 5] = [10, 15, 20, 25, 30];

pub struct Settings {
    pub fps: u32,
    pub cursor: bool,
    /// Outline around drawings: "auto", "off", "white", "black" or "#rrggbb".
    pub outline: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { fps: 15, cursor: true, outline: "auto".into() }
    }
}

impl Settings {
    fn path() -> PathBuf {
        util::app_data_dir().join("settings.ini")
    }

    pub fn exists() -> bool {
        Self::path().exists()
    }

    pub fn load() -> Settings {
        let mut s = Settings::default();
        let Ok(text) = fs::read_to_string(Self::path()) else {
            return s;
        };
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            let v = v.trim();
            match k.trim() {
                "gif_fps" => {
                    if let Ok(f) = v.parse::<u32>() {
                        if (5..=50).contains(&f) {
                            s.fps = f;
                        }
                    }
                }
                "gif_cursor" => s.cursor = matches!(v, "1" | "true" | "yes"),
                "outline_color" => s.outline = v.to_string(),
                _ => {}
            }
        }
        s
    }

    pub fn save(&self) {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let _ = fs::write(
            path,
            format!("gif_fps={}\ngif_cursor={}\noutline_color={}\n", self.fps, self.cursor as u8, self.outline),
        );
    }
}

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const RUN_VALUE: &str = "ScreenCapture";

fn open_run_key(access: u32) -> Option<HKEY> {
    let key = wide(RUN_KEY);
    let mut h: HKEY = null_mut();
    let rc = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, key.as_ptr(), 0, access, &mut h) };
    (rc == ERROR_SUCCESS).then_some(h)
}

pub fn autostart_enabled() -> bool {
    let Some(h) = open_run_key(KEY_READ) else { return false };
    let name = wide(RUN_VALUE);
    let mut size = 0u32;
    let rc = unsafe { RegQueryValueExW(h, name.as_ptr(), null(), null_mut(), null_mut(), &mut size) };
    unsafe { RegCloseKey(h) };
    rc == ERROR_SUCCESS
}

pub fn set_autostart(on: bool) -> bool {
    let Some(h) = open_run_key(KEY_WRITE) else { return false };
    let name = wide(RUN_VALUE);
    let rc = unsafe {
        if on {
            let exe = std::env::current_exe().unwrap_or_default();
            let cmd = wide(&format!("\"{}\"", exe.display()));
            RegSetValueExW(h, name.as_ptr(), 0, REG_SZ, cmd.as_ptr() as *const u8, (cmd.len() * 2) as u32)
        } else {
            RegDeleteValueW(h, name.as_ptr())
        }
    };
    unsafe { RegCloseKey(h) };
    rc == ERROR_SUCCESS
}
