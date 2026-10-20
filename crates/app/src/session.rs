//! One capture session: grab every display, show an overlay per display, deliver the
//! selection, close the overlays. A recording starts the same way: the selection is the area
//! to record instead of the pixels to keep.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, Global, Task, Window,
    WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions, point, px, size,
};
#[cfg(target_os = "macos")]
use gpui_kit::{ClipboardItem, Image, ImageFormat};
use snapwright_capture::{Capturer, Display, Frame, Rect, WindowInfo};
use snapwright_library::{Library, Metadata, NewCapture, Source};

use crate::output;
use crate::overlay::Overlay;

/// The capture backend, kept warm for the life of the app. Captures run on a background
/// thread, so it sits behind a mutex.
struct CaptureService(Arc<Mutex<Capturer>>);
impl Global for CaptureService {}

/// The capture library, if it could be opened. Writes happen on background threads.
#[derive(Clone)]
pub struct LibraryService(pub Option<Arc<Mutex<Library>>>);
impl Global for LibraryService {}

/// A window the selection can snap to, in one frame's pixels, with what it showed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapTarget {
    pub rect: Rect,
    pub app: String,
    pub title: String,
}

/// The Linux clipboard (GPUI's X11 clipboard writes text only).
#[cfg(target_os = "linux")]
struct Clipboard(Arc<output::LinuxClipboard>);
#[cfg(target_os = "linux")]
impl Global for Clipboard {}

/// The last region delivered, for "repeat last region".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LastRegion {
    display_id: u64,
    rect: Rect,
}
impl Global for LastRegion {}

/// What the overlay's selection is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// A screenshot: copy and keep the selected pixels.
    #[default]
    Capture,
    /// A recording of the selected area.
    Record(snapwright_record::Format),
}

/// Overlay windows of the session in progress, if any.
#[derive(Default)]
struct Active {
    capturing: bool,
    windows: Vec<AnyWindowHandle>,
    mode: Mode,
    /// The displays the overlays show, for starting a recording on one.
    displays: Vec<Display>,
}
impl Global for Active {}

/// GPUI's X11 backend creates every window 2 px right of where it was asked (a Zed
/// workaround for GNOME). Override-redirect pop-ups keep that shift, which would offset the
/// frozen frame from the real screen.
#[cfg(target_os = "linux")]
const X11_SHIFT: f32 = 2.0;

/// Bounds to request for a pop-up window so it lands exactly at `bounds`.
pub fn popup_bounds(bounds: Bounds<gpui_kit::Pixels>) -> Bounds<gpui_kit::Pixels> {
    #[cfg(target_os = "linux")]
    if !snapwright_capture::wayland_session() {
        return Bounds::new(
            point(bounds.origin.x - px(X11_SHIFT), bounds.origin.y),
            bounds.size,
        );
    }
    bounds
}

/// Where a pop-up goes on Wayland, which places surfaces itself.
#[derive(Debug, Clone, Copy)]
// Only Wayland reads the margin.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub enum Placement {
    /// The whole display (the overlay).
    Fill,
    Centered,
    /// The bottom-right corner, this far from the edges.
    BottomRight(f32),
}

/// The window kind for a pop-up: a plain pop-up (override-redirect on X11), or on Wayland a
/// layer-shell surface above everything, since a normal Wayland window is tiled or placed by
/// the compositor. `keyboard` gives the surface the keyboard while it is up.
pub fn popup_kind(placement: Placement, keyboard: bool) -> WindowKind {
    #[cfg(target_os = "linux")]
    if snapwright_capture::wayland_session() {
        use gpui_kit::layer_shell::{Anchor, KeyboardInteractivity, Layer, LayerShellOptions};
        let (anchor, margin) = match placement {
            Placement::Fill => (
                Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT,
                None,
            ),
            Placement::Centered => (Anchor::empty(), None),
            Placement::BottomRight(m) => (
                Anchor::BOTTOM | Anchor::RIGHT,
                Some((px(0.), px(m), px(m), px(0.))),
            ),
        };
        return WindowKind::LayerShell(LayerShellOptions {
            namespace: "snapwright".into(),
            layer: Layer::Overlay,
            anchor,
            // -1: draw over panels and bars instead of beside them.
            exclusive_zone: matches!(placement, Placement::Fill).then(|| px(-1.)),
            margin,
            keyboard_interactivity: if keyboard {
                KeyboardInteractivity::Exclusive
            } else {
                KeyboardInteractivity::None
            },
            ..Default::default()
        });
    }
    let _ = (placement, keyboard);
    WindowKind::PopUp
}

