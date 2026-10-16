# Screen Capture

A Lightshot-style screenshot tool for Windows, written in Rust. Press **PrtSc**, select an
area, draw on it, copy it. Press **Shift+PrtSc** to record the area as an animated GIF.

Single ~1 MB executable, no runtime dependencies. Lives in the notification area.

## Build

```sh
cargo build --release
# -> target/release/screen-capture.exe
```

Requires the MSVC Rust toolchain (`x86_64-pc-windows-msvc`) and the Windows SDK (installed with
Visual Studio Build Tools), which is used to embed the icon and DPI manifest.

## Use

| Key / action | What it does |
|---|---|
| **PrtSc** | Open the capture overlay |
| **Shift+PrtSc** | Record a GIF: select an area (or click a window) and recording starts immediately |
| **PrtSc** while recording | Stop and save the GIF |
| Tray icon click | Capture; right-click for the menu |

Alt/Ctrl/Win+PrtSc are left alone, so the Windows built-ins keep working.

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
  before pixelating, the OCR engine built into Windows finds the words, and they're replaced with
  random fake words of the same size, color and roughly the same width. Images are pixelated
  normally, and text looks like pixelated text, just not the real text. What still shows is
  where words are and roughly how long, as with any redaction box. Text the OCR misses gets
  ordinary pixelation; if OCR isn't available at all, the area becomes a mosaic of its colors
  instead. The result is frozen when you draw it, so in GIFs nothing typed behind it shows.
- **Ctrl+C**, **Enter** or **double-click** copies the image to the clipboard and closes.
  **Ctrl+S** saves a PNG. **Ctrl+Z / Ctrl+Y** undo/redo. **G** records the selection as a GIF.
  **Esc** closes. **Right-click** steps back (cancels tool, then selection, then closes).

### GIFs

- Annotations drawn before pressing **G** are burned into every frame, so you can, for example,
  pixelate a private area or point an arrow at something during a recording.
- A red frame marks the recorded area. The floating bar shows the time and has **Stop** and
  **discard** buttons (drag it by its background if it's in the way). It's excluded from the
  recording.
- GIFs are saved to `Pictures\Screen Capture\` and put on the clipboard as a file, so they paste
  (still animated) into Explorer, Slack, Discord, Teams, e-mail and so on. Click the notification
  to show the file.
- Frame rate (10–30 fps) and whether the mouse cursor is recorded are in the tray menu.
- Encoding: unchanged frames are merged, and each frame stores only the changed rectangle,
  with unchanged pixels transparent. Each frame gets its own palette, and quantization runs in
  parallel. Typical UI recordings stay small.

### Tray menu

Capture, Record GIF, GIF frame rate, cursor in GIFs, open the GIF folder, **Start with Windows**
(registers the exe's current path; move the exe to its final location first), Exit.

Command line: `screen-capture.exe --capture` or `--gif` triggers a capture in the running
instance (handy for binding to another key or a shortcut).

Settings: `%APPDATA%\screen-capture\settings.ini`.

## Development

Debug builds include a non-interactive self-test that captures the screen, renders the overlay
offscreen, encodes and re-decodes GIFs, and writes the results to a folder:

```sh
cargo run -- --selftest out
```

| Module | Purpose |
|---|---|
| `main.rs` | Tray app, PrtSc keyboard hook (own thread), recording lifecycle |
| `overlay.rs` | Selection/annotation overlay, toolbars, magnifier, window snapping |
| `annotate.rs` | Shapes and their rasterization (tiny-skia) |
| `gif_rec.rs` | Recording session: capture thread, border, control bar |
| `gif_enc.rs` | GIF encoder (frame diffing, quantization, parallel LZW) |
| `capture.rs` | GDI screen capture, PNG encoding |
| `clipboard.rs` | CF_DIB + PNG images, CF_HDROP files |
| `text.rs`, `ui.rs`, `icon.rs` | Font rendering, UI drawing and icons, app icon |
