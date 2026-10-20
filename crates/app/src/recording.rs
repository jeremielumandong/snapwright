//! Screen recording: the record hotkey opens the selection overlay, the selection starts a
//! [`Recorder`] on that area, and a small bar beside it shows the time with Pause and Stop.
//! The same hotkey (or the tray, or the bar) stops it; the video lands in the recordings
//! folder and in the library, with its first frame as the thumbnail.
//!
//! The bar sits outside the recorded area when there is room. Where the OS keeps this app's
//! windows out of captures (macOS, and Windows 10 2004+) it may also sit inside; elsewhere it
//! is left out when it would cover the recording, and the hotkey or tray stops it.
//!
//! [`Prefs`] say how videos are recorded: frame rate, quality, and whether the system's sound
//! and the microphone go in. They are picked in the tray's Recording options, or with `A` and
//! `M` in the record overlay, and saved in `settings.toml`.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use gpui_kit::assets::IconName;
use gpui_kit::component::Icon;
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, Context, FontWeight, Global,
    InteractiveElement as _, IntoElement, ParentElement as _, Pixels, Point, Render,
    StatefulInteractiveElement as _, Styled as _, Window, WindowBackgroundAppearance, WindowBounds,
    WindowOptions, div, hsla, point, px, size,
};
use snapwright_capture::{Display, Rect};
use snapwright_library::{Metadata, NewCapture, NewRecording};
use snapwright_record::{Audio, DisplaySource, Format, Options, Quality, Recorder};

use crate::session::{self, LibraryService, Mode};
use crate::theme::hud;

const HUD_W: f32 = 196.;
const HUD_H: f32 = 44.;
/// Space between the recorded area and the bar, and the bar and the display edge.
const GAP: f32 = 12.;
const EDGE: f32 = 8.;

/// A video's frame rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FrameRate {
    /// The recorded display's refresh rate (60 if the OS does not say).
    #[default]
    Display,
    Fixed(u32),
}

impl FrameRate {
    /// The frame rate for recording `display`.
    pub fn for_display(self, display: &Display) -> u32 {
        match self {
            Self::Display => snapwright_record::display_fps(display),
            Self::Fixed(fps) => fps.clamp(1, snapwright_record::MAX_FPS),
        }
    }

    /// The value in `settings.toml`: `display` or a number.
    pub fn key(self) -> String {
        match self {
            Self::Display => "display".into(),
            Self::Fixed(fps) => fps.to_string(),
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        if key.eq_ignore_ascii_case("display") {
            return Some(Self::Display);
        }
        key.parse()
            .ok()
            .filter(|fps| (1..=snapwright_record::MAX_FPS).contains(fps))
            .map(Self::Fixed)
    }
}

/// How videos are recorded. GIFs keep their own frame rate and have no sound.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Prefs {
    pub fps: FrameRate,
    pub quality: Quality,
    pub audio: Audio,
}

impl Global for Prefs {}

/// The frame rates the tray offers.
pub const FPS_CHOICES: [FrameRate; 3] = [
    FrameRate::Display,
    FrameRate::Fixed(60),
    FrameRate::Fixed(30),
];

/// Sets the saved recording options at launch.
pub fn init(prefs: Prefs, cx: &mut App) {
    cx.set_global(prefs);
}

/// The recording options in use.
pub fn prefs(cx: &App) -> Prefs {
    cx.try_global::<Prefs>().copied().unwrap_or_default()
}

/// Applies a pick from the tray or the overlay, ticks the tray to match and saves it.
pub fn choose(choice: crate::tray::RecordingChoice, cx: &mut App) {
    use crate::tray::RecordingChoice;
    let mut p = prefs(cx);
    match choice {
        RecordingChoice::SystemAudio => p.audio.system = !p.audio.system,
        RecordingChoice::Microphone => p.audio.microphone = !p.audio.microphone,
        RecordingChoice::Fps(fps) => p.fps = fps,
        RecordingChoice::Quality(q) => p.quality = q,
    }
    cx.set_global(p);
    if let Some(tray) = cx.try_global::<crate::tray::Tray>() {
        tray.set_recording_prefs(p);
    }
    tracing::info!(?p, "recording options");
    cx.background_executor()
        .spawn(async move {
            if let Err(e) = crate::settings::update(|s| s.recording = p) {
                tracing::warn!(error = %e, "could not save the recording options");
            }
        })
        .detach();
}

