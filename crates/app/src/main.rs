//! Snapwright desktop app: a resident process that shows the selection overlay on a global
//! hotkey and puts the selected region on the clipboard.

// Release builds on Windows are GUI-subsystem binaries: no console window behind the app.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod assets;
mod countdown;
mod editor_view;
mod hotkey;
#[cfg(target_os = "linux")]
mod hyprland;
#[cfg(target_os = "linux")]
mod instance;
mod library_view;
mod ocr;
mod omarchy;
mod output;
mod overlay;
mod recording;
mod selection;
mod session;
mod settings;
mod shortcuts;
mod shortcuts_view;
mod theme;
mod toast;
mod tray;
mod update;
mod updates;

use std::time::Instant;

use anyhow::Result;
use futures::StreamExt as _;
use snapwright_record::Format;
use tracing_subscriber::EnvFilter;

#[cfg(target_os = "linux")]
use instance::Request;

/// What a launch asks for (on Linux, also what a later launch hands to the running app).
#[cfg(not(target_os = "linux"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Request {
    Capture,
    RepeatLast,
    CaptureIn(u32),
    Library,
    Shortcuts,
    EditLatest,
    Record,
    RecordGif,
}

/// `--capture-now` starts a capture as soon as the app is up, or in the running app (what
/// the Hyprland binds run; bind it to a key in other Wayland compositors).
const CAPTURE_NOW_ARG: &str = "--capture-now";
/// `--repeat-last` captures the last region again.
const REPEAT_LAST_ARG: &str = "--repeat-last";
/// `--library` opens the library window at launch.
const LIBRARY_ARG: &str = "--library";
/// `--shortcuts` opens the Shortcuts window at launch.
const SHORTCUTS_ARG: &str = "--shortcuts";
/// `--edit-latest` opens the newest capture in the editor (for tests).
const EDIT_LATEST_ARG: &str = "--edit-latest";
/// `--capture-in=N` starts a capture after an N-second countdown.
const CAPTURE_IN_ARG: &str = "--capture-in=";
/// `--record` starts recording the screen to MP4 (or stops a recording in progress).
const RECORD_ARG: &str = "--record";
/// `--record-gif` does the same for a GIF.
const RECORD_GIF_ARG: &str = "--record-gif";
/// Delay for the tray's "Capture in 3 seconds".
const TRAY_DELAY_SECS: u32 = 3;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("SNAPWRIGHT_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let requests = requests(std::env::args());
    #[cfg(target_os = "linux")]
    if instance::forward(&requests) {
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    let later_launches = match instance::listen() {
        Ok(rx) => Some(rx),
        Err(e) => {
            tracing::warn!(error = %e, "single-instance socket unavailable");
            None
        }
    };
    // Read before the UI thread starts, so the first window opens in the right colors.
    let omarchy = omarchy::load();
    let saved = settings::load();
    let theme = saved.theme.unwrap_or(if omarchy.is_some() {
        theme::Choice::Omarchy
    } else {
        theme::Choice::System
    });

    new_application()
        .with_assets(assets::AppAssets)
        .run(move |cx| {
            gpui_kit::init(cx);
            let has_omarchy = omarchy.is_some();
            theme::init(theme, omarchy, cx);
            if let Err(e) = session::init(cx) {
                tracing::error!(error = %e, "screen capture is unavailable");
                cx.quit();
                return;
            }
            ocr::init(cx);
            #[cfg(target_os = "linux")]
            if let Some(mut launches) = later_launches {
                cx.spawn(async move |cx| {
                    while let Some((request, at)) = launches.next().await {
                        cx.update(|cx| handle(request, at, cx));
                    }
                })
                .detach();
            }
            shortcuts::init(saved.shortcuts, cx);
            let labels = cx.global::<tray::Labels>().clone();
            recording::init(saved.recording, cx);
            match tray::create(&labels, theme, has_omarchy, saved.recording) {
                Ok((tray, mut actions)) => {
                    cx.set_global(tray);
                    cx.spawn(async move |cx| {
                        while let Some(action) = actions.next().await {
                            cx.update(|cx| on_tray(action, cx));
                        }
                    })
                    .detach();
                }
                Err(e) => tracing::warn!(error = %e, "tray icon unavailable"),
            }
            updates::init(cx);
            let now = Instant::now();
            for request in &requests {
                handle(*request, now, cx);
            }
        });
    // "Restart to update" quit the app; start the new version or its installer now.
    updates::run_after_exit();
    Ok(())
}

/// The GPUI application. On macOS AccessKit is off: its adapter subclasses each window's
/// content view at runtime and restores the class when the window goes, which breaks the
/// Touch Bar's key-value observers, and AppKit then aborts the app on Touch Bar Macs
/// (zed-industries/zed#64819, fixed upstream in #65186 but not yet in gpui-fast).
fn new_application() -> gpui_kit::Application {
    #[cfg(target_os = "macos")]
    {
        gpui_kit::Application::new_inaccessible(gpui_kit::platform::current_platform(false))
    }
    #[cfg(not(target_os = "macos"))]
    {
        gpui_kit::application()
    }
}

/// A press of `action`'s shortcut (a global hotkey, or a Hyprland bind's launch).
fn on_hotkey(action: hotkey::Action, at: Instant, cx: &mut gpui_kit::App) {
    match action {
        hotkey::Action::Capture => session::start(at, cx),
        hotkey::Action::RepeatLast => session::repeat_last(at, cx),
        hotkey::Action::Record => recording::toggle(Format::Mp4, at, cx),
        hotkey::Action::RecordGif => recording::toggle(Format::Gif, at, cx),
    }
}

/// What the command line asks for, in the order the app does it.
fn requests(args: impl Iterator<Item = String>) -> Vec<Request> {
    let args: Vec<String> = args.collect();
    let has = |flag: &str| args.iter().any(|a| a == flag);
    let mut out = Vec::new();
    if has(CAPTURE_NOW_ARG) {
        out.push(Request::Capture);
    }
    if has(REPEAT_LAST_ARG) {
        out.push(Request::RepeatLast);
    }
    if has(LIBRARY_ARG) {
        out.push(Request::Library);
    }
    if has(SHORTCUTS_ARG) {
        out.push(Request::Shortcuts);
    }
    if let Some(n) = args.iter().find_map(|a| {
        a.strip_prefix(CAPTURE_IN_ARG)
            .and_then(|n| n.parse::<u32>().ok())
    }) {
        out.push(Request::CaptureIn(n));
    }
    if has(EDIT_LATEST_ARG) {
        out.push(Request::EditLatest);
    }
    if has(RECORD_ARG) {
        out.push(Request::Record);
    }
    if has(RECORD_GIF_ARG) {
        out.push(Request::RecordGif);
    }
    out
}

fn handle(request: Request, at: Instant, cx: &mut gpui_kit::App) {
    match request {
        Request::Capture => session::start(at, cx),
        Request::RepeatLast => session::repeat_last(at, cx),
        Request::CaptureIn(seconds) => countdown::start(seconds, cx),
        Request::Library => library_view::open(cx),
        Request::Shortcuts => shortcuts_view::open(cx),
        Request::EditLatest => library_view::edit_latest(cx),
        Request::Record => recording::toggle(Format::Mp4, at, cx),
        Request::RecordGif => recording::toggle(Format::Gif, at, cx),
    }
}

fn on_tray(action: tray::Action, cx: &mut gpui_kit::App) {
    match action {
        tray::Action::Capture => session::start(Instant::now(), cx),
        tray::Action::CaptureDelayed => countdown::start(TRAY_DELAY_SECS, cx),
        tray::Action::RepeatLast => session::repeat_last(Instant::now(), cx),
        tray::Action::Record => recording::toggle(Format::Mp4, Instant::now(), cx),
        tray::Action::RecordGif => recording::toggle(Format::Gif, Instant::now(), cx),
        tray::Action::Recording(choice) => recording::choose(choice, cx),
        tray::Action::OpenLibrary => library_view::open(cx),
        tray::Action::Shortcuts => shortcuts_view::open(cx),
        tray::Action::OpenFolder => match output::captures_dir() {
            Ok(dir) => {
                if let Err(e) = std::fs::create_dir_all(&dir) {
                    tracing::warn!(error = %e, "could not create the captures folder");
                }
                cx.open_with_system(&dir);
            }
            Err(e) => tracing::warn!(error = %e, "no captures folder"),
        },
        tray::Action::Theme(choice) => {
            theme::choose(choice, cx);
            if let Some(tray) = cx.try_global::<tray::Tray>() {
                tray.set_theme(choice);
            }
        }
        tray::Action::Update => updates::on_tray(cx),
        tray::Action::Quit => cx.quit(),
    }
}
