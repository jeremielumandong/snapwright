# Decisions

Append an entry when a call is non-obvious or the user made it. Newest last.

## 2026-10-12: Platforms, macOS and Windows first

Jeremie chose macOS and Windows for the first release, with Linux one phase behind. X11 is
in from the start because it is cheap and testable in CI; Wayland lands in Phase 2.

## 2026-10-12: UI stack, the same as Switchyard

gpui-kit 0.7.1 (gpui-pre from Longbridge's gpui-fast fork + gpui-component 0.7.1), as in
`jeremielumandong/switchyard`. Its theme, zoom, palette, overlays, updater and packaging are
ported from there. Image and editor code comes from `jeremielumandong/emulsion` (MIT).

## 2026-10-12: Capture backends are our own, not GPUI's

GPUI's screen capture is a streaming API built for screen sharing; starting a stream costs
more than one screenshot. `snapwright-capture` calls ScreenCaptureKit's
`SCScreenshotManager`, DXGI Desktop Duplication and X11 MIT-SHM directly. Recording may still
use GPUI's stream later.

## 2026-10-12: Frames are BGRA

macOS and Windows capture in BGRA and GPUI's `RenderImage` takes BGRA, so frames go to the
overlay without conversion. Encoders that need RGBA convert at export time.

## 2026-10-12: `unsafe` only in platform modules

The workspace denies `unsafe_code`; `capture`'s `linux`, `macos` and `windows` modules opt
out, and every `unsafe` block carries a `SAFETY:` comment (clippy `undocumented_unsafe_blocks`).

## 2026-10-12: GPU warm-up window at launch

The first GPUI window creates the GPU device and pipelines: 2.8 s with a software renderer
under Xvfb. The app opens a hidden 1x1 window at launch so the first overlay does not pay
for it. Later overlays then show in 66 to 112 ms on the same software renderer.

## 2026-10-12: `arboard` for clipboard images on Linux

GPUI's X11 clipboard writes text only. On Linux the app puts captures on the clipboard with
`arboard` (MIT/Apache-2.0), kept alive for the app's life because on X11 the owner serves
every paste. macOS uses GPUI's clipboard, which supports PNG. (Windows: see 2026-10-17.)

## 2026-10-12: Tray icon through `tray-icon` with the `ksni` backend

`tray-icon` (MIT/Apache-2.0) gives the native status item on macOS and Windows. On Linux its
default backend needs GTK and libappindicator; the `ksni` backend speaks
StatusNotifierItem over D-Bus instead, so the app links no GTK. Without a D-Bus session (as
under Xvfb) the tray is skipped with a warning and the hotkey still works.

## 2026-10-12: Selection: click snaps to windows, Shift on release keeps it open

Hovering highlights the frontmost window under the pointer and a click takes it; on bare
desktop a click takes the display. A drag still copies on release, the fastest path. Holding
Shift on release keeps the region open: drag inside to move it, arrows nudge, Shift+arrows
resize (Ctrl/Cmd for 10 px), Enter copies. Windows are listed after the frames are grabbed
and before any overlay opens, so ours are never in the list; the app also drops its own pid.

## 2026-10-12: Overlay windows ask for 2 px less on X11

gpui-fast's X11 backend creates every window 2 px right of the requested origin (a Zed
workaround for GNOME). Override-redirect pop-ups keep the shift, so the frozen frame sat 2 px
off the real screen. `session::open_overlays` asks for `x - 2`; a gpui-fast fix would let us
drop that.

## 2026-10-12: P0-12 (reused overlay windows) waits for real-hardware numbers

GPUI has no per-window hide or show, so reusing overlay windows means patching gpui-fast.
Under Xvfb the overlay appears 66 to 95 ms after the hotkey, but that is a software renderer.
Do it only if P0-9's numbers on real machines show window creation is the bottleneck.

## 2026-10-12: Library store and the captures folder

The library keeps each image once, named by the BLAKE3 hash of its PNG, under the platform
data folder, with metadata, tags and an FTS5 index in SQLite (`rusqlite` with bundled
SQLite, MIT). Every capture is also still written to `Pictures/Snapwright` so people find a
plain file where they expect one; that copy becomes a setting once settings exist. Search
matches word prefixes across app, title, URL, note, tags and recognised text; FTS5 syntax in
what people type is treated as plain words.

