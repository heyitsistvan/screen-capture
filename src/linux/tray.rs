//! The tray icon (StatusNotifierItem over D-Bus): KDE, and GNOME with the
//! AppIndicator extension (Ubuntu ships it enabled).

use ksni::menu::{CheckmarkItem, StandardItem, SubMenu};
use ksni::{Icon, MenuItem, ToolTip, Tray};
use winit::event_loop::EventLoopProxy;

use super::{Cmd, UserEvent};
use crate::icon;
use crate::recording::Format;
use crate::settings::FPS_CHOICES;
use crate::util::{APP_NAME, HOMEPAGE, MAKER, REPOSITORY, VERSION};

pub struct TrayModel {
    pub proxy: EventLoopProxy<UserEvent>,
    pub recording: bool,
    pub fps: u32,
    pub cursor: bool,
    pub format: Format,
    pub autostart: bool,
    /// Recent captures, newest first: (history id, label, thumbnail PNG).
    pub history: Vec<(u64, String, Vec<u8>)>,
    pub pins: usize,
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
            "Recording: press PrtSc to stop".to_string()
        } else {
            format!("PrtSc: capture an area · Shift+PrtSc: record {}", self.format.label())
        };
        ToolTip { title: APP_NAME.into(), description, ..Default::default() }
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
        let formats = Format::ALL.iter().map(|&f| check(f.label(), f == self.format, Cmd::SetFormat(f))).collect();
        let recent: MenuItem<Self> = if self.history.is_empty() {
            StandardItem { label: "Recent captures".into(), enabled: false, ..Default::default() }.into()
        } else {
            let mut items: Vec<MenuItem<Self>> = self
                .history
                .iter()
                .map(|(id, label, thumb)| {
                    let id = *id;
                    StandardItem {
                        label: label.clone(),
                        icon_data: thumb.clone(),
                        activate: Box::new(move |t: &mut Self| t.send(Cmd::PinHistory(id))),
                        ..Default::default()
                    }
                    .into()
                })
                .collect();
            items.extend([MenuItem::Separator, item("Clear history", Cmd::ClearHistory)]);
            SubMenu { label: "Recent captures".into(), submenu: items, ..Default::default() }.into()
        };
        let about = vec![
            StandardItem { label: format!("{APP_NAME} {VERSION}"), enabled: false, ..Default::default() }.into(),
            item(&format!("Made by {MAKER} (pixobit.eu)"), Cmd::OpenUrl(HOMEPAGE)),
            item("Source code on GitHub", Cmd::OpenUrl(REPOSITORY)),
        ];
        vec![
            item("Capture area (PrtSc)", Cmd::Capture),
            item("Capture and pin (Ctrl+PrtSc)", Cmd::Pin),
            if self.recording {
                item("Stop recording (PrtSc)", Cmd::Stop)
            } else {
                item(&format!("Record {} (Shift+PrtSc)", self.format.label()), Cmd::Record)
            },
            MenuItem::Separator,
            recent,
            StandardItem {
                label: if self.pins == 1 { "Close the pin".into() } else { format!("Close all {} pins", self.pins) },
                visible: self.pins > 0,
                activate: Box::new(|t: &mut Self| t.send(Cmd::ClosePins)),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            SubMenu { label: "Recording format".into(), submenu: formats, ..Default::default() }.into(),
            SubMenu { label: "Recording frame rate".into(), submenu: fps, ..Default::default() }.into(),
            check("Show mouse cursor in recordings", self.cursor, Cmd::ToggleCursor),
            item("Open recordings folder", Cmd::OpenFolder),
            MenuItem::Separator,
            check("Start at login", self.autostart, Cmd::ToggleAutostart),
            MenuItem::Separator,
            SubMenu { label: "About".into(), submenu: about, ..Default::default() }.into(),
            item("Exit", Cmd::Exit),
        ]
    }
}
