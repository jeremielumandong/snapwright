//! macOS backend: ScreenCaptureKit's `SCScreenshotManager` (macOS 14+).
//!
//! `CGDisplayCreateImage` is obsolete since macOS 15, so ScreenCaptureKit is the only path.
//! Its calls complete on a background queue; this backend waits on a channel. The shareable
//! content (displays and their filters) is cached and refreshed when a display is not found.

use std::ptr::NonNull;
use std::sync::mpsc;
use std::time::Duration;

use block2::RcBlock;
use objc2::AllocAnyThread;
use objc2::rc::Retained;
use objc2_core_foundation::CFRetained;
use objc2_core_graphics::{
    CGDataProvider, CGDisplayCopyDisplayMode, CGDisplayMode, CGImage, CGMainDisplayID,
};
use objc2_foundation::{NSArray, NSError};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCRunningApplication, SCScreenshotManager, SCShareableContent,
    SCStreamConfiguration,
};

use crate::{CaptureError, Display, Frame, Rect, Result, WindowInfo};

/// `kCVPixelFormatType_32BGRA` ('BGRA').
const PIXEL_FORMAT_BGRA: u32 = 0x4247_5241;
/// `SCStreamErrorUserDeclined`: Screen Recording permission was not granted.
const USER_DECLINED: isize = -3801;
const TIMEOUT: Duration = Duration::from_secs(5);

/// Carries a ScreenCaptureKit or CoreGraphics object from the completion queue to the
/// waiting thread.
struct Handoff<T>(T);
// SAFETY: the wrapped objects (SCShareableContent, CGImage) are immutable once delivered and
// are only touched by the receiving thread after the send.
unsafe impl<T> Send for Handoff<T> {}

fn ns_error(api: &'static str, error: *mut NSError) -> CaptureError {
    // SAFETY: ScreenCaptureKit passes either null or a valid NSError for the call's duration.
    let Some(error) = (unsafe { error.as_ref() }) else {
        return CaptureError::Platform {
            api,
            message: "failed without an error".into(),
        };
    };
    let message = error.localizedDescription().to_string();
    if error.code() == USER_DECLINED {
        CaptureError::PermissionDenied(format!(
            "{message} (allow Snapwright in System Settings > Privacy & Security > Screen Recording)"
        ))
    } else {
        CaptureError::Platform { api, message }
    }
}

fn timeout(api: &'static str) -> CaptureError {
    CaptureError::Platform {
        api,
        message: "timed out waiting for ScreenCaptureKit".into(),
    }
}

struct Target {
    display: Display,
    filter: Retained<SCContentFilter>,
    pixel_width: usize,
    pixel_height: usize,
}

// SAFETY: ScreenCaptureKit filters and content snapshots are immutable after creation and
// owned by this backend; `Capturer` takes `&mut self` for every call, so they are never shared
// between threads.
unsafe impl Send for Backend {}

pub(crate) struct Backend {
    /// The last shareable-content snapshot: displays and windows.
    content: Retained<SCShareableContent>,
    targets: Vec<Target>,
}

impl Backend {
    pub(crate) fn new() -> Result<Self> {
        let content = load_content()?;
        Ok(Self {
            targets: targets(&content),
            content,
        })
    }

    fn refresh(&mut self) -> Result<()> {
        self.content = load_content()?;
        self.targets = targets(&self.content);
        Ok(())
    }