/// GPUI's display for a capture display. Wayland outputs are matched by connector name
/// (GPUI's display UUID is derived from it); elsewhere by origin.
fn gpui_display(
    display: &Display,
    wanted: Bounds<gpui_kit::Pixels>,
    screens: &[std::rc::Rc<dyn gpui_kit::PlatformDisplay>],
) -> Option<std::rc::Rc<dyn gpui_kit::PlatformDisplay>> {
    #[cfg(target_os = "linux")]
    if snapwright_capture::wayland_session() {
        let uuid = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_DNS, display.name.as_bytes());
        return screens
            .iter()
            .find(|d| d.uuid().is_ok_and(|u| u == uuid))
            .cloned();
    }
    let _ = display;
    screens
        .iter()
        .find(|d| {
            let o = d.bounds().origin;
            (o.x - wanted.origin.x).abs() < px(2.) && (o.y - wanted.origin.y).abs() < px(2.)
        })
        .cloned()
}

/// The app's warm capturer, shared with recordings.
pub fn capturer(cx: &App) -> Arc<Mutex<Capturer>> {
    cx.global::<CaptureService>().0.clone()
}

/// Connects to the platform capture API once, at startup.
pub fn init(cx: &mut App) -> snapwright_capture::Result<()> {
    let capturer = Capturer::new()?;
    cx.set_global(CaptureService(Arc::new(Mutex::new(capturer))));
    cx.set_global(Active::default());
    let library = match output::library_dir().and_then(|dir| Ok(Library::open(dir)?)) {
        Ok(lib) => Some(Arc::new(Mutex::new(lib))),
        Err(e) => {
            tracing::error!(error = %e, "capture library unavailable; captures are not kept");
            None
        }
    };
    cx.set_global(LibraryService(library));
    #[cfg(target_os = "linux")]
    cx.set_global(Clipboard(Arc::new(output::LinuxClipboard::new())));
    warm_up(cx);
    Ok(())
}

/// Opens a hidden 1x1 window at startup so the GPU device, swapchain setup and shaders are
/// ready before the first hotkey press; without it the first overlay pays for all of that.
fn warm_up(cx: &mut App) {
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::new(
            point(px(0.), px(0.)),
            size(px(1.), px(1.)),
        ))),
        titlebar: None,
        focus: false,
        show: false,
        kind: popup_kind(Placement::Centered, false),
        window_background: WindowBackgroundAppearance::Transparent,
        is_movable: false,
        is_resizable: false,
        is_minimizable: false,
        app_id: Some("snapwright".into()),
        ..Default::default()
    };
    if let Err(e) = cx.open_window(options, |_, cx| cx.new(|_| gpui_kit::EmptyView)) {
        tracing::warn!(error = %e, "could not pre-create the GPU context");
    }
}

/// Starts a session: captures every display off the UI thread, then shows the overlays.
/// Ignored while a session is already open.
pub fn start(pressed_at: Instant, cx: &mut App) {
    start_mode(Mode::Capture, pressed_at, cx);
}

/// Starts a session whose selection is used as `mode` says.
pub fn start_mode(mode: Mode, pressed_at: Instant, cx: &mut App) {
    let active = cx.global_mut::<Active>();
    if active.capturing || !active.windows.is_empty() {
        return;
    }
    active.capturing = true;
    active.mode = mode;
    // A quick-access preview still on screen would end up in the capture.
    let settle = crate::toast::dismiss(cx).then_some(crate::toast::SETTLE);
    let capturer = cx.global::<CaptureService>().0.clone();
    let timer = settle.map(|d| cx.background_executor().timer(d));
    let grab = cx.background_executor().spawn(async move {
        if let Some(timer) = timer {
            timer.await;
        }
        let mut capturer = capturer
            .lock()
            .map_err(|_| anyhow::anyhow!("capture backend poisoned by an earlier panic"))?;
        let displays = capturer.displays()?;
        let shots = displays
            .into_iter()
            .map(|d| capturer.capture_display(&d).map(|f| (d, f)))
            .collect::<Result<Vec<_>, _>>()?;
        // Listed before any overlay opens, so none of ours are in it. Snapping is a nicety:
        // a failure here still shows the overlay.
        let windows = capturer.windows().unwrap_or_else(|e| {
            tracing::warn!(error = %e, "window list unavailable; snapping off");
            Vec::new()
        });
        anyhow::Ok((shots, windows))
    });
    cx.spawn(async move |cx| {
        let shots = grab.await;
        cx.update(|cx| {
            cx.global_mut::<Active>().capturing = false;
            match shots {
                Ok((shots, windows)) => {
                    tracing::debug!(
                        ms = pressed_at.elapsed().as_secs_f64() * 1000.0,
                        displays = shots.len(),
                        windows = windows.len(),
                        "captured"
                    );
                    open_overlays(shots, &windows, mode, pressed_at, cx);
                }
                Err(e) => tracing::error!(error = %e, "capture failed"),
            }
        });
    })
    .detach();
}