## 2026-10-12: Repeat-last-region hotkey and delayed capture

Repeat last region is Ctrl+Alt+Shift+2 (Cmd+Option+Shift+2 on macOS), next to the capture
hotkey and clear of the system's Cmd+Shift+3/4/5. It grabs the same display and crops the same
frame pixels with no overlay. Delayed capture has no hotkey yet: tray "Capture in 3 seconds"
or `--capture-in=N`. Hotkeys another app owns are skipped with a warning instead of failing
the rest.

## 2026-10-12: Editor engine is a small crate on Emulsion's rasteriser, not emulsion-core

Emulsion's `emulsion-core` carries documents, storyboards, diagrams and cosmic-text; pulling it
in would multiply build time and binary size for what a screenshot editor needs. Snapwright's
`snapwright-editor` instead keeps a small serde scene (arrow, line, rectangle, ellipse,
highlight, text, step, blur, pixelate, crop) and renders it with tiny-skia 0.11, the rasteriser
Emulsion uses, with Emulsion's Gaussian kernel ported (MIT). Text is laid out with ab_glyph and
the bundled Geist font (OFL, copied from Emulsion's assets) as vector paths, so exports match on
every OS. The preview is rendered by the same code as the export, on a background thread, so
what the user sees is what gets copied.

Redactions (blur, pixelate) always apply to the image's own pixels under every other shape, so
nothing drawn later can be undone to reveal them, and exports contain no original pixels there.

## 2026-10-12: Edits live in the library row (schema v2)

"Saved beside the image" means: the scene JSON and the rendered image's hash are columns on the
capture. The rendered image is a second content-addressed file with its own thumbnail and is
what the library shows, copies and opens; the original file is never touched, and "Revert to
original" drops the edit. Closing the editor saves; Done also copies. Old edit files are deleted
once no capture uses them.

## 2026-10-12: Our own windows stay out of captures where the OS allows

The quick-access preview sits on screen for six seconds, often while the next capture starts.
Closing it and waiting for the compositor cost 120 ms per capture, which the latency harness
caught (repeat last region went from 5 ms to 128 ms). Now:

- macOS: the ScreenCaptureKit filter excludes this application, so none of Snapwright's own
  windows (preview, library, editor, overlay) are ever in a capture, and nothing waits.
- Windows: the preview window is marked `WDA_EXCLUDEFROMCAPTURE` (Windows 10 2004+); when that
  fails it falls back to closing and waiting.
- Linux/X11 has no such switch. Repeat last region keeps the preview up unless it covers the
  region; a new overlay still closes it and waits.

Capturing Snapwright's own library or editor window on macOS is therefore not possible; if that
turns out to matter, except those windows in the filter.

## 2026-10-12: Release builds copied from Switchyard

Jeremie asked for Switchyard's release build as is, so the workflows, packaging scripts, upload
and bump scripts and the Windows icon resource (`crates/app/build.rs`) are Switchyard's with the
names changed. Differences:

- The app binary stays `snapwright-app` and the CLI `snapwright`; the AppImage runs the CLI when
  invoked through a symlink named `snapwright`.
- macOS needs 14.0 (ScreenCaptureKit's `SCScreenshotManager`), not 11.0, and the hardened
  runtime takes no exceptions (Switchyard's library-validation exception was for its database
  drivers). Info.plist carries `NSScreenCaptureUsageDescription`.
- Switchyard's Entra client ID and updater key variables are dropped; Snapwright has neither.
- The Windows publisher check reads only `SNAPWRIGHT_SIGN_EXPECTED_SUBJECT`.

Secrets and variables are listed in `docs/RELEASING.md`.

## 2026-10-12: Wayland through wlr-screencopy, layer shell and a keybinding

Pulled forward from Phase 2 because Jeremie runs Hyprland and the app refused to start there.

- Capture uses `wlr-screencopy` into a reused `wl_shm` memfd buffer (wlroots compositors:
  Hyprland, Sway, river, Wayfire). It came first because it is the protocol every wlroots
  compositor has; `ext-image-copy-capture` and the portal (GNOME, KDE) are still to do, and
  those sessions get `CaptureError::Unsupported`.
- `Display::bounds` on Wayland is the output's logical (`xdg-output`) position times its scale,
  with the copied buffer's size, so "physical pixels" holds and the app's `bounds / scale`
  gives the logical rectangle as elsewhere. Rotated and flipped outputs are refused until
  someone can test them.
- Copies wait at most 2 s: a compositor copies on its next repaint, and a display in DPMS
  sleep never repaints.
- The window list comes from Hyprland's IPC socket; other compositors get no snapping.
- Overlay, preview and countdown are `zwlr_layer_shell_v1` surfaces on the overlay layer (a
  normal Wayland window would be tiled). The overlay takes the keyboard exclusively, and its
  GPUI display is found by the UUID GPUI derives from the connector name.
- Wayland has no global key grabs, so the app is single-instance on Linux: a second launch
  sends its flags (`--capture-now`, `--repeat-last`, `--library`, …) to the running app over
  `$XDG_RUNTIME_DIR/snapwright-<display>.sock` and exits. Users bind
  `snapwright-app --capture-now` in their compositor. A launch with no flags opens the library.
- arboard's `wayland-data-control` feature puts images on the Wayland clipboard.

## 2026-10-12: UI refresh from the design handoff

The design (`docs/design/Snapwright Redesign.dc.html` with its Library and Editor pages) is
the reference; `crates/app/src/theme.rs` holds its tokens.

- Windows follow the system light/dark setting (`theme::sync` on appearance changes); the
  capture HUDs are always dark because they sit over arbitrary screen content.
- The palette is pushed into gpui-component's `Theme`, including the `button_*` fields its
  buttons read (they do not use `primary`), so library and editor controls match the custom
  elements around them.
- Geist is bundled as static Regular, Medium and SemiBold (from Switchyard); GPUI does not pick
  weights out of the editor's variable `Geist.ttf`.
- The library grid became a variable-height `list` so each day gets a header row; arrow keys
  move by grid row across those headers (`step_vertical`).
- The note saves when the field loses focus instead of on Enter, since it is now multi-line.
- Not built from the design: drag handles on the overlay selection (no handle dragging yet)
  and the image drop shadow inside thumbnails (`object_fit` boxes have no fitted bounds to
  shadow).

## 2026-10-13: Screen recording

Jeremie asked for recording now (it was Phase 3). Choices made:

- **Encoders are the OS's.** macOS: AVAssetWriter, which uses VideoToolbox (the media engine
  on Apple silicon). Windows: a Media Foundation sink writer, which picks the GPU's H.264 MFT
  (Quick Sync, NVENC, AMF) and falls back to Microsoft's software one. Both are licensed by
  the OS vendor, so Snapwright ships no H.264 encoder and owes no patent royalties. Linux has
  no system encoder, so MP4 goes through the user's `ffmpeg` (libx264); without it MP4 fails
  with a clear message and GIF still works. Bundling openh264 was the alternative; Cisco's
  royalty cover only applies to its own downloaded binary.
- **Windows gets NV12 from us** (`yuv.rs`, BT.709 limited range), so the sink writer needs no
  colour converter and colours match the other platforms. All three tag BT.709.
- **The index comes first** (`shouldOptimizeForNetworkUse`, `MF_MPEG4SINK_MOOV_BEFORE_MDAT`,
  `+faststart`): a recording pasted into Slack or a browser plays before it fully loads.
- **Frames come from the warm `Capturer`**, one grab per tick on the capture thread and an
  encoder on its own thread behind a 3-frame queue; a slow encode drops a frame rather than
  slowing the clock. Timestamps are wall-clock minus pauses. On macOS this is one
  `SCScreenshotManager` call per frame (about 15 ms, fine for 30 fps); an `SCStream` is the
  follow-up for 60 fps.
- **The pointer** is in recordings and not in screenshots (`Capturer::set_cursor`): native on
  ScreenCaptureKit and wlr-screencopy, drawn from XFixes on X11 and from the DXGI pointer
  shape on Windows (colour, monochrome and masked-colour shapes).
- **GIF** is pure Rust (`gif` crate, NeuQuant per frame) at 15 fps, scaled down by a whole
  factor to at most 1280 px wide, and writes only the rectangle that changed, with unchanged
  pixels transparent. Still screens cost almost nothing.
- **Hotkeys:** Ctrl/Cmd+Shift+6 records MP4 and Ctrl/Cmd+Alt/Option+Shift+6 a GIF; the same key
  stops. Shift+5 is macOS's own recording toolbar. `--record` / `--record-gif` do the same
  from the command line (and over the single-instance socket on Linux).
- **The library keeps recordings as captures**: the first frame is the image (thumbnail,
  grid, search all unchanged) and the video sits beside it in the store under its own hash,
  hard-linked from `Movies/Snapwright` (schema version 3). Enter or Play opens the system
  player; the editor is for images only.
- **The bar** (time, Pause, Stop) goes under the area, else above, else inside it where the OS
  keeps our windows out of captures (macOS; Windows with `WDA_EXCLUDEFROMCAPTURE`). On X11 it
  is left out rather than recorded; on Wayland it is not shown yet (no anchor beside an
  arbitrary area), and the compositor binding stops the recording.
- **Main's red CI fixed here:** `Placement::BottomRight`'s margin is unread off Linux, and the
  e2e script's expected overlay shade and editor image offset predate the UI refresh.

No audio yet; it is the first follow-up.
## 2026-10-13: On-device OCR for search and copy text

`snapwright-ocr` wraps what each OS has: Apple Vision (accurate level, language correction),
`Windows.Media.Ocr` (profile languages, English fallback) and, on Linux, ocrs (pure Rust,
MIT/Apache). Nothing is sent anywhere.

- ocrs models (`text-detection.rten`, `text-recognition.rten`, ~12 MB, trained on HierText,
  CC BY-SA 4.0) are fetched on first use into the data folder (`ocr-models` beside
  `library`) and pinned by BLAKE3; `SNAPWRIGHT_OCR_MODELS` points at a folder that already
  has them (packagers, offline machines, CI). Bundling them would add 12 MB to every Linux
  download for a feature that works a minute later anyway.
- ocrs squeezes any image into its 800x600 detection input, which loses screenshot-sized text,
  so Linux detects words per overlapping 800x600 tile and reads lines on the whole image.
  Windows tiles the same way past `OcrEngine::MaxImageDimension`, and macOS reads at 2x in
  1024 px tiles, since Vision garbles small text on a whole 1080p frame. Each engine still
  misreads a letter of 14 to 16 px Geist now and then (Windows reads "l" as "t" at any scale,
  ocrs 16 px "y" as "v"), so the whole-screen test checks that every line is read once and
  accepts a word one edit off. On Linux, images under 0.24 MP (a
  selection) are read at 2x; full screens at 1x, which reads about the same text in a third
  of the time (1 to 1.5 s for 1600x1000 on four cores).
- The library stores the text of the image it shows (the edit when there is one) and which
  image that was (`ocr_hash`). An edit or revert makes the capture pending again, so blurring
  a secret also takes it out of search. It is schema v4.
- One worker thread owns the engine and blocks on a channel (0% idle). Library changes wake
  it; it waits for a quiet second, then reads pending captures newest first, which also
  backfills an existing library. `T` in the overlay sends the selection to the same worker
  ahead of the queue and puts the text on the clipboard.

## 2026-10-13: In-app updates, ported from Switchyard

Jeremie asked for auto-update like Switchyard's. `crates/app/src/update.rs` is Switchyard's
`switchyard-core::update` with the names changed, and `updates.rs` its UI state. Differences:

- Blocking `ureq` (already used for the OCR models) on GPUI's background executor, not
  `reqwest` on tokio; Snapwright has no tokio runtime. No progress or cancel. Like
  Switchyard it trusts the OS certificate store (ureq's `platform-verifier`), so it works
  behind TLS-inspecting proxies. Redirects are followed by hand, https only: ureq's own
  redirect handling failed to read GitHub's chunked 302 body through a proxy.
- No minisign check: Snapwright has no update key (see the release builds entry). Integrity
  is the SHA-256 GitHub records for each asset, or the `.sha256` file beside it, over an
  HTTPS download from the github.com release.
- No settings window yet, so the UI is one tray item: "Check for updates", then
  "Downloading…", then "Restart to update to X" (AppImage replaced in place, Windows
  installer run after exit) or "Install Snapwright X" (macOS opens the verified DMG). Copies
  that can't update themselves get "Download Snapwright X", which opens the release page.
- The app is resident, so release builds check 30 s after launch and then once a day, not
  only at startup. `SNAPWRIGHT_NO_UPDATE_CHECK` turns the automatic checks off. Restarting
  waits while a recording runs.

## 2026-10-13: Hyprland shortcuts and following the Omarchy theme

- Wayland has no global key grabs, so Ctrl+Shift+2 did nothing on Hyprland (Omarchy) unless
  the user bound `--capture-now` themselves. On Hyprland the app now adds the binds over the
  IPC socket (`keyword bind CTRL SHIFT, 2, exec, '<exe>' --capture-now`, and the repeat and
  record keys). Hyprland drops runtime binds when it reloads its config (every Omarchy theme
  switch), so a thread on the event socket re-adds them on `configreloaded`. A key the user
  already bound to something else is left alone; an older Snapwright bind (moved AppImage) is
  replaced. The binds outlive the app, so a press after Quit launches it and captures. The
  GlobalShortcuts portal was the alternative, but Hyprland's portal still needs a `global`
  bind in the user's config, so it would not have made the key work on its own.
- Theme: the tray has Theme > Match system / Light / Dark / Follow Omarchy, saved in
  `settings.toml` in the config folder. The Omarchy reader comes from Switchyard (same author,
  Apache-2.0) and maps the theme's background, foreground, accent and terminal colors onto the
  window palette; HUDs stay dark. With no saved choice, Omarchy machines start on Follow
  Omarchy. Nothing polls (idle CPU stays 0%): the theme is re-read on a Hyprland config reload
  and when a library or editor window comes to the front.

## 2026-10-13: Omarchy themes in the app and on the site

From the "Omarchy theme implementation" design handoff.

- The app bundles 16 Omarchy themes (Tokyo Night through White, colours from each theme's
  `colors.toml`) as `omarchy::BUNDLED`, listed under Theme in the tray after Match system,
  Light, Dark and Follow Omarchy. On Omarchy, Follow Omarchy stays the default.
- One mapping turns any Omarchy theme into the window palette, as the handoff specifies:
  background is the window, `dark_background` the sidebar, `darker_background` the canvas,
  `selection` the selected row, the brightest foreground the text, the accent the accent
  (with the darkest shade as its text when the accent is light, white otherwise), red the
  danger colour. Themes on disk that leave a shade out get one mixed from background and
  foreground; a pale `bright_foreground` on a light theme (Aether's) falls back to
  `foreground`. Secondary text is nudged only if it would drop below readable contrast.
- The site has the same 17 themes (Snapwright's own look plus the 16) behind a Theme button
  in the header, which opens the handoff's theme dock (chips, ← → to cycle), saved in
  localStorage and settable with `?theme=`. The handoff's dock was always on screen; on the
  real site it opens from the header so it doesn't cover the page.
- The library and editor screenshots on the site are real app windows in each theme,
  rendered by `scripts/site-shots.sh` from a sample dashboard
  (`scripts/site-shots/northwind.html`) and a seeded library
  (`snapwright-editor`'s `seed` example). The overlay, countdown and preview shots stay as
  they are: those HUDs are always dark.

## 2026-10-14: Shortcuts on Omarchy 4, and changing them

- Omarchy 4 runs Hyprland's Lua config, and Hyprland refuses `keyword` under it ("keyword
  can't work with non-legacy parsers"), so the IPC binds from 2026-10-13 never took on
  Omarchy 4 and Ctrl+Shift+2 did nothing. The app now asks Hyprland first (`eval return
  true` answers `ok` only under Lua) and binds with `eval hl.bind(...)` there, with
  `keyword bind` kept for the legacy config. Lua binds list only as `__lua` in
  `hyprctl binds`, so ours carry the description "Snapwright: …" to be found again.
- Shortcuts are settings: tray > Keyboard shortcuts… (or Change on the library's capture
  card) opens a window that listens for the next key press per action, with Clear and
  Reset to defaults. They save as `shortcut.<action> = "Ctrl+Shift+2"` (or `"off"`) in
  `settings.toml`, only where they differ from the defaults, and apply at once: global
  hotkeys are re-registered, Hyprland binds on old keys are removed. While the window
  listens, the binds are lifted, so pressing the current shortcut records it.
- A shortcut ends in a letter, a digit, F1 to F24 or Print; letters and digits need Ctrl,
  Alt or Super so typing never sets one off. Hyprland binds use key names (`2`), so on
  layouts without digits on the unshifted row the digit defaults need changing; binding by
  keycode is a follow-up.

## 2026-10-14: AccessKit off on macOS (Touch Bar crash)

- v0.2.4 aborted a few seconds after launch on a Touch Bar MacBook Pro (M1, macOS 26.3):
  an uncaught `NSRangeException` from `-[_NSTouchBarFinderObservation invalidate]` removing
  a key-value observer during a display-cycle flush. GPUI's AccessKit adapter
  (`accesskit_macos::SubclassingAdapter`) subclasses each window's content view at runtime
  and restores the class when the window goes, which leaves the Touch Bar's observers
  unregistered (zed-industries/zed#64819; Zed's fix, #65186, attaches the plain adapter to
  `GPUIView` instead). gpui-fast, up to d599978, still has the subclassing adapter.
- The app now starts with `Application::new_inaccessible` on macOS, so the adapter is
  never attached. GPUI windows lose VoiceOver support until gpui-fast picks up the upstream
  fix; switch back to `gpui_kit::application()` then.

## 2026-10-17: Windows clipboard images as a bitmap and PNG

- GPUI's Windows clipboard writes images in the registered "PNG" format only. Browsers read
  that, but Paint, Office and Win+V clipboard history read bitmaps, so a copied capture
  looked like it never reached the clipboard (reported from the library's Copy button).
- The app now writes the clipboard itself on Windows with `clipboard-win` (MIT, already in
  the tree through `arboard`): a bottom-up `CF_DIBV5` built from the RGBA pixels
  (`output::dib_v5`; Windows synthesises `CF_DIB` and `CF_BITMAP` from it) and the same PNG
  bytes as before, off the UI thread. A top-down DIB is avoided because Word cannot paste
  one. macOS keeps GPUI's `public.png` write.

## 2026-10-20: Recording sound and HD

- Sound is captured in-process and mixed on the recorder's clock, not muxed by the OS
  from a second capture session, so pause, the start offset and both sources stay in one
  timeline: each source feeds a 48 kHz stereo queue (linear resampling), and the capture
  thread takes the mix 250 ms behind the latest frame, padding a silent source with zeros
  (WASAPI loopback sends nothing while nothing plays). Sound queued before the start or
  during a pause is dropped; a source more than 500 ms ahead loses its oldest sound.
- Devices go through cpal 0.18 (Apache-2.0) on macOS and Windows: its input stream on the
  default output device is WASAPI loopback, and on macOS a Core Audio process tap (14.6+).
  That links `AudioHardwareCreateProcessTap` (14.2+), so the macOS minimum is now 14.2. No
  ScreenCaptureKit audio stream: it would need a running `SCStream`, which the capture
  backend does not have yet. Linux uses `ffmpeg -f pulse` (the monitor source for system
  audio), like MP4 encoding there, so no ALSA/PipeWire build dependency is added.
- A source that cannot start (no device, permission refused, no Pulse server) is logged and
  left out; the recording goes on. The app's entitlements now include
  `com.apple.security.device.audio-input`, which the hardened runtime needs for the mic.
- MP4s default to the recorded display's refresh rate (`Display::refresh_hz`: Core
  Graphics display mode, `EnumDisplaySettingsW`, the RandR CRTC mode, the wl_output mode;
  60 when unknown), up to 240 fps; the tray also offers fixed 60 and 30. On macOS the
  per-frame `SCScreenshotManager` grab (~15 ms) holds real rates near 60 until recording
  moves to a running `SCStream`; the recorder drops frames rather than time.
- MP4s default to High quality (0.15 bit per pixel per frame, H.264 High
  profile; 19 Mbit/s for 1080p60, 45 for 1440p144). Standard keeps the old 0.1, Maximum is 0.3; caps are 50,
  100 and 160 Mbit/s. Linux maps them to x264 CRF 20, 17 and 14.
