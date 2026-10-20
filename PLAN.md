# Plan

Product plan: [Snapwright: Product & Technical Plan](https://claude.ai/code/artifact/4bcb7cd7-57cc-4ff4-87cc-561af7643f9c).
Work through the current phase in order. Tick a task when it lands and add a one-line note.

## Phase 0: Spikes (weeks 1 to 4)

Gate: hotkey to overlay under 50 ms (p95) on macOS, Windows and X11.

- [x] P0-1 Workspace, toolchain pin, lints, CI on three OSes.
- [x] P0-2 `snapwright-capture`: `Capturer`, `Display`, `Frame` (BGRA), backends for
      macOS (ScreenCaptureKit), Windows (DXGI Desktop Duplication), X11 (MIT-SHM + GetImage).
      Note: X11 grabs 2560x1440 at 3.9 ms p50 under Xvfb; macOS and Windows need a real
      machine to measure (hosted runners lack permission or a GPU).
- [x] P0-3 `snapwright` CLI: `displays`, `capture`, `bench`.
- [x] P0-4 `snapwright-app`: gpui-kit shell and resident process, with a hidden window at
      launch that sets up the GPU before the first hotkey. Tray icon moved to P0-10.
- [x] P0-5 Global hotkey (`global-hotkey`): Ctrl+Shift+2, Cmd+Shift+2 on macOS.
- [x] P0-6 Overlay: one borderless pop-up window per display showing the frozen frame
      (BGRA straight into `RenderImage`, no copy); drag to select with a size label, Enter
      or release copies, a click takes the whole display, Esc cancels.
      Note: under Xvfb with a software GPU (llvmpipe), capture takes 7 to 16 ms and the
      overlay is visible 66 to 112 ms after the hotkey. Arrow-key nudge and loupe moved to P0-11.
- [x] P0-7 Copy to clipboard (PNG; `arboard` on Linux, GPUI elsewhere) and save to
      `Pictures/Snapwright`.
- [x] P0-8 Timing logged: hotkey to overlay visible, hotkey to clipboard.
      `scripts/overlay-e2e.sh` checks the whole flow under Xvfb in CI.
- [ ] P0-9 Measure on real macOS, Windows and X11 machines; record numbers here.
      First data from hosted CI runners (`snapwright bench`, VMs without a GPU):
      macOS (ScreenCaptureKit, 1024x768): first 273 ms, p50 15.5 ms, p95 26.9 ms.
      Windows (DXGI, 1024x768): first 10.7 ms, p50 0.57 ms (unchanged screen reuses the
      last frame). X11 (Xvfb, 2560x1440): first 26 ms, p50 2.5 ms, p95 2.7 ms.
- [x] P0-10 Tray icon (`tray-icon`) with Capture, Open captures folder, Quit.
      Note: Linux uses StatusNotifierItem (`ksni`), no GTK.
- [x] P0-11 Overlay polish: arrow keys nudge and Shift+arrows resize the selection; loupe
      with pixel colour; window snapping preview.
      Note: hover highlights the window under the pointer, a click takes it; Shift on release
      keeps a drag open for adjusting; `C` copies the colour under the pointer as hex.
      `Capturer::windows()` and `snapwright windows` list windows on all three OSes.
- [ ] P0-12 Pre-create one hidden overlay window per display and reuse it, if real-hardware
      numbers show window creation dominates. Waiting on P0-9: GPUI has no per-window
      hide/show, so this needs a gpui-fast patch (see DECISIONS).

## Phase 1: Alpha (weeks 5 to 14)

Gate: the team uses it daily with no capture lost in two weeks.

- [x] Window capture with snapping; capture delay timer; repeat last region.
      Note: snapping landed in P0-11. Tray "Capture in 3 seconds" (or `--capture-in=N`)
      shows a countdown that closes before the grab. Ctrl/Cmd+Alt+Shift+2 repeats the last
      region with no overlay: 5 ms from hotkey to clipboard under Xvfb.
- [x] Quick-access thumbnail after capture.
      Note: bottom-right preview for 6 s (longer while hovered); a click opens the capture in
      the library; a new capture closes it first so it never shows up in a grab.
- [x] Editor on Emulsion's engine: arrow, rectangle, ellipse, line, text, highlight, blur and
      pixelate, step counter, crop; undo and redo; non-destructive scene saved beside the image.
      Note: `snapwright-editor` renders with tiny-skia (Emulsion's rasteriser) and Emulsion's
      Gaussian blur; text uses bundled Geist. Opens from the preview's Edit, or E/Enter in the
      library. The scene and rendered result live in the library row; the original never
      changes and "Revert to original" drops the edit.
- [x] `snapwright-library`: SQLite + FTS5, content-addressed store, metadata (app, window
      title, URL, display, time), thumbnails, grid with virtual scrolling, search, tags.
      Note: every capture lands in the library with the app and title of the window under it;
      the library window (tray > Open library, or `--library`) has search, tags, notes, trash
      and copy. URLs need per-browser support and are left empty for now.
- [ ] `snapwright-bench` hotkey-to-clipboard harness in CI; signed builds.
      Note: harness done: `scripts/latency-bench.sh` presses the real hotkeys under Xvfb and
      reports p50/p95 for overlay, overlay over a preview, and repeat-to-clipboard; CI runs it
      with loose budgets and uploads `latency.json`. It caught the preview adding 120 ms to
      repeats (fixed: 5.7 ms p50). Release workflows are in (Switchyard's, see
      `docs/RELEASING.md`); signed builds wait on the signing secrets and variables.

## Phase 2: Public beta (weeks 15 to 26)

Gate: 1,000 beta users and 99.5% crash-free sessions.

- [ ] Wayland: `ext-image-copy-capture-v1` / `wlr-screencopy`, portal ScreenCast fallback.
      Note: `wlr-screencopy` done (Hyprland, Sway…), with layer-shell overlay, Hyprland window
      list and a single-instance socket for compositor keybindings (see DECISIONS).
      `ext-image-copy-capture` and the portal (GNOME, KDE) remain.
- [x] UI refresh from the design handoff (`docs/design/`): Geist, light/dark tokens mapped
      onto gpui-component (`theme.rs`), 32 px controls.
      Note: library has a brand sidebar with tag list and hotkey card, search with a Capture
      split button (region, 3 s delay, repeat), a day-grouped grid and a slimmer inspector;
      the editor has a tool rail with shortcuts, a floating colour/size bar and keycap hints;
      overlay, loupe, preview and countdown share one dark HUD material with keycap hints.
- [ ] OCR index (Vision, Windows.Media.Ocr, `ocrs`); collections; pin to screen.
      Note: OCR done (`snapwright-ocr`): every capture's text is indexed for search (backfills
      the existing library, re-reads after edits), the inspector shows it with Copy text, and
      `T` in the overlay copies the text of a selection. Collections and pin remain.
- [ ] Scrolling capture; smart redaction; beautifier; export formats; HDR tone mapping.

## Phase 3: v1.0 (weeks 27 to 36)

- [ ] Screen recording to MP4 and GIF; S3 uploads; after-capture workflows.
      Note: recording pulled forward (Jeremie, 2026-10-13). `snapwright-record` records a
      display or region with the pointer to MP4 (AVFoundation, Media Foundation, ffmpeg on
      Linux) or GIF (pure Rust, changed rectangles only), with pause. Ctrl/Cmd+Shift+6 (GIF:
      +Alt) picks the area in the overlay, a bar beside it shows time, Pause and Stop; the
      video goes to `Movies/Snapwright` and the library. `snapwright record` in the CLI.
      Sound and HD (Jeremie, 2026-10-20): MP4s record system audio and/or the microphone,
      mixed into one AAC track (cpal on macOS and Windows, `ffmpeg -f pulse` on Linux), at
      60 fps by default with Standard/High/Maximum bit rates; tray > Recording options, `A`
      and `M` in the record overlay, `--system-audio --mic --quality` in the CLI.
      S3 uploads and after-capture workflows remain.
- [ ] CLI library queries; licensing and payments; docs site.

## Open decisions

Licence, pricing, cloud sharing and team size: see the plan's decisions table.

## Follow-ups

- Updates: a Settings switch for the automatic checks and downloads (Switchyard's
  `updates.check` / `updates.auto_download`); the Shortcuts window could grow into the
  settings window.
- Shortcuts: bind digits by keycode on Hyprland (`code:11`, as Omarchy does) so the defaults
  work on layouts without digits on the unshifted row (AZERTY).
- Recording: pick the microphone (not only the default), per-source volume, and a level
  meter on the record bar; Linux system audio through the portal on GNOME/KDE.
- Recording: an output size option (1080p, 1440p) so 5K/6K displays fit H.264's 4096-wide
  limit, and 120 fps for games; HEVC where the OS encoder has it.
- Recording: macOS through a running `SCStream` (IOSurfaces straight into the writer, 60 fps)
  instead of one `SCScreenshotManager` grab per frame; Windows straight from the duplication
  texture into a GPU NV12 conversion.
- Recording: click highlights and keystroke overlays; webcam bubble; trim before saving;
  a countdown option; a dashed outline of the recorded area on screen.
- Recording: auto-zoom on clicks (Screen Studio style) as an export step, from the pointer
  path recorded alongside the video.
- Recording: the bar on Wayland (a layer-shell anchor beside the area) and a portal backend.
- Recording: write pointer positions, clicks and keystrokes to a sidecar event track next to
  the video from day one, so click highlights, keystroke badges and auto-zoom stay
  non-destructive edits (CleanShot bakes them in).
- Recording: warm the encoder before the first frame and add hotkey-to-first-frame to
  `latency-bench.sh`. CI's macOS VMs (software VideoToolbox) shed about half the frames of
  the first second.

- macOS: `SCScreenshotManager` costs ~15 ms a shot and ~270 ms cold. Try keeping an
  `SCStream` paused between captures, or starting one when the hotkey's modifiers go down.

- Windows: honour `DXGI_OUTPUT_DESC::Rotation` for portrait monitors.
- Windows: HDR desktops through `DuplicateOutput1` with an FP16 format, tone mapped to sRGB.
- macOS: display names from `NSScreen.localizedName`.
- macOS: confirm `SCShareableContent.windows` comes front to back on a real machine; if not,
  order by `CGWindowListCopyWindowInfo`.
- Library: fill `url` from browsers (macOS Accessibility / AppleScript, Windows UI Automation).
- Library: drag a thumbnail out to other apps; multi-select; import existing images.
- Captures folder: make the extra copy in `Pictures/Snapwright` a setting once settings exist.
- gpui-fast: upstream a fix for the 2 px X11 window shift, then drop `session::popup_bounds`.
- Editor: resize handles on the selected shape; zoom and pan for large captures; IME and
  caret movement in text; more fonts and a text background option.
- Editor: per-tool remembered colour and width; a magnifier (callout) tool and spotlight.
- Linux: the overlay waits 120 ms for an open preview to leave the screen (about 200 ms vs
  70 ms under Xvfb). Measure real compositors and shorten `toast::SETTLE`, or keep the preview
  hidden from the grab another way.

- Wayland: support rotated/flipped outputs in screencopy (refused today, untested).
- Wayland: window lists for Sway (`swaymsg -t get_tree`) and niri, so snapping works there.
- Wayland: the e2e and latency scripts are X11-only; a headless Sway run would cover Wayland.
- Overlay: drag handles on an adjusted selection (the design draws eight; today it resizes
  with Shift+arrows only, so no handles are drawn).
- Library: a "Capture" from the library minimizes the window first where the OS does not
  exclude our windows; check that this feels right on X11 and Windows.
- UI: light mode was checked against the design only through code; screenshot it on a light
  desktop.
- OCR: show a short "Text copied" HUD after `T` (today only the log says so); highlight the
  matching words on the image when a search hit came from its text.
- OCR: video transcripts for recordings; a language setting for Vision and Windows OCR.