/// Snap targets for one display: other processes' windows, in that frame's pixels.
fn snap_targets(display: &Display, frame: &Frame, windows: &[WindowInfo]) -> Vec<SnapTarget> {
    let own = std::process::id();
    let b = display.bounds;
    // Frames can differ from the display's reported size (scaling); map proportionally.
    let sx = frame.width as f32 / b.width.max(1) as f32;
    let sy = frame.height as f32 / b.height.max(1) as f32;
    windows
        .iter()
        .filter(|w| w.pid != own)
        .filter_map(|w| Some((w, w.bounds.intersect(&b)?)))
        .map(|(w, r)| SnapTarget {
            rect: Rect::new(
                ((r.x - b.x) as f32 * sx).round() as i32,
                ((r.y - b.y) as f32 * sy).round() as i32,
                (r.width as f32 * sx).round() as u32,
                (r.height as f32 * sy).round() as u32,
            ),
            app: w.app.clone(),
            title: w.title.clone(),
        })
        .collect()
}

/// Describes a selection for the library: a window when it is exactly a snap target, the
/// display when it is the whole frame, otherwise a region labelled with the frontmost window
/// under its centre.
pub fn describe(
    r: Rect,
    frame_width: u32,
    frame_height: u32,
    targets: &[SnapTarget],
    display_id: u64,
) -> Metadata {
    let centre = (r.x + (r.width / 2) as i32, r.y + (r.height / 2) as i32);
    let (source, under) = if let Some(t) = targets.iter().find(|t| t.rect == r) {
        (Source::Window, Some(t))
    } else if r == Rect::new(0, 0, frame_width, frame_height) {
        (Source::Display, None)
    } else {
        (
            Source::Region,
            targets.iter().find(|t| t.rect.contains(centre.0, centre.1)),
        )
    };
    Metadata {
        source,
        app: under.map(|t| t.app.clone()).unwrap_or_default(),
        window_title: under.map(|t| t.title.clone()).unwrap_or_default(),
        url: String::new(),
        display_id: Some(display_id),
    }
}

fn open_overlays(
    shots: Vec<(Display, Frame)>,
    on_screen: &[WindowInfo],
    mode: Mode,
    pressed_at: Instant,
    cx: &mut App,
) {
    let screens = cx.displays();
    let mut windows = Vec::with_capacity(shots.len());
    cx.global_mut::<Active>().displays = shots.iter().map(|(d, _)| d.clone()).collect();
    for (display, frame) in shots {
        let b = display.bounds;
        let s = display.scale;
        let wanted = Bounds::new(
            point(px(b.x as f32 / s), px(b.y as f32 / s)),
            size(px(b.width as f32 / s), px(b.height as f32 / s)),
        );
        // Prefer GPUI's own idea of the display, so the window lands exactly on it. Wayland
        // anchors the overlay to the output's edges, so only the output matters there.
        let screen = gpui_display(&display, wanted, &screens);
        let bounds = match &screen {
            Some(d) if !snapwright_capture::wayland_session() => d.bounds(),
            _ => wanted,
        };
        let bounds = popup_bounds(bounds);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            display_id: screen.map(|d| d.id()),
            titlebar: None,
            focus: true,
            show: true,
            kind: popup_kind(Placement::Fill, true),
            is_movable: false,
            is_resizable: false,
            is_minimizable: false,
            window_background: WindowBackgroundAppearance::Opaque,
            app_id: Some("snapwright".into()),
            ..Default::default()
        };
        let targets = snap_targets(&display, &frame, on_screen);
        match cx.open_window(options, |window, cx| {
            cx.new(|cx| Overlay::new(frame, targets, mode, pressed_at, window, cx))
        }) {
            Ok(handle) => windows.push(handle.into()),
            Err(e) => {
                let display_id = display.id;
                tracing::error!(error = %e, display_id, "overlay window failed");
            }
        }
    }
    cx.global_mut::<Active>().windows = windows;
}

