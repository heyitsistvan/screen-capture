# Screen Capture

A Lightshot-style screenshot tool for Windows and Linux, written in Rust. Press **PrtSc**,
select an area, draw on it, copy it. Press **Shift+PrtSc** to record the area as an animated
GIF.

A single executable (about 1 MB on Windows) that lives in the notification area / system tray.

## Build

```sh
cargo build --release
# Windows: target/release/screen-capture.exe
# Linux:   target/release/screen-capture
```

**Windows**: requires the MSVC Rust toolchain (`x86_64-pc-windows-msvc`) and the Windows SDK
(installed with Visual Studio Build Tools), which is used to embed the icon and DPI manifest. No
runtime dependencies.

**Linux**: needs a C toolchain, clang (for the PipeWire bindings) and the PipeWire headers:

| Distribution | Packages |
|---|---|
| Debian / Ubuntu | `build-essential pkg-config clang libpipewire-0.3-dev` |
| Fedora | `gcc pkgconf clang-devel pipewire-devel` |
| Arch | `base-devel clang pipewire` |

At runtime it needs `libpipewire-0.3` (present on any current desktop). Optional extras:

- `tesseract` (packages `tesseract-ocr` / `tesseract`): makes pixelate safe for text (see
  below). Without it, pixelate falls back to a color mosaic.
- `zenity` (or `kdialog` on KDE): the Save dialog and the custom color picker. Without them,
  Ctrl+S saves straight to `~/Pictures/Screen Capture/`.
- A tray that supports StatusNotifierItem: KDE, XFCE, Cinnamon, sway/waybar and most others
  do; GNOME needs the "AppIndicator and KStatusNotifierItem Support" extension. Without a tray,
  use PrtSc and the command line below.

## Use

| Key / action | What it does |
|---|---|
| **PrtSc** | Open the capture overlay |
| **Shift+PrtSc** | Record a GIF: select an area (or click a window) and recording starts immediately |
| **PrtSc** while recording | Stop and save the GIF |
| Tray icon click | Capture; right-click for the menu |

On Windows, Alt/Ctrl/Win+PrtSc are left alone, so the Windows built-ins keep working.

### In the overlay

- **Drag** to select an area, or **click** to snap to the window under the cursor (the desktop
  snaps to the whole monitor). A magnifier shows exact pixels and colors; **C** copies the color
  under the cursor as `#RRGGBB`.
- Move the selection by dragging inside it, resize with the handles or edges. Arrow keys nudge
  it (Shift = 10 px), Ctrl+arrows resize. Ctrl+A selects the whole monitor.
- **Tools** (vertical toolbar): Pen `P`, Line `L`, Arrow `A`, Rectangle `R`, Ellipse `E`,
  Marker `M`, Text `T`, Step numbers `N`, Pixelate `X`. Hold **Shift** for 45° lines and
  squares/circles. Step numbers count up with each click (1, 2, 3…); hold the button to drag one
  into place.
  **Mouse wheel** changes the size of the current tool. Click the color dot for the palette
  (or the `+` for any color).
- **Outline**: drawings get a halo so they're visible on any background. Pick it in the palette's
  Outline row: off, **auto** (the default), white, black, or a custom color. Auto picks black or
  white per drawing: it always contrasts with the drawing's color, and for mid-tone colors (red,
  blue, green…) it also stands out from the background under the drawing. **O** toggles it off
  and back on. It applies to what you draw next and is remembered. The marker and pixelate don't
  get one.
- **Moving and deleting**: with no tool selected, **drag any drawing to move it** (lines and
  outlines are grabbed by their stroke, so clicking inside a rectangle still moves the selection).
  **Delete** removes the drawing under the cursor. Ctrl+Z undoes moves and deletes too.
- **Text**: click to place, type (Enter for a new line, Ctrl+V pastes), click elsewhere or
  press Esc to finish. **Click a text to edit it** again, or drag it (also while typing). This
  works with no tool selected or with the Text tool.
- **Pixelate** works on anything and is safe for passwords and keys. Plain pixelated text can be
  read back by rendering guesses in the same font and comparing (tools like Depix do this). So
  before pixelating, OCR finds the words (the engine built into Windows; Tesseract on Linux),
  and they're replaced with random fake words of the same size, color and roughly the same
  width. Images are pixelated normally, and text looks like pixelated text, just not the real
  text. What still shows is where words are and roughly how long, as with any redaction box.
  Text the OCR misses gets ordinary pixelation; if OCR isn't available at all, the area becomes
  a mosaic of its colors instead. The result is frozen when you draw it, so in GIFs nothing
  typed behind it shows.
- **Ctrl+C**, **Enter** or **double-click** copies the image to the clipboard and closes.
  **Ctrl+S** saves a PNG. **Ctrl+Z / Ctrl+Y** undo/redo. **G** records the selection as a GIF.
  **Esc** closes. **Right-click** steps back (cancels tool, then selection, then closes).

### GIFs

