//! The tray icon (StatusNotifierItem over D-Bus): KDE, and GNOME with the
//! AppIndicator extension (Ubuntu ships it enabled).

use ksni::menu::{CheckmarkItem, StandardItem, SubMenu};
use ksni::{Icon, MenuItem, ToolTip, Tray};
use winit::event_loop::EventLoopProxy;

use super::{Cmd, UserEvent};
use crate::icon;
use crate::settings::FPS_CHOICES;
use crate::util::APP_NAME;

pub struct TrayModel {
    pub proxy: EventLoopProxy<UserEvent>,
    pub recording: bool,
    pub fps: u32,
    pub cursor: bool,
    pub autostart: bool,
}

impl TrayModel {
    fn send(&self, cmd: Cmd) {
        let _ = self.proxy.send_event(UserEvent::Command(cmd));
    }
}

fn pixmap(size: u32, recording: bool) -> Icon {
    // ARGB32 in network byte order.
    let data = icon::render_rgba(size, recording).chunks_exact(4).flat_map(|p| [p[3], p[0], p[1], p[2]]).collect();
    Icon { width: size as i32, height: size as i32, data }
}

impl Tray for TrayModel {
    fn id(&self) -> String {
        "screen-capture".into()
    }

    fn title(&self) -> String {
        APP_NAME.into()
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        [22, 32, 48, 64].iter().map(|&s| pixmap(s, self.recording)).collect()
    }

    fn tool_tip(&self) -> ToolTip {
        let description = if self.recording {
            "Recording a GIF: press PrtSc to stop"
        } else {
            "PrtSc: capture an area · Shift+PrtSc: record a GIF"
        };
        ToolTip { title: APP_NAME.into(), description: description.into(), ..Default::default() }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.send(if self.recording { Cmd::Stop } else { Cmd::Capture });
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let item = |label: &str, cmd: Cmd| -> MenuItem<Self> {
            StandardItem { label: label.into(), activate: Box::new(move |t: &mut Self| t.send(cmd)), ..Default::default() }
                .into()
        };
        let check = |label: &str, checked: bool, cmd: Cmd| -> MenuItem<Self> {
            CheckmarkItem {
                label: label.into(),
                checked,
                activate: Box::new(move |t: &mut Self| t.send(cmd)),
                ..Default::default()
            }
            .into()
        };
        let fps = FPS_CHOICES.iter().map(|&f| check(&format!("{f} fps"), f == self.fps, Cmd::SetFps(f))).collect();
        vec![
            item("Capture area (PrtSc)", Cmd::Capture),
            if self.recording {
                item("Stop GIF recording (PrtSc)", Cmd::Stop)
            } else {
                item("Record GIF (Shift+PrtSc)", Cmd::Gif)
            },
            MenuItem::Separator,
            SubMenu { label: "GIF frame rate".into(), submenu: fps, ..Default::default() }.into(),
            check("Show mouse cursor in GIFs", self.cursor, Cmd::ToggleCursor),
            item("Open GIF folder", Cmd::OpenFolder),
            MenuItem::Separator,
            check("Start at login", self.autostart, Cmd::ToggleAutostart),
            MenuItem::Separator,
            item("Exit", Cmd::Exit),
        ]
    }
}