/// Captures the last delivered region again, without the overlay: same display, same
/// pixels. Falls back to a normal session when nothing was captured yet or that display is
/// gone.
pub fn repeat_last(pressed_at: Instant, cx: &mut App) {
    let Some(last) = cx.try_global::<LastRegion>().copied() else {
        start(pressed_at, cx);
        return;
    };
    let active = cx.global_mut::<Active>();
    if active.capturing || !active.windows.is_empty() {
        return;
    }
    active.capturing = true;
    repeat_grab(last, pressed_at, false, false, cx);
}

/// What a repeat grab found.
enum Repeat {
    Shot(Frame, Metadata),
    /// One of our own windows (the quick-access preview) covers the region.
    Covered,
    /// The display is gone or the region is off it.
    Gone,
}

/// Grabs `last` again. The quick-access preview stays up unless it covers the region and the
/// OS cannot keep it out of the capture (then it is closed and the grab retried once it has
/// left the screen), so repeats in a row do not each wait for the previous one's preview.
fn repeat_grab(last: LastRegion, pressed_at: Instant, retry: bool, settle: bool, cx: &mut App) {
    let capturer = cx.global::<CaptureService>().0.clone();
    let timer = settle.then(|| cx.background_executor().timer(crate::toast::SETTLE));
    let grab = cx.background_executor().spawn(async move {
        if let Some(timer) = timer {
            timer.await;
        }
        let mut capturer = capturer
            .lock()
            .map_err(|_| anyhow::anyhow!("capture backend poisoned by an earlier panic"))?;
        let Some(display) = capturer
            .displays()?
            .into_iter()
            .find(|d| d.id == last.display_id)
        else {
            return Ok(Repeat::Gone);
        };
        let windows = capturer.windows().unwrap_or_default();
        if !retry
            && !snapwright_capture::EXCLUDES_OWN_WINDOWS
            && own_window_covers(&display, &windows, last.rect)
        {
            return Ok(Repeat::Covered);
        }
        let frame = capturer.capture_display(&display)?;
        let targets = snap_targets(&display, &frame, &windows);
        let Some(region) = last
            .rect
            .intersect(&Rect::new(0, 0, frame.width, frame.height))
        else {
            return Ok(Repeat::Gone);
        };
        let meta = describe(region, frame.width, frame.height, &targets, display.id);
        let crop = frame
            .crop(region)
            .ok_or_else(|| anyhow::anyhow!("region outside the display"))?;
        anyhow::Ok(Repeat::Shot(crop, meta))
    });
    cx.spawn(async move |cx| {
        let shot = grab.await;
        cx.update(|cx| match shot {
            Ok(Repeat::Shot(frame, meta)) => {
                cx.global_mut::<Active>().capturing = false;
                keep(frame, meta, pressed_at, cx);
            }
            Ok(Repeat::Covered) => {
                let settle = crate::toast::dismiss(cx);
                repeat_grab(last, pressed_at, true, settle, cx);
            }
            Ok(Repeat::Gone) => {
                cx.global_mut::<Active>().capturing = false;
                tracing::info!("last region's display is gone; selecting a new one");
                start(pressed_at, cx);
            }
            Err(e) => {
                cx.global_mut::<Active>().capturing = false;
                tracing::error!(error = %e, "repeat capture failed");
            }
        });
    })
    .detach();
}

/// Whether one of this process's windows overlaps `region` (display-local pixels).
fn own_window_covers(display: &Display, windows: &[WindowInfo], region: Rect) -> bool {
    let own = std::process::id();
    let b = display.bounds;
    let global = Rect::new(b.x + region.x, b.y + region.y, region.width, region.height);
    windows
        .iter()
        .any(|w| w.pid == own && w.bounds.intersect(&global).is_some())
}

/// Closes every overlay of the session. `current` is the overlay whose event handler is
/// running: it is mid-update, so it can only be removed directly, not through its handle.
pub fn finish(current: &mut Window, cx: &mut App) {
    let windows = std::mem::take(&mut cx.global_mut::<Active>().windows);
    let this = current.window_handle();
    for handle in windows {
        if handle == this {
            current.remove_window();
        } else if let Err(e) = handle.update(cx, |_, window, _| window.remove_window()) {
            tracing::warn!(error = %e, "could not close an overlay");
        }
    }
}