    /// Windows from the snapshot taken by the last `displays` call, so a session that lists
    /// displays, captures, then lists windows pays for one ScreenCaptureKit query.
    pub(crate) fn windows(&mut self) -> Result<Vec<WindowInfo>> {
        // SAFETY: plain getters on a delivered, immutable snapshot.
        let windows = unsafe { self.content.windows() };
        let mut out = Vec::new();
        for w in windows.iter() {
            // SAFETY: as above.
            let (id, frame, layer, on_screen, title, owner) = unsafe {
                (
                    w.windowID(),
                    w.frame(),
                    w.windowLayer(),
                    w.isOnScreen(),
                    w.title(),
                    w.owningApplication(),
                )
            };
            // Layer 0 holds normal app windows; menus, the Dock and overlays sit above it.
            if !on_screen || layer != 0 || frame.size.width < 2.0 || frame.size.height < 2.0 {
                continue;
            }
            // Points to pixels with the scale of the display holding the window's centre, the
            // same rule display bounds use.
            let cx = frame.origin.x + frame.size.width / 2.0;
            let cy = frame.origin.y + frame.size.height / 2.0;
            let scale = self
                .targets
                .iter()
                .find(|t| {
                    let b = &t.display.bounds;
                    let s = f64::from(t.display.scale);
                    cx >= f64::from(b.x) / s
                        && cy >= f64::from(b.y) / s
                        && cx < (f64::from(b.x) + f64::from(b.width)) / s
                        && cy < (f64::from(b.y) + f64::from(b.height)) / s
                })
                .map_or(1.0, |t| f64::from(t.display.scale));
            let x0 = (frame.origin.x * scale).round();
            let y0 = (frame.origin.y * scale).round();
            let x1 = ((frame.origin.x + frame.size.width) * scale).round();
            let y1 = ((frame.origin.y + frame.size.height) * scale).round();
            // SAFETY: plain getters on the owning application.
            let (pid, app) = owner.map_or((0, String::new()), |o| unsafe {
                (o.processID().max(0) as u32, o.applicationName().to_string())
            });
            out.push(WindowInfo {
                id: u64::from(id),
                pid,
                app,
                title: title.map(|t| t.to_string()).unwrap_or_default(),
                bounds: Rect::new(x0 as i32, y0 as i32, (x1 - x0) as u32, (y1 - y0) as u32),
            });
        }
        Ok(out)
    }

    pub(crate) fn displays(&mut self) -> Result<Vec<Display>> {
        self.refresh()?;
        Ok(self.targets.iter().map(|t| t.display.clone()).collect())
    }

    pub(crate) fn capture(&mut self, display: &Display, cursor: bool) -> Result<Frame> {
        if !self.targets.iter().any(|t| t.display.id == display.id) {
            self.refresh()?;
        }
        let Some(target) = self.targets.iter().find(|t| t.display.id == display.id) else {
            return Err(CaptureError::NoSuchDisplay(display.id));
        };
        let image = screenshot(target, cursor)?;
        let image = Some(&*image);
        let width = CGImage::width(image) as u32;
        let height = CGImage::height(image) as u32;
        let stride = CGImage::bytes_per_row(image);
        let data = CGImage::data_provider(image)
            .and_then(|p| CGDataProvider::data(Some(&p)))
            .ok_or_else(|| CaptureError::Platform {
                api: "CGDataProviderCopyData",
                message: "image has no pixel data".into(),
            })?;
        // SAFETY: the CFData is immutable and outlives this borrow.
        let bytes = unsafe { data.as_bytes_unchecked() };
        if bytes.len() < stride * height as usize || stride < width as usize * 4 {
            return Err(CaptureError::Platform {
                api: "SCScreenshotManager",
                message: format!(
                    "unexpected image layout {width}x{height}, {stride} bytes per row"
                ),
            });
        }
        Ok(Frame::from_bgrx_rows(
            display.id, width, height, stride, bytes,
        ))
    }
}

fn load_content() -> Result<Retained<SCShareableContent>> {
    let (tx, rx) = mpsc::sync_channel(1);
    let block = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            // SAFETY: ScreenCaptureKit passes a valid object or null; `retain` keeps it alive.
            let result = match unsafe { Retained::retain(content) } {
                Some(content) => Ok(Handoff(content)),
                None => Err(ns_error("SCShareableContent", error)),
            };
            let _ = tx.send(result);
        },
    );
    // SAFETY: the block is retained by ScreenCaptureKit until it runs.
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&block) };
    let content = rx
        .recv_timeout(TIMEOUT)
        .map_err(|_| timeout("SCShareableContent"))??
        .0;
    Ok(content)
}