/// Recorder options for recording `display` to `format` with these preferences.
fn options(format: Format, prefs: Prefs, display: &Display) -> Options {
    let mut options = Options::new(format);
    if format == Format::Mp4 {
        options.fps = prefs.fps.for_display(display);
        options.quality = prefs.quality;
        options.audio = prefs.audio;
    }
    options
}

/// Where recording stands.
#[derive(Default)]
enum Phase {
    #[default]
    Idle,
    /// The area is chosen and the recorder is starting.
    Starting,
    Recording(Live),
    /// The file is being finished.
    Stopping,
}

struct Live {
    recorder: Arc<Mutex<Option<Recorder>>>,
    hud: Option<AnyWindowHandle>,
    format: Format,
    meta: Metadata,
}

#[derive(Default)]
struct State(Phase);
impl Global for State {}

/// Whether a recording is running (or starting, or finishing).
pub fn is_busy(cx: &mut App) -> bool {
    !matches!(cx.default_global::<State>().0, Phase::Idle)
}

/// The record hotkey: starts choosing an area to record, or stops the recording running.
pub fn toggle(format: Format, pressed_at: Instant, cx: &mut App) {
    match cx.default_global::<State>().0 {
        Phase::Idle => session::start_mode(Mode::Record(format), pressed_at, cx),
        Phase::Recording(_) => stop(cx),
        Phase::Starting | Phase::Stopping => {}
    }
}

/// Starts recording `region` (frame pixels) of `display`. The overlays have just been
/// closed; the first frame waits until they have left the screen.
pub fn begin(display: Display, region: Rect, format: Format, meta: Metadata, cx: &mut App) {
    if is_busy(cx) {
        return;
    }
    cx.set_global(State(Phase::Starting));
    crate::toast::dismiss(cx);
    let capturer = session::capturer(cx);
    let options = options(format, prefs(cx), &display);
    let timer = cx.background_executor().timer(crate::toast::SETTLE);
    let source_display = display.clone();
    let start = cx.background_executor().spawn(async move {
        timer.await;
        let path = crate::output::recording_path(format.extension())?;
        let source = DisplaySource::new(capturer, source_display, Some(region));
        anyhow::Ok(Recorder::start(source, options, path)?)
    });
    cx.spawn(async move |cx| {
        let started = start.await;
        cx.update(|cx| match started {
            Ok(recorder) => {
                tracing::info!(path = %recorder.path().display(), ?region, "recording");
                let recorder = Arc::new(Mutex::new(Some(recorder)));
                let hud = open_hud(&display, region, recorder.clone(), cx);
                cx.set_global(State(Phase::Recording(Live {
                    recorder,
                    hud,
                    format,
                    meta,
                })));
                set_tray(true, cx);
            }
            Err(e) => {
                cx.set_global(State(Phase::Idle));
                tracing::error!(error = format_args!("{e:#}"), "recording could not start");
            }
        });
    })
    .detach();
}