/// Ends a recording session with a selection: closes the overlays and starts recording
/// `region` (frame pixels) of display `display_id`.
pub fn deliver_recording(
    display_id: u64,
    region: Rect,
    meta: Metadata,
    current: &mut Window,
    cx: &mut App,
) {
    let active = cx.global::<Active>();
    let mode = active.mode;
    let display = active.displays.iter().find(|d| d.id == display_id).cloned();
    finish(current, cx);
    match (mode, display) {
        (Mode::Record(format), Some(display)) => {
            crate::recording::begin(display, region, format, meta, cx);
        }
        _ => tracing::error!(display_id, "no display to record"),
    }
}

/// Ends the session with a selection: closes the overlays at once, remembers the region for
/// "repeat last region", then hands the pixels to [`keep`].
pub fn deliver(
    frame: Frame,
    region: Rect,
    meta: Metadata,
    pressed_at: Instant,
    current: &mut Window,
    cx: &mut App,
) {
    finish(current, cx);
    cx.set_global(LastRegion {
        display_id: frame.display_id,
        rect: region,
    });
    keep(frame, meta, pressed_at, cx);
}

/// Encodes a finished capture off the UI thread, puts the PNG on the clipboard, and keeps it
/// in the library and the captures folder.
fn keep(frame: Frame, meta: Metadata, pressed_at: Instant, cx: &mut App) {
    #[cfg(target_os = "linux")]
    let clipboard = cx.global::<Clipboard>().0.clone();
    let library = cx.global::<LibraryService>().0.clone();
    let (width, height) = (frame.width, frame.height);
    let encode = cx.background_executor().spawn(async move {
        let rgba = frame.to_rgba();
        let png = output::encode_png(frame.width, frame.height, &rgba)?;
        #[cfg(target_os = "linux")]
        clipboard.set_image(frame.width, frame.height, rgba.clone())?;
        #[cfg(target_os = "windows")]
        output::set_windows_image(frame.width, frame.height, &rgba, &png)?;
        anyhow::Ok((png, rgba))
    });
    cx.spawn(async move |cx| {
        let (png, rgba) = match encode.await {
            Ok(done) => done,
            Err(e) => {
                tracing::error!(error = %e, "copying the capture failed");
                return;
            }
        };
        #[cfg(target_os = "macos")]
        cx.update(|cx| {
            cx.write_to_clipboard(ClipboardItem::new_image(&Image::from_bytes(
                ImageFormat::Png,
                png.clone(),
            )));
        });
        tracing::info!(
            ms = pressed_at.elapsed().as_secs_f64() * 1000.0,
            "on the clipboard (from hotkey, including selection time)"
        );
        let keep = cx.background_executor().spawn(async move {
            let mut stored = None;
            if let Some(library) = library {
                let added = library
                    .lock()
                    .map_err(|_| anyhow::anyhow!("library poisoned by an earlier panic"))
                    .and_then(|mut lib| {
                        let id = lib.add(&NewCapture {
                            png: &png,
                            rgba: &rgba,
                            width,
                            height,
                            meta,
                        })?;
                        let thumb = lib.thumb_path_of(&lib.get(id)?);
                        Ok((id, thumb))
                    });
                match added {
                    Ok((id, thumb)) => {
                        tracing::info!(id, "added to the library");
                        stored = Some((id, thumb));
                    }
                    Err(e) => tracing::error!(error = %e, "adding to the library failed"),
                }
            }
            match output::save(&png) {
                Ok(path) => tracing::info!(path = %path.display(), "saved"),
                Err(e) => tracing::error!(error = %e, "saving the capture failed"),
            }
            stored
        });
        let stored = keep.await;
        cx.update(|cx| {
            crate::library_view::notify_changed(cx);
            if let Some((id, thumb)) = stored {
                crate::toast::show(id, thumb, (width, height), None, cx);
            }
        });
    })
    .detach();
}