- Annotations drawn before pressing **G** are burned into every frame, so you can, for example,
  pixelate a private area or point an arrow at something during a recording.
- A red frame marks the recorded area. The floating bar shows the time and has **Stop** and
  **discard** buttons (drag it by its background if it's in the way). It's excluded from the
  recording.
- GIFs are saved to `Pictures\Screen Capture\` (`~/Pictures/Screen Capture/` on Linux) and put on
  the clipboard as a file, so they paste (still animated) into a file manager, Slack, Discord,
  Teams, e-mail and so on. Click the notification to show the file.
- Frame rate (10–30 fps) and whether the mouse cursor is recorded are in the tray menu.
- Encoding: unchanged frames are merged, and each frame stores only the changed rectangle,
  with unchanged pixels transparent. Each frame gets its own palette, and quantization runs in
  parallel. Typical UI recordings stay small.

### Tray menu

Capture, Record GIF, GIF frame rate, cursor in GIFs, open the GIF folder, **Start with Windows** /
**Start at login** (registers the executable's current path; move it to its final location
first), About (version, links to Pixobit Solutions and the source code), Exit.

Command line: `screen-capture --capture` or `--gif` triggers a capture in the running instance
(handy for binding to another key or a shortcut). Like PrtSc, `--capture` during a recording
stops it; on Linux, `--stop` does only that.

Settings: `%APPDATA%\screen-capture\settings.ini` on Windows,
`~/.config/screen-capture/settings.ini` on Linux.

## Linux notes

Everything above works the same on X11 and Wayland, with these differences.

**X11**: PrtSc is grabbed directly. Many desktops already bind PrtSc to their own screenshot tool
(GNOME, KDE, XFCE, Cinnamon, openbox's default config…); then the app says so on startup, and you
can free the key or bind another one in the desktop's keyboard settings to
`screen-capture --capture` (and Shift+PrtSc to `screen-capture --gif`).

**Wayland** doesn't let apps read the screen or grab keys directly, so the app goes through the
desktop portals (`xdg-desktop-portal` plus your desktop's backend):

- **Screenshots** use the Screenshot portal. GNOME asks once whether to allow it.
- **PrtSc** is registered through the GlobalShortcuts portal where the desktop has one (KDE
  Plasma 5.27+, GNOME 48+); the desktop may ask you to confirm or pick the key. Elsewhere (sway,
  Hyprland, …), the app explains this once on startup; bind the keys in your compositor config,
  e.g. for sway:

  ```
  bindsym Print exec screen-capture --capture
  bindsym Shift+Print exec screen-capture --gif
  ```

- **GIF recording** uses the ScreenCast portal and PipeWire. The first recording asks which
  screen to share; the choice is remembered.
- Wayland doesn't tell apps where windows are, so **a click selects the whole screen** instead of
  a window. The overlay covers one monitor (the primary one), and the recording bar is placed by
  the compositor (drag it aside if needed); there's no red frame around the recorded area.
- wlroots compositors (sway, Hyprland, river, …) need `xdg-desktop-portal-wlr` (or
  `xdg-desktop-portal-hyprland`) **and** `xdg-desktop-portal-gtk`: the Screenshot portal only
  works when a backend for its permission dialog is installed too.

## Development

Debug builds include a non-interactive self-test that captures the screen, renders the overlay
offscreen, checks text editing, drawing, outlines and pixelate (including an attack that tries to
read pixelated passwords back), encodes and re-decodes GIFs, and writes the results to a folder:

```sh
cargo run -- --selftest out [photo-tests-dir]
```

The optional folder holds `photo_*.png` images with a photo in the left 320 px and text on the
right, to check that pixelate scrambles the text and leaves the photo alone.

Linux debug builds also have `--dump-screen <file.png>` (save the X screen), `--portal-test <dir>`
(portal screenshot plus a 3-second screen-cast recording) and
`--pipewire-test <dir> <node> <w> <h>` (record from any PipeWire video node).

| Module | Purpose |
|---|---|
| `editor.rs` | The overlay: selection, toolbars, magnifier, annotation editing; platform-neutral |
| `annotate.rs` | Shapes and their rasterization (tiny-skia), safe pixelate |
| `ocr.rs` | Shared OCR preparation and word/line merging for pixelate |
| `gif_enc.rs` | GIF encoder (frame diffing, quantization, parallel LZW) |
| `recbar.rs` | The recording bar, drawn the same on both platforms |
| `text.rs`, `ui.rs`, `icon.rs` | Font rendering, UI drawing and icons, app icon |
| `win/` | Windows: tray, PrtSc keyboard hook, overlay window, GDI capture, clipboard, WinRT OCR, recording |
| `linux/` | Linux: tray (StatusNotifierItem), X11 key grab and capture, portals, PipeWire recording, Tesseract OCR, dialogs |

## About

Made by [Pixobit Solutions](https://pixobit.eu). Source code:
[github.com/heyitsistvan/screen-capture](https://github.com/heyitsistvan/screen-capture).