/// Stops the recording, finishes the file off the UI thread, keeps it in the library and
/// shows the quick-access preview.
pub fn stop(cx: &mut App) {
    let Phase::Recording(live) =
        std::mem::replace(&mut cx.default_global::<State>().0, Phase::Stopping)
    else {
        return;
    };
    if let Some(hud) = live.hud {
        // Fails harmlessly when the bar's own Stop button got here (it closed itself).
        let _ = hud.update(cx, |_, window, _| window.remove_window());
    }
    set_tray(false, cx);
    let library = cx.global::<LibraryService>().0.clone();
    let Live {
        recorder,
        format,
        meta,
        ..
    } = live;
    let finish = cx.background_executor().spawn(async move {
        let recorder = recorder
            .lock()
            .map_err(|_| anyhow::anyhow!("recorder poisoned by an earlier panic"))?
            .take()
            .ok_or_else(|| anyhow::anyhow!("no recorder"))?;
        let summary = recorder.stop()?;
        if let Some(why) = &summary.interrupted {
            tracing::warn!(reason = %why, "recording stopped early");
        }
        for problem in &summary.audio_problems {
            tracing::warn!(problem = %problem, "recorded without this sound");
        }
        let ms = summary.duration.as_millis() as u64;
        tracing::info!(
            path = %summary.path.display(),
            seconds = summary.duration.as_secs_f64(),
            frames = summary.frames,
            dropped = summary.dropped,
            audio = summary.audio,
            ?format,
            "recording saved"
        );
        let poster = &summary.poster;
        let rgba = poster.to_rgba();
        let png = crate::output::encode_png(poster.width, poster.height, &rgba)?;
        let mut stored = None;
        if let Some(library) = library {
            let mut lib = library
                .lock()
                .map_err(|_| anyhow::anyhow!("library poisoned by an earlier panic"))?;
            let id = lib.add_recording(&NewRecording {
                poster: NewCapture {
                    png: &png,
                    rgba: &rgba,
                    width: poster.width,
                    height: poster.height,
                    meta,
                },
                video: &summary.path,
                duration_ms: ms,
            })?;
            let thumb = lib.thumb_path_of(&lib.get(id)?);
            stored = Some((id, thumb));
        }
        anyhow::Ok((stored, (summary.width, summary.height), summary.path, ms))
    });
    cx.spawn(async move |cx| {
        let done = finish.await;
        cx.update(|cx| {
            cx.set_global(State(Phase::Idle));
            match done {
                Ok((stored, dims, path, ms)) => {
                    crate::library_view::notify_changed(cx);
                    if let Some((id, thumb)) = stored {
                        crate::toast::show(id, thumb, dims, Some((path, ms)), cx);
                    }
                }
                Err(e) => tracing::error!(
                    error = format_args!("{e:#}"),
                    "finishing the recording failed"
                ),
            }
        });
    })
    .detach();
}

fn set_tray(recording: bool, cx: &mut App) {
    if let Some(tray) = cx.try_global::<crate::tray::Tray>() {
        tray.set_recording(recording);
    }
}

/// The bar's top-left corner in logical pixels, and whether it covers the recorded area.
/// `display` and `region` are logical rectangles: `(x, y, width, height)`.
fn hud_origin(display: [f32; 4], region: [f32; 4]) -> (Point<Pixels>, bool) {
    let [dx, dy, dw, dh] = display;
    let [rx, ry, rw, rh] = region;
    let x = (rx + rw / 2. - HUD_W / 2.).clamp(dx + EDGE, (dx + dw - HUD_W - EDGE).max(dx + EDGE));
    let below = ry + rh + GAP;
    if below + HUD_H <= dy + dh - EDGE {
        return (point(px(x), px(below)), false);
    }
    let above = ry - GAP - HUD_H;
    if above >= dy + EDGE {
        return (point(px(x), px(above)), false);
    }
    // No room outside: inside the bottom of the area.
    let inside = (ry + rh - HUD_H - 3. * GAP).max(dy + EDGE);
    (point(px(x), px(inside)), true)
}

/// Opens the bar beside the recorded area, unless it would end up in the recording.
fn open_hud(
    display: &Display,
    region: Rect,
    recorder: Arc<Mutex<Option<Recorder>>>,
    cx: &mut App,
) -> Option<AnyWindowHandle> {
    if snapwright_capture::wayland_session() {
        // Layer-shell surfaces are placed by anchors; there is no anchor beside an arbitrary
        // area yet. The compositor keybinding stops the recording.
        return None;
    }
    let s = display.scale.max(0.1);
    let b = display.bounds;
    let logical = [
        b.x as f32 / s,
        b.y as f32 / s,
        b.width as f32 / s,
        b.height as f32 / s,
    ];
    let area = [
        (b.x + region.x) as f32 / s,
        (b.y + region.y) as f32 / s,
        region.width as f32 / s,
        region.height as f32 / s,
    ];
    let (origin, covers) = hud_origin(logical, area);
    let can_hide = snapwright_capture::EXCLUDES_OWN_WINDOWS || cfg!(target_os = "windows");
    if covers && !can_hide {
        tracing::info!(
            "no room for the recording bar outside the area; stop with the hotkey or tray"
        );
        return None;
    }
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(session::popup_bounds(Bounds::new(
            origin,
            size(px(HUD_W), px(HUD_H)),
        )))),
        titlebar: None,
        focus: false,
        show: true,
        kind: session::popup_kind(session::Placement::Centered, false),
        is_movable: false,
        is_resizable: false,
        is_minimizable: false,
        window_background: WindowBackgroundAppearance::Transparent,
        app_id: Some("snapwright".into()),
        ..Default::default()
    };
    let mut hidden = false;
    let window = match cx.open_window(options, |window, cx| {
        hidden = crate::toast::hide_from_capture(window);
        cx.new(|_| Hud { recorder })
    }) {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(error = %e, "recording bar unavailable");
            return None;
        }
    };
    if covers && !hidden {
        tracing::info!("the recording bar could not be hidden from the capture; closing it");
        let _ = window.update(cx, |_, window, _| window.remove_window());
        return None;
    }
    // Ticks the clock.
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(250))
                .await;
            if window.update(cx, |_, _, cx| cx.notify()).is_err() {
                break;
            }
        }
    })
    .detach();
    Some(window.into())
}

