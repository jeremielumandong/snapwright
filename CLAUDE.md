# CLAUDE.md — Snapwright

Snapwright is a fast, cross-platform screenshot and screen recording tool with a searchable
capture library, written in Rust with GPUI (through gpui-kit).

- Product plan: [Snapwright: Product & Technical Plan](https://claude.ai/code/artifact/4bcb7cd7-57cc-4ff4-87cc-561af7643f9c)
- Task plan: `PLAN.md` (work through the current phase in order)
- Decisions log: `docs/DECISIONS.md` (append when you make or need a non-obvious call)
- Sister projects to borrow from: `jeremielumandong/switchyard` (GPUI app shell, theme,
  palette, updater, packaging) and `jeremielumandong/emulsion` (MIT: canvas engine, filters,
  export, thumbnails, local AI models). Check licences before copying anything else.

## How to work in this repo

1. Open `PLAN.md` and take the first unchecked task in the current phase.
2. State a short plan: files you will touch, types you will add, how you will test.
3. Implement only that task. Note drive-by ideas under "Follow-ups" in `PLAN.md`.
4. Run the checks below. All must pass.
5. Tick the task in `PLAN.md` with a one-line note.

## Commands

```bash
cargo build --workspace
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
xvfb-run -a -s "-screen 0 1920x1080x24" cargo test -p snapwright-capture --test x11 -- --ignored
cargo run --release -p snapwright-cli -- bench --runs 100
cargo run --release -p snapwright-cli -- record --seconds 3 -o clip.mp4   # or clip.gif
cargo run --release -p snapwright-app -- --capture-now   # SNAPWRIGHT_LOG=debug for timings
xvfb-run -a -s "-screen 0 1600x1000x24" scripts/overlay-e2e.sh target/release
xvfb-run -a -s "-screen 0 1920x1080x24" scripts/latency-bench.sh target/release   # p50/p95
```

Linux builds of the app need `libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev
libvulkan-dev libfontconfig-dev libfreetype-dev`, and `mesa-vulkan-drivers` to open windows
under Xvfb.

Type-check other platforms from Linux (no linking):

```bash
cargo clippy -p snapwright-capture -p snapwright-record --target aarch64-apple-darwin -- -D warnings
CC_x86_64_pc_windows_msvc=clang cargo clippy -p snapwright-capture -p snapwright-record --target x86_64-pc-windows-msvc -- -D warnings
```

## Workspace layout

```
crates/
  app/      snapwright-app      GPUI binary: global hotkey, tray, per-display overlay, clipboard, save
  capture/  snapwright-capture  Capturer, Display, Frame; one backend module per OS
  cli/      snapwright-cli      `snapwright` binary: displays, windows, capture, bench
  editor/   snapwright-editor   annotation scene (shapes, crop), tiny-skia renderer, undo history
  library/  snapwright-library  capture library: content-addressed store, SQLite + FTS5, tags, trash
  ocr/      snapwright-ocr      on-device text recognition: Vision, Windows.Media.Ocr, ocrs (Linux)
  record/   snapwright-record   screen recording: paced capture, MP4 (OS encoders / ffmpeg) and GIF
scripts/overlay-e2e.sh          hotkey to clipboard under Xvfb (needs xdotool, xclip, ImageMagick)
scripts/site-shots.sh           the site's app screenshots in every theme, under Xvfb
packaging/                      AppImage, DMG/PKG and NSIS builds (from Switchyard); see docs/RELEASING.md
install.sh                      Linux one-line installer: latest AppImage, checksum, launcher, CLI
site/                           marketing site (static HTML, deployed by .github/workflows/pages.yml)
```

## Rules

- **Speed is the product.** Every change on the capture path is measured with
  `snapwright bench` before and after. Targets: overlay visible < 50 ms after the hotkey,
  selection on the clipboard < 100 ms, idle CPU 0%.
- **Frames are BGRA, tightly packed, alpha opaque.** Convert only at export.
- **Keep OS state warm.** Backends hold devices, duplications, SHM segments and filters
  between captures; never set them up per capture.
- **`unsafe` only in platform modules**, each block with a `SAFETY:` comment.
- **The UI thread never does I/O or encoding.** Encoding, thumbnails and OCR run off it.
- No `unwrap()` / `expect()` outside tests. Errors are `thiserror` enums in library crates,
  `anyhow` in binaries. Logging with `tracing`.

## Gotchas

- macOS: `CGDisplayCreateImage` is obsolete since macOS 15; use ScreenCaptureKit.
  `SCDisplay` sizes are in points; multiply by `SCContentFilter::pointPixelScale`.
- macOS: objc2-foundation needs the `NSEnumerator` feature for `NSArray::iter`.
- Windows: Desktop Duplication needs per-monitor DPI awareness, set in `Backend::new`.
  After the first frame, `AcquireNextFrame` only returns when the screen changed; on a
  timeout reuse the last staging texture.
- Windows: a frame with `LastPresentTime == 0` only moved the pointer; on a fresh duplication
  its texture is black. Copy only frames with a present time.
- X11: Xvfb resets the root window when its last client disconnects; tests keep a
  connection open while they paint. Tests that paint the root window must not run in parallel.
- Wayland: capture is `wlr-screencopy` (wlroots compositors only; GNOME/KDE wait for the
  portal backend). `snapwright_capture::wayland_session()` picks the backend and makes
  pop-ups layer-shell surfaces (`session::popup_kind`). No global hotkeys there: the app is
  single-instance (`instance.rs`); on Hyprland it binds its keys over IPC (`hyprland.rs`,
  re-bound on every config reload; `keyword bind` on the legacy config, `eval hl.bind` on the
  Lua config Omarchy 4 runs, which refuses `keyword`), elsewhere users bind
  `snapwright-app --capture-now` in the compositor. Screencopy waits for a repaint, so it times out while the display sleeps.
- Shortcuts are settings (`shortcuts.rs`, `shortcut.capture = "Ctrl+Shift+2"` in
  `settings.toml`), changed in the Shortcuts window (`--shortcuts`, tray > Keyboard
  shortcuts…), which lifts every bind while it listens for a key.
- Theme choice (tray > Theme) lives in `settings.toml` in the config folder; "Follow Omarchy"
  reads `~/.local/state/omarchy/current/theme` (Omarchy 4) or `~/.config/omarchy/current/theme`.
  The 16 bundled Omarchy themes (`omarchy::BUNDLED`) and `site/theme.js` share one list and
  one colour mapping; change both together and re-run `scripts/site-shots.sh`, which renders
  the site's library and editor screenshots in every theme (`site/img/themes/<id>/`).
- Running the Xvfb scripts from a Wayland desktop needs `env -u WAYLAND_DISPLAY
  XDG_SESSION_TYPE=x11`, or the app and tests pick the Wayland backend.
- GPUI: `window.remove_window()` through a window's own handle fails while that window is
  being updated (its event handlers run inside that update). Close the current window
  directly and the others through their handles (`session::finish`).
- GPUI: the first window pays for GPU setup (seconds on a software renderer). The app opens a
  hidden window at launch to take that cost before the first hotkey.
- GPUI's X11 clipboard writes text only; images go through `arboard` on Linux.
- GPUI's X11 backend creates windows 2 px right of the requested origin; override-redirect
  pop-ups go through `session::popup_bounds`, which asks for 2 px less.
- The selection logic lives in `selection.rs` in frame pixels with no GPUI types; test
  interaction changes there first.
- `tray-icon` on Linux needs a D-Bus session; without one the tray is skipped.
- The library lives in the platform data folder (`~/.local/share/snapwright/library` on
  Linux); tests that set `HOME` get their own. Library calls run on background threads
  behind `session::LibraryService`'s mutex, never on the UI thread.
- `cargo run --release -p snapwright-app -- --edit-latest` opens the newest capture in the editor;
  `cargo run -p snapwright-editor --example sample -- out.png` renders every shape kind.
- `cargo run --release -p snapwright-app -- --library` opens the library window at launch;
  `--capture-in=3` starts a countdown capture.
- Anything shown on screen right after a capture (the quick-access preview) must close
  before the next grab: `session::start` and `repeat_last` dismiss it and wait
  `toast::SETTLE`.
- OCR on Linux fetches two ocrs models (~12 MB) on first use; set `SNAPWRIGHT_OCR_MODELS` to
  a folder that has them to skip the download (the ocr tests cache them in `target/tmp`).
  `cargo run --release -p snapwright-ocr --example recognize -- shot.png` prints a PNG's text.
- `RenderImage` takes BGRA pixels in an `image::RgbaImage` buffer; the channel order is not
  converted, so capture frames go in as they are. `image` must be the version GPUI links.
- Colours come from `theme::palette(cx)` (windows) or `theme::hud` (overlay, loupe, preview,
  countdown); `docs/design/` is the reference. gpui-component buttons read `button_primary*`
  and `button*`, not `primary`; `theme::apply` sets both.
- Geist has no ⌘ ⌥ ⌃ ⌫ glyphs and Linux shows a box for them; keycap text goes through
  `theme::key_label`.
- After a rebuild, a running dev app's `/proc/<pid>/exe` ends in ` (deleted)`; match by prefix
  when stopping it, or `--library` and friends get forwarded to the stale instance.
- Recording: MP4 on Linux needs `ffmpeg` on PATH (`SNAPWRIGHT_FFMPEG` overrides); its tests
  skip without it. Encoders live on their own thread for their whole life (COM and
  AVFoundation objects never cross threads). H.264 wants even sizes; the recorder rounds down.
- Recording sound: `snapwright_record::audio` mixes sources on the recorder's clock; tests
  feed a synthetic `AudioSource` through `Recorder::start_with_audio`, never a device. On
  macOS the mic needs the `audio-input` entitlement and `NSMicrophoneUsageDescription`,
  system audio `NSAudioCaptureUsageDescription` and macOS 14.6+. On Linux sound comes from
  `ffmpeg -f pulse` and is muxed in after the video (a second, copy-only ffmpeg run).
- Recording: the record overlay is the screenshot overlay in `Mode::Record`; the recorder
  starts `toast::SETTLE` after the overlays close so they are not in the first frame.
- Xvfb has no window manager: keyboard focus follows the pointer, which is fine for the
  full-screen overlay. `tracing` macros shadow a local named `display`.
- OCR models on Linux: cargo runs the `recognize` tests in parallel and each fetches the
  models into the same folder, so every fetch writes its own `.part` file before renaming
  (a shared one was renamed away under the other test: "No such file" in CI).
