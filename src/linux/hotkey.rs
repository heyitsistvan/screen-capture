//! PrtSc as a global hotkey on X11: grabs the key on the root window. (On
//! Wayland the GlobalShortcuts portal does this; see `portal.rs`.)

use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt as _, GrabMode, ModMask};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

const XK_PRINT: u32 = 0xff61;

/// Grabs PrtSc and Shift+PrtSc and calls `on_press(shift)` from a background
/// thread for each press.
pub fn grab_print(on_press: impl Fn(bool) + Send + 'static) -> Result<(), String> {
    let (conn, screen) = x11rb::connect(None).map_err(|e| e.to_string())?;
    let root = conn.setup().roots[screen].root;
    let keycode = print_keycode(&conn).ok_or("there's no PrtSc key in the keyboard layout")?;
    // Grab with every combination of the lock modifiers, or Caps/Num Lock
    // being on would let the key through.
    let locks = [ModMask::from(0u16), ModMask::LOCK, ModMask::M2, ModMask::LOCK | ModMask::M2];
    for base in [ModMask::from(0u16), ModMask::SHIFT] {
        for lock in locks {
            conn.grab_key(true, root, base | lock, keycode, GrabMode::ASYNC, GrabMode::ASYNC)
                .map_err(|e| e.to_string())?
                .check()
                .map_err(|_| "another program already uses PrtSc".to_string())?;
        }
    }
    conn.flush().map_err(|e| e.to_string())?;
    std::thread::spawn(move || {
        let mut last = Instant::now() - Duration::from_secs(1);
        while let Ok(ev) = conn.wait_for_event() {
            if let Event::KeyPress(k) = ev {
                // Holding the key auto-repeats; one press is one capture.
                if k.detail == keycode && last.elapsed() > Duration::from_millis(400) {
                    last = Instant::now();
                    on_press(u16::from(k.state) & u16::from(ModMask::SHIFT) != 0);
                }
            }
        }
    });
    Ok(())
}

fn print_keycode(conn: &RustConnection) -> Option<u8> {
    let setup = conn.setup();
    let (min, max) = (setup.min_keycode, setup.max_keycode);
    let map = conn.get_keyboard_mapping(min, max - min + 1).ok()?.reply().ok()?;
    let per = map.keysyms_per_keycode as usize;
    map.keysyms.chunks(per.max(1)).position(|syms| syms.contains(&XK_PRINT)).map(|i| min + i as u8)
}