/// The bar: a red dot, the time recorded, Pause or Resume, and Stop.
struct Hud {
    recorder: Arc<Mutex<Option<Recorder>>>,
}

impl Hud {
    fn status(&self) -> (u64, bool) {
        match self.recorder.lock() {
            Ok(guard) => guard.as_ref().map_or((0, false), |r| {
                (r.elapsed().as_millis() as u64, r.is_paused())
            }),
            Err(_) => (0, false),
        }
    }
}

impl Render for Hud {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (ms, paused) = self.status();
        let button = |id: &'static str| {
            div()
                .id(id)
                .size(px(30.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(8.))
                .cursor_pointer()
                .hover(|d| d.bg(hsla(0., 0., 1., 0.12)))
        };
        div()
            .size_full()
            .flex()
            .items_center()
            .gap(px(8.))
            .pl(px(14.))
            .pr(px(7.))
            .rounded(px(12.))
            .bg(hud::bg())
            .border_1()
            .border_color(hud::border())
            .child(div().size(px(10.)).rounded_full().bg(if paused {
                hud::fg3()
            } else {
                hud::record()
            }))
            .child(
                div()
                    .flex_1()
                    .text_size(px(13.))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(if paused { hud::fg3() } else { hud::fg() })
                    .child(crate::toast::clock(ms)),
            )
            .child(
                button("hud-pause")
                    .child(
                        Icon::new(if paused {
                            IconName::Play
                        } else {
                            IconName::Pause
                        })
                        .size(px(15.))
                        .text_color(hud::fg()),
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Ok(guard) = this.recorder.lock()
                            && let Some(r) = guard.as_ref()
                        {
                            if r.is_paused() {
                                r.resume();
                            } else {
                                r.pause();
                            }
                        }
                        cx.notify();
                    })),
            )
            .child(
                button("hud-stop")
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .child(div().size(px(11.)).rounded(px(2.)).bg(hud::record())),
                    )
                    .on_click(cx.listener(|_, _, window, cx| {
                        window.remove_window();
                        stop(cx);
                    })),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(p: Point<Pixels>) -> (f32, f32) {
        (f32::from(p.x), f32::from(p.y))
    }

    #[test]
    fn bar_goes_below_then_above_then_inside() {
        let display = [0., 0., 1440., 900.];
        let (p, covers) = hud_origin(display, [100., 100., 400., 300.]);
        assert_eq!((at(p), covers), ((202., 412.), false));
        let (p, covers) = hud_origin(display, [100., 500., 400., 390.]);
        assert_eq!((at(p), covers), ((202., 444.), false));
        let (p, covers) = hud_origin(display, [0., 0., 1440., 900.]);
        assert_eq!((at(p), covers), ((622., 820.), true));
    }

    #[test]
    fn bar_stays_on_the_display() {
        // A second display to the right; an area at its right edge.
        let display = [1440., 0., 1920., 1080.];
        let (p, _) = hud_origin(display, [3300., 100., 60., 60.]);
        assert_eq!(at(p).0, 1440. + 1920. - HUD_W - EDGE);
    }
}
