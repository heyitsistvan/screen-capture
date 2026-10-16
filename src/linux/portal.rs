//! Wayland desktop portals (xdg-desktop-portal): screenshots, the global
//! PrtSc shortcut and screen casting for GIF recording. They're async, so they
//! run on a small tokio runtime and report back through the event loop.

use std::os::fd::OwnedFd;
use std::sync::OnceLock;

use ashpd::desktop::global_shortcuts::{GlobalShortcuts, NewShortcut};
use ashpd::desktop::screencast::{CursorMode, Screencast, SourceType};
use ashpd::desktop::screenshot::Screenshot;
use ashpd::desktop::PersistMode;
use futures_util::StreamExt;
use tokio::runtime::Runtime;
use tokio::sync::oneshot;

pub fn runtime() -> &'static Runtime {
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("could not start the async runtime")
    })
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// A screenshot of the whole desktop as opaque BGRA `(w, h, pixels)`.
pub async fn screenshot() -> Result<(i32, i32, Vec<u8>), String> {
    let shot = Screenshot::request().interactive(false).modal(false).send().await.map_err(err)?.response().map_err(err)?;
    let path = shot.uri().to_file_path().map_err(|_| "the screenshot wasn't saved to a file".to_string())?;
    let data = std::fs::read(&path).map_err(err)?;
    // The portal saved it for us alone; don't leave copies around.
    let _ = std::fs::remove_file(&path);
    decode_png(&data)
}

fn decode_png(data: &[u8]) -> Result<(i32, i32, Vec<u8>), String> {
    let mut dec = png::Decoder::new(data);
    dec.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = dec.read_info().map_err(err)?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).map_err(err)?;
    let channels = match info.color_type {
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Indexed => return Err("unexpected palette PNG".into()),
    };
    let px = buf[..info.buffer_size()]
        .chunks_exact(channels)
        .flat_map(|p| if channels < 3 { [p[0], p[0], p[0], 255] } else { [p[2], p[1], p[0], 255] })
        .collect();
    Ok((info.width as i32, info.height as i32, px))
}

/// Binds PrtSc and Shift+PrtSc through the GlobalShortcuts portal and calls
/// `on_press(gif, activation_token)` for each press. Runs as long as the
/// shortcuts stay bound.
pub async fn global_shortcuts(on_press: impl Fn(bool, Option<String>) + Send + 'static) -> Result<(), String> {
    let portal = GlobalShortcuts::new().await.map_err(err)?;
    let session = portal.create_session().await.map_err(err)?;
    let shortcuts = [
        NewShortcut::new("capture", "Capture an area").preferred_trigger("Print"),
        NewShortcut::new("gif", "Record an area as a GIF").preferred_trigger("<Shift>Print"),
    ];
    portal.bind_shortcuts(&session, &shortcuts, None).await.map_err(err)?.response().map_err(err)?;
    let mut presses = portal.receive_activated().await.map_err(err)?;
    while let Some(press) = presses.next().await {
        // Lets the compositor give our window focus (xdg-activation).
        let token = press.options().get("activation_token").and_then(|v| v.downcast_ref::<String>().ok());
        on_press(press.shortcut_id() == "gif", token);
    }
    drop(session);
    Ok(())
}

/// A running screen cast: PipeWire stream to read frames from.
pub struct Cast {
    pub fd: OwnedFd,
    pub node: u32,
    /// Lets the portal skip asking which screen next time.
    pub restore_token: Option<String>,
    /// Ends the portal session when sent (or dropped).
    pub stop: oneshot::Sender<()>,
}

/// Starts casting one monitor (the portal may ask which one).
pub async fn start_cast(restore_token: Option<String>, cursor: bool) -> Result<Cast, String> {
    let portal = Screencast::new().await.map_err(err)?;
    let session = portal.create_session().await.map_err(err)?;
    let modes = portal.available_cursor_modes().await.unwrap_or_default();
    let cursor_mode = if cursor && modes.contains(CursorMode::Embedded) { CursorMode::Embedded } else { CursorMode::Hidden };
    portal
        .select_sources(
            &session,
            cursor_mode,
            SourceType::Monitor.into(),
            false,
            restore_token.as_deref(),
            PersistMode::ExplicitlyRevoked,
        )
        .await
        .map_err(err)?
        .response()
        .map_err(err)?;
    let streams = portal.start(&session, None).await.map_err(err)?.response().map_err(err)?;
    let stream = streams.streams().first().ok_or("no screen was chosen")?;
    let node = stream.pipe_wire_node_id();
    let restore_token = streams.restore_token().map(str::to_string);
    let fd = portal.open_pipe_wire_remote(&session).await.map_err(err)?;
    let (stop, stopped) = oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = stopped.await;
        let _ = session.close().await;
    });
    Ok(Cast { fd, node, restore_token, stop })
}
