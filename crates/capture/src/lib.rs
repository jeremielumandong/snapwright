//! `snapwright-capture` — grab the screen as fast as each OS allows.
//!
//! One API over a native backend per platform:
//!
//! * macOS 14+: ScreenCaptureKit (`SCScreenshotManager`).
//! * Windows 10+: DXGI Desktop Duplication.
//! * Linux X11: MIT-SHM `GetImage`, falling back to a plain `GetImage`.
//! * Linux Wayland: `wlr-screencopy` (wlroots compositors such as Hyprland and Sway). GNOME
//!   and KDE need the portal backend ([`CaptureError::Unsupported`] until then).
//!
//! Every backend returns [`Frame`]s in BGRA, the layout macOS and Windows capture in and the
//! one GPUI's `RenderImage` takes, so a frame reaches the overlay without a conversion.

mod cursor;
mod frame;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod macos;
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
mod wayland;
#[cfg(target_os = "windows")]
#[allow(unsafe_code)]
mod windows;
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
mod x11;

pub use frame::{Frame, Rect};

/// Whether captures leave out this process's own windows by themselves. On macOS the
/// ScreenCaptureKit filter excludes this application, so a preview or toolbar can stay on
/// screen during a capture. Elsewhere such windows must be closed first, or on Windows kept
/// out with [`exclude_window`].
pub const EXCLUDES_OWN_WINDOWS: bool = cfg!(target_os = "macos");

/// Keeps a window out of every screen capture, this app's and others' (Windows 10 2004 and
/// later: `WDA_EXCLUDEFROMCAPTURE`). `hwnd` is the window's handle. Returns whether it took.
#[cfg(target_os = "windows")]
pub fn exclude_window(hwnd: isize) -> bool {
    windows::exclude_window(hwnd)
}

/// Whether this is a Wayland session, where capture goes through the Wayland backend and
/// pop-ups must be layer-shell surfaces. Always false off Linux.
pub fn wayland_session() -> bool {
    cfg!(target_os = "linux")
        && (std::env::var("XDG_SESSION_TYPE").is_ok_and(|t| t == "wayland")
            || std::env::var_os("WAYLAND_DISPLAY").is_some_and(|d| !d.is_empty()))
}

/// The frontmost window containing `(x, y)` (physical pixels), from a front-to-back list.
pub fn window_at(windows: &[WindowInfo], x: i32, y: i32) -> Option<&WindowInfo> {
    windows.iter().find(|w| w.bounds.contains(x, y))
}

#[cfg(target_os = "linux")]
use linux::Backend;
#[cfg(target_os = "macos")]
use macos::Backend;
#[cfg(target_os = "windows")]
use windows::Backend;

/// Errors from capture backends.
#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    /// This platform or session type has no backend yet.
    #[error("screen capture is not supported here: {0}")]
    Unsupported(String),
    /// The OS refused capture, usually a missing Screen Recording permission.
    #[error("screen capture permission denied: {0}")]
    PermissionDenied(String),
    /// No display with that id is connected.
    #[error("no display with id {0}")]
    NoSuchDisplay(u64),
    /// The OS capture API failed.
    #[error("{api} failed: {message}")]
    Platform {
        /// The OS call that failed.
        api: &'static str,
        /// What it reported.
        message: String,
    },
}

/// Shorthand for capture results.
pub type Result<T, E = CaptureError> = std::result::Result<T, E>;

/// A connected display, in physical pixels on the global desktop.
#[derive(Debug, Clone, PartialEq)]
pub struct Display {
    /// Stable id for this session (CGDirectDisplayID, RandR output, or DXGI output index).
    pub id: u64,
    /// Human-readable name, such as the connector or monitor name.
    pub name: String,
    /// Bounds in physical pixels.
    pub bounds: Rect,
    /// Physical pixels per logical point (2.0 on a Retina display).
    pub scale: f32,
    /// Whether this is the primary display.
    pub primary: bool,
    /// Refresh rate in whole hertz (60, 120, 144), when the OS reports one.
    pub refresh_hz: Option<u32>,
}

/// A refresh rate in hertz as whole hertz, or `None` for nonsense (0, or "default").
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]
pub(crate) fn whole_hz(hz: f64) -> Option<u32> {
    (hz.is_finite() && hz >= 2.0).then(|| hz.round() as u32)
}

/// A top-level window on screen, for snapping a selection to it.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowInfo {
    /// Platform window id (CGWindowID, X11 window, or HWND).
    pub id: u64,
    /// Owning process.
    pub pid: u32,
    /// Owning application, when the platform reports one.
    pub app: String,
    /// Window title; may be empty.
    pub title: String,
    /// Visible frame in physical pixels on the global desktop, like [`Display::bounds`].
    /// Excludes invisible borders and drop shadows.
    pub bounds: Rect,
}

/// A live connection to the platform's capture API.
///
/// Create one and keep it: backends hold devices, connections and shared memory that are
/// expensive to set up, so repeat captures are much faster than the first.
pub struct Capturer {
    backend: Backend,
    cursor: bool,
}

// Captures run off the UI thread, so the capturer must move between threads.
const _: () = {
    const fn assert_send<T: Send>() {}
    assert_send::<Capturer>();
};

impl Capturer {
    /// Connects to the platform capture API.
    pub fn new() -> Result<Self> {
        Ok(Self {
            backend: Backend::new()?,
            cursor: false,
        })
    }

    /// Whether captures include the mouse pointer. Off by default (screenshots); recordings
    /// turn it on.
    pub fn set_cursor(&mut self, cursor: bool) {
        self.cursor = cursor;
    }

    /// Whether captures include the mouse pointer.
    pub fn cursor(&self) -> bool {
        self.cursor
    }

    /// Lists the connected displays.
    pub fn displays(&mut self) -> Result<Vec<Display>> {
        self.backend.displays()
    }

    /// Captures one display at full resolution.
    pub fn capture_display(&mut self, display: &Display) -> Result<Frame> {
        self.backend.capture(display, self.cursor)
    }

    /// Lists visible top-level windows, front to back. Call it before showing any window of
    /// your own, or filter those out by [`WindowInfo::pid`].
    pub fn windows(&mut self) -> Result<Vec<WindowInfo>> {
        self.backend.windows()
    }

    /// Captures every display, in the order [`Capturer::displays`] returns them.
    pub fn capture_all(&mut self) -> Result<Vec<Frame>> {
        let displays = self.displays()?;
        displays.iter().map(|d| self.capture_display(d)).collect()
    }
}