/// Puts an image already encoded as `png` (from these RGBA pixels) on the clipboard. Call it
/// on the UI thread; on Linux and Windows the clipboard write itself is handed to a
/// background thread.
pub fn copy_image(width: u32, height: u32, rgba: Vec<u8>, png: Vec<u8>, cx: &mut App) {
    #[cfg(target_os = "linux")]
    {
        let _ = png;
        let clipboard = cx.global::<Clipboard>().0.clone();
        cx.background_executor()
            .spawn(async move {
                if let Err(e) = clipboard.set_image(width, height, rgba) {
                    tracing::error!(error = %e, "copying the image failed");
                }
            })
            .detach();
    }
    #[cfg(target_os = "windows")]
    cx.background_executor()
        .spawn(async move {
            if let Err(e) = output::set_windows_image(width, height, &rgba, &png) {
                tracing::error!(error = %e, "copying the image failed");
            }
        })
        .detach();
    #[cfg(target_os = "macos")]
    {
        let _ = (width, height, rgba);
        cx.write_to_clipboard(ClipboardItem::new_image(&Image::from_bytes(
            ImageFormat::Png,
            png,
        )));
    }
}

/// Puts a PNG file on the clipboard, reading (and on Linux and Windows decoding) it off the
/// UI thread. The task finishes once the image is on the clipboard.
pub fn copy_file_to_clipboard(path: std::path::PathBuf, cx: &mut App) -> Task<anyhow::Result<()>> {
    #[cfg(target_os = "linux")]
    let clipboard = cx.global::<Clipboard>().0.clone();
    let read = cx.background_executor().spawn(async move {
        let png = std::fs::read(&path)?;
        #[cfg(target_os = "linux")]
        {
            let (width, height, rgba) = output::decode_png(&png)?;
            clipboard.set_image(width, height, rgba)?;
        }
        #[cfg(target_os = "windows")]
        {
            let (width, height, rgba) = output::decode_png(&png)?;
            output::set_windows_image(width, height, &rgba, &png)?;
        }
        anyhow::Ok(png)
    });
    cx.spawn(async move |cx| {
        let png = read.await?;
        #[cfg(target_os = "macos")]
        cx.update(|cx| {
            cx.write_to_clipboard(ClipboardItem::new_image(&Image::from_bytes(
                ImageFormat::Png,
                png,
            )));
        });
        #[cfg(not(target_os = "macos"))]
        let _ = (png, cx);
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snap_targets_are_display_local_clipped_and_skip_our_windows() {
        let display = Display {
            id: 1,
            name: "right".into(),
            bounds: Rect::new(1920, 0, 2560, 1440),
            scale: 1.0,
            primary: false,
            refresh_hz: None,
        };
        let frame = Frame {
            display_id: 1,
            width: 2560,
            height: 1440,
            data: Vec::new(),
        };
        let window = |pid, bounds| WindowInfo {
            id: 0,
            pid,
            app: String::new(),
            title: String::new(),
            bounds,
        };
        let windows = [
            window(1, Rect::new(2000, 100, 800, 600)),
            window(1, Rect::new(1800, 50, 300, 300)), // straddles both displays
            window(1, Rect::new(0, 0, 500, 500)),     // other display only
            window(std::process::id(), Rect::new(1920, 0, 2560, 1440)),
        ];
        let rects: Vec<Rect> = snap_targets(&display, &frame, &windows)
            .into_iter()
            .map(|t| t.rect)
            .collect();
        assert_eq!(
            rects,
            vec![Rect::new(80, 100, 800, 600), Rect::new(0, 50, 180, 300)]
        );
    }

    #[test]
    fn describe_names_windows_displays_and_regions() {
        let target = |rect, app: &str| SnapTarget {
            rect,
            app: app.into(),
            title: format!("{app} window"),
        };
        let targets = [
            target(Rect::new(100, 100, 200, 200), "Front"),
            target(Rect::new(0, 0, 800, 600), "Back"),
        ];
        let d = describe(Rect::new(100, 100, 200, 200), 1000, 800, &targets, 3);
        assert_eq!((d.source, d.app.as_str()), (Source::Window, "Front"));
        assert_eq!(d.window_title, "Front window");
        assert_eq!(d.display_id, Some(3));
        let d = describe(Rect::new(0, 0, 1000, 800), 1000, 800, &targets, 3);
        assert_eq!((d.source, d.app.as_str()), (Source::Display, ""));
        let d = describe(Rect::new(150, 150, 20, 20), 1000, 800, &targets, 3);
        assert_eq!((d.source, d.app.as_str()), (Source::Region, "Front"));
        let d = describe(Rect::new(500, 400, 50, 50), 1000, 800, &targets, 3);
        assert_eq!((d.source, d.app.as_str()), (Source::Region, "Back"));
        let d = describe(Rect::new(900, 700, 50, 50), 1000, 800, &targets, 3);
        assert_eq!((d.source, d.app.as_str()), (Source::Region, ""));
    }
}