fn targets(content: &SCShareableContent) -> Vec<Target> {
    let main = CGMainDisplayID();
    // This process's own windows (a capture preview, a toolbar) never belong in a capture.
    let own = std::process::id() as libc::pid_t;
    // SAFETY: plain getters on a delivered object.
    let mine: Vec<Retained<SCRunningApplication>> = unsafe { content.applications() }
        .iter()
        // SAFETY: plain getter.
        .filter(|a| unsafe { a.processID() } == own)
        .collect();
    let mine = NSArray::from_retained_slice(&mine);
    // SAFETY: plain getters on a delivered object.
    let displays = unsafe { content.displays() };
    displays.iter().map(|d| target(&d, &mine, main)).collect()
}

fn target(display: &SCDisplay, exclude: &NSArray<SCRunningApplication>, main: u32) -> Target {
    let excepting = NSArray::new();
    // SAFETY: a display and applications from the current shareable content.
    let filter = unsafe {
        SCContentFilter::initWithDisplay_excludingApplications_exceptingWindows(
            SCContentFilter::alloc(),
            display,
            exclude,
            &excepting,
        )
    };
    // SAFETY: plain getters.
    let (id, frame, scale) = unsafe {
        (
            display.displayID(),
            display.frame(),
            filter.pointPixelScale(),
        )
    };
    let scale = if scale > 0.0 { scale } else { 1.0 };
    let pixel_width = (frame.size.width * f64::from(scale)).round() as usize;
    let pixel_height = (frame.size.height * f64::from(scale)).round() as usize;
    Target {
        display: Display {
            id: u64::from(id),
            name: format!("Display {id}"),
            // Global coordinates are in points on macOS; scaled here so every backend reports
            // physical pixels. Mixed-scale layouts keep each display's own origin scale.
            bounds: Rect::new(
                (frame.origin.x * f64::from(scale)).round() as i32,
                (frame.origin.y * f64::from(scale)).round() as i32,
                pixel_width as u32,
                pixel_height as u32,
            ),
            scale,
            primary: id == main,
            refresh_hz: CGDisplayCopyDisplayMode(id)
                .and_then(|mode| crate::whole_hz(CGDisplayMode::refresh_rate(Some(&mode)))),
        },
        filter,
        pixel_width,
        pixel_height,
    }
}

fn screenshot(target: &Target, cursor: bool) -> Result<CFRetained<CGImage>> {
    // SAFETY: a fresh configuration object, set before use.
    let config = unsafe {
        let config = SCStreamConfiguration::new();
        config.setWidth(target.pixel_width);
        config.setHeight(target.pixel_height);
        config.setPixelFormat(PIXEL_FORMAT_BGRA);
        config.setShowsCursor(cursor);
        config
    };
    let (tx, rx) = mpsc::sync_channel(1);
    let block = RcBlock::new(move |image: *mut CGImage, error: *mut NSError| {
        let result = match NonNull::new(image) {
            // SAFETY: a non-null CGImage valid for the callback; retaining keeps it alive.
            Some(image) => Ok(Handoff(unsafe { CFRetained::retain(image) })),
            None => Err(ns_error("SCScreenshotManager", error)),
        };
        let _ = tx.send(result);
    });
    // SAFETY: filter and configuration are valid; the block is retained until it runs.
    unsafe {
        SCScreenshotManager::captureImageWithFilter_configuration_completionHandler(
            &target.filter,
            &config,
            Some(&block),
        );
    }
    Ok(rx
        .recv_timeout(TIMEOUT)
        .map_err(|_| timeout("SCScreenshotManager"))??
        .0)
}
