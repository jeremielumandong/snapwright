//! The tray (menu bar) icon: Capture, Record, recording options (sound, frame rate,
//! quality), Open captures folder, Theme, Keyboard shortcuts, updates, Quit.
//!
//! macOS and Windows use the native status item; Linux uses StatusNotifierItem over D-Bus
//! (`tray-icon`'s `ksni` backend), so no GTK is needed. Menu clicks arrive on a platform
//! thread and are forwarded to the UI thread over a channel.

use std::cell::{Cell, RefCell};

use anyhow::Result;
use futures::channel::mpsc;
use snapwright_record::Quality;
use tray_icon::menu::{
    CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu,
};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::hotkey;
use crate::omarchy;
use crate::recording::{self, FrameRate, Prefs};
use crate::theme::Choice;

/// What the user picked from the tray menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Capture,
    CaptureDelayed,
    RepeatLast,
    Record,
    RecordGif,
    Recording(RecordingChoice),
    OpenLibrary,
    OpenFolder,
    Shortcuts,
    Theme(Choice),
    Update,
    Quit,
}

/// A pick from the Recording submenu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingChoice {
    /// Toggles recording what the computer plays.
    SystemAudio,
    /// Toggles recording the microphone.
    Microphone,
    Fps(FrameRate),
    Quality(Quality),
}

/// The Recording submenu's items, ticked to match [`Prefs`].
struct RecordingItems {
    system: CheckMenuItem,
    microphone: CheckMenuItem,
    fps: Vec<(FrameRate, CheckMenuItem)>,
    quality: Vec<(Quality, CheckMenuItem)>,
}

/// Keeps the tray icon alive for the life of the app.
pub struct Tray {
    _icon: TrayIcon,
    capture: MenuItem,
    repeat: MenuItem,
    record: MenuItem,
    record_gif: MenuItem,
    themes: Vec<(Choice, CheckMenuItem)>,
    recording_items: RecordingItems,
    update: MenuItem,
    labels: RefCell<Labels>,
    recording: Cell<bool>,
}

impl Tray {
    /// Switches the record items between starting and stopping a recording.
    pub fn set_recording(&self, recording: bool) {
        self.recording.set(recording);
        let labels = self.labels.borrow();
        let text = if recording {
            "Stop recording"
        } else {
            "Record screen"
        };
        self.record
            .set_text(with_shortcut(text, labels.record.as_ref()));
        self.record_gif.set_enabled(!recording);
    }

    /// Shows new shortcuts beside the items (they were changed, or Hyprland bound them).
    pub fn set_labels(&self, labels: &Labels) {
        self.capture
            .set_text(with_shortcut("Capture region", labels.capture.as_ref()));
        self.repeat
            .set_text(with_shortcut("Repeat last region", labels.repeat.as_ref()));
        self.record_gif
            .set_text(with_shortcut("Record GIF", labels.record_gif.as_ref()));
        *self.labels.borrow_mut() = labels.clone();
        self.set_recording(self.recording.get());
    }

    /// Shows what the updater is doing (`crate::updates`).
    pub fn set_update(&self, text: &str, enabled: bool) {
        self.update.set_text(text);
        self.update.set_enabled(enabled);
    }
}

impl Tray {
    /// Ticks the theme in use (a click toggles the item itself, so set every one).
    pub fn set_theme(&self, choice: Choice) {
        for (c, item) in &self.themes {
            item.set_checked(*c == choice);
        }
    }

    /// Ticks the recording options in use.
    pub fn set_recording_prefs(&self, prefs: Prefs) {
        let items = &self.recording_items;
        items.system.set_checked(prefs.audio.system);
        items.microphone.set_checked(prefs.audio.microphone);
        for (fps, item) in &items.fps {
            item.set_checked(*fps == prefs.fps);
        }
        for (q, item) in &items.quality {
            item.set_checked(*q == prefs.quality);
        }
    }
}

impl gpui_kit::Global for Tray {}

const ID_CAPTURE: &str = "capture";
const ID_DELAYED: &str = "capture-delayed";
const ID_REPEAT: &str = "repeat-last";
const ID_RECORD: &str = "record";
const ID_RECORD_GIF: &str = "record-gif";
const ID_SYSTEM_AUDIO: &str = "record-system-audio";
const ID_MICROPHONE: &str = "record-microphone";
const ID_FPS_PREFIX: &str = "record-fps-";
const ID_QUALITY_PREFIX: &str = "record-quality-";
const ID_OPEN: &str = "open-folder";
const ID_LIBRARY: &str = "library";
const ID_SHORTCUTS: &str = "shortcuts";
const ID_UPDATE: &str = "update";
const ID_QUIT: &str = "quit";
const ID_THEME_PREFIX: &str = "theme-";

/// Shortcut labels shown beside menu items.
#[derive(Debug, Clone, Default)]
pub struct Labels {
    pub capture: Option<String>,
    pub repeat: Option<String>,
    pub record: Option<String>,
    pub record_gif: Option<String>,
}

/// The shortcuts that work in this session, for the library's hint too.
impl gpui_kit::Global for Labels {}

impl Labels {
    pub fn get(&self, action: hotkey::Action) -> Option<&String> {
        match action {
            hotkey::Action::Capture => self.capture.as_ref(),
            hotkey::Action::RepeatLast => self.repeat.as_ref(),
            hotkey::Action::Record => self.record.as_ref(),
            hotkey::Action::RecordGif => self.record_gif.as_ref(),
        }
    }

    pub fn from_actions(labels: &[(hotkey::Action, String)]) -> Self {
        let find = |action| {
            labels
                .iter()
                .find(|(a, _)| *a == action)
                .map(|(_, l)| l.clone())
        };
        Self {
            capture: find(hotkey::Action::Capture),
            repeat: find(hotkey::Action::RepeatLast),
            record: find(hotkey::Action::Record),
            record_gif: find(hotkey::Action::RecordGif),
        }
    }
}

fn with_shortcut(text: &str, shortcut: Option<&String>) -> String {
    match shortcut {
        Some(k) => format!("{text}\t{k}"),
        None => text.to_owned(),
    }
}

/// Creates the tray icon. Call on the main thread once the app is running. `omarchy` adds
/// "Follow Omarchy" to the Theme menu.
pub fn create(
    labels: &Labels,
    theme: Choice,
    omarchy: bool,
    prefs: Prefs,
) -> Result<(Tray, mpsc::UnboundedReceiver<Action>)> {
    let menu = Menu::new();
    let themes: Vec<(Choice, CheckMenuItem)> = Choice::all()
        .filter(|c| omarchy || *c != Choice::Omarchy || theme == Choice::Omarchy)
        .map(|c| {
            let id = format!("{ID_THEME_PREFIX}{}", c.key());
            (
                c,
                CheckMenuItem::with_id(id, c.label(), true, c == theme, None),
            )
        })
        .collect();
    let theme_menu = Submenu::with_id("theme", "Theme", true);
    for (c, item) in &themes {
        // The bundled themes under a rule, after System, Light, Dark and Follow Omarchy.
        if matches!(c, Choice::Named(id) if *id == omarchy::BUNDLED[0].id) {
            theme_menu.append(&PredefinedMenuItem::separator())?;
        }
        theme_menu.append(item)?;
    }
    let record = MenuItem::with_id(
        ID_RECORD,
        with_shortcut("Record screen", labels.record.as_ref()),
        true,
        None,
    );
    let record_gif = MenuItem::with_id(
        ID_RECORD_GIF,
        with_shortcut("Record GIF", labels.record_gif.as_ref()),
        true,
        None,
    );
    let recording_items = RecordingItems {
        system: CheckMenuItem::with_id(
            ID_SYSTEM_AUDIO,
            "Record system audio",
            true,
            prefs.audio.system,
            None,
        ),
        microphone: CheckMenuItem::with_id(
            ID_MICROPHONE,
            "Record microphone",
            true,
            prefs.audio.microphone,
            None,
        ),
        fps: recording::FPS_CHOICES
            .into_iter()
            .map(|fps| {
                let id = format!("{ID_FPS_PREFIX}{}", fps.key());
                let text = match fps {
                    FrameRate::Display => "Display refresh rate".to_owned(),
                    FrameRate::Fixed(n) => format!("{n} fps"),
                };
                let item = CheckMenuItem::with_id(id, text, true, fps == prefs.fps, None);
                (fps, item)
            })
            .collect(),
        quality: Quality::ALL
            .into_iter()
            .map(|q| {
                let id = format!("{ID_QUALITY_PREFIX}{}", q.key());
                let text = format!("{} quality", q.label());
                (
                    q,
                    CheckMenuItem::with_id(id, text, true, q == prefs.quality, None),
                )
            })
            .collect(),
    };
    let recording_menu = Submenu::with_id("recording", "Recording options", true);
    recording_menu.append(&recording_items.system)?;
    recording_menu.append(&recording_items.microphone)?;
    recording_menu.append(&PredefinedMenuItem::separator())?;
    for (_, item) in &recording_items.fps {
        recording_menu.append(item)?;
    }
    recording_menu.append(&PredefinedMenuItem::separator())?;
    for (_, item) in &recording_items.quality {
        recording_menu.append(item)?;
    }
    let update = MenuItem::with_id(ID_UPDATE, "Check for updates", true, None);
    let capture = MenuItem::with_id(
        ID_CAPTURE,
        with_shortcut("Capture region", labels.capture.as_ref()),
        true,
        None,
    );
    let repeat = MenuItem::with_id(
        ID_REPEAT,
        with_shortcut("Repeat last region", labels.repeat.as_ref()),
        true,
        None,
    );
    menu.append_items(&[
        &capture,
        &MenuItem::with_id(ID_DELAYED, "Capture in 3 seconds", true, None),
        &repeat,
        &PredefinedMenuItem::separator(),
        &record,
        &record_gif,
        &recording_menu,
        &PredefinedMenuItem::separator(),
        &MenuItem::with_id(ID_LIBRARY, "Open library", true, None),
        &MenuItem::with_id(ID_OPEN, "Open captures folder", true, None),
        &theme_menu,
        &MenuItem::with_id(ID_SHORTCUTS, "Keyboard shortcuts…", true, None),
        &PredefinedMenuItem::separator(),
        &update,
        &MenuItem::with_id(ID_QUIT, "Quit Snapwright", true, None),
    ])?;

    let (tx, rx) = mpsc::unbounded();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        if let Some(action) = action_for(&event.id) {
            let _ = tx.unbounded_send(action);
        }
    }));

    let builder = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("Snapwright");
    // macOS tints a black template image to match the menu bar; other trays show the colour.
    #[cfg(target_os = "macos")]
    let builder = builder.with_icon_templated(Icon::from_rgba(
        icon_rgba(ICON_SIZE, TEMPLATE),
        ICON_SIZE,
        ICON_SIZE,
    )?);
    #[cfg(not(target_os = "macos"))]
    let builder = builder.with_icon(Icon::from_rgba(
        icon_rgba(ICON_SIZE, ACCENT),
        ICON_SIZE,
        ICON_SIZE,
    )?);
    let icon = builder.build()?;
    Ok((
        Tray {
            _icon: icon,
            capture,
            repeat,
            record,
            record_gif,
            themes,
            recording_items,
            update,
            labels: RefCell::new(labels.clone()),
            recording: Cell::new(false),
        },
        rx,
    ))
}

fn action_for(id: &MenuId) -> Option<Action> {
    match id.as_ref() {
        ID_CAPTURE => Some(Action::Capture),
        ID_DELAYED => Some(Action::CaptureDelayed),
        ID_REPEAT => Some(Action::RepeatLast),
        ID_RECORD => Some(Action::Record),
        ID_RECORD_GIF => Some(Action::RecordGif),
        ID_LIBRARY => Some(Action::OpenLibrary),
        ID_OPEN => Some(Action::OpenFolder),
        ID_SHORTCUTS => Some(Action::Shortcuts),
        ID_UPDATE => Some(Action::Update),
        ID_QUIT => Some(Action::Quit),
        ID_SYSTEM_AUDIO => Some(Action::Recording(RecordingChoice::SystemAudio)),
        ID_MICROPHONE => Some(Action::Recording(RecordingChoice::Microphone)),
        other => {
            if let Some(fps) = other.strip_prefix(ID_FPS_PREFIX) {
                return FrameRate::from_key(fps)
                    .map(|fps| Action::Recording(RecordingChoice::Fps(fps)));
            }
            if let Some(q) = other.strip_prefix(ID_QUALITY_PREFIX) {
                return Quality::from_key(q)
                    .map(|q| Action::Recording(RecordingChoice::Quality(q)));
            }
            other
                .strip_prefix(ID_THEME_PREFIX)
                .and_then(Choice::from_key)
                .map(Action::Theme)
        }
    }
}

const ICON_SIZE: u32 = 32;
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
const TEMPLATE: [u8; 4] = [0, 0, 0, 255];
/// Readable on both light and dark taskbars.
#[cfg_attr(target_os = "macos", allow(dead_code))]
const ACCENT: [u8; 4] = [0x2f, 0x80, 0xed, 255];

/// Draws the tray glyph in `colour`: four viewfinder corners around a dot, on transparent.
fn icon_rgba(size: u32, colour: [u8; 4]) -> Vec<u8> {
    let s = size as i32;
    let edge = s / 8; // margin
    let arm = s * 5 / 16; // length of each corner arm
    let thick = (s / 12).max(2);
    let far = s - 1 - edge;
    let mut out = vec![0u8; (size * size * 4) as usize];
    for y in 0..s {
        for x in 0..s {
            let near_x = x - edge;
            let near_y = y - edge;
            let from_far_x = far - x;
            let from_far_y = far - y;
            let in_box = near_x >= 0 && near_y >= 0 && from_far_x >= 0 && from_far_y >= 0;
            let on_vertical = near_x < thick || from_far_x < thick;
            let on_horizontal = near_y < thick || from_far_y < thick;
            let in_corner_x = near_x < arm || from_far_x < arm;
            let in_corner_y = near_y < arm || from_far_y < arm;
            let corner = in_box && in_corner_x && in_corner_y && (on_vertical || on_horizontal);
            let c = (s - 1) as f32 / 2.0;
            let r = s as f32 / 8.0;
            let (dx, dy) = (x as f32 - c, y as f32 - c);
            let dot = dx * dx + dy * dy <= r * r;
            if corner || dot {
                let i = ((y * s + x) * 4) as usize;
                out[i..i + 4].copy_from_slice(&colour);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_ids_map_to_actions() {
        assert_eq!(action_for(&MenuId::new(ID_CAPTURE)), Some(Action::Capture));
        assert_eq!(
            action_for(&MenuId::new(ID_DELAYED)),
            Some(Action::CaptureDelayed)
        );
        assert_eq!(
            action_for(&MenuId::new(ID_REPEAT)),
            Some(Action::RepeatLast)
        );
        assert_eq!(action_for(&MenuId::new(ID_RECORD)), Some(Action::Record));
        assert_eq!(
            action_for(&MenuId::new(ID_RECORD_GIF)),
            Some(Action::RecordGif)
        );
        assert_eq!(action_for(&MenuId::new(ID_OPEN)), Some(Action::OpenFolder));
        assert_eq!(
            action_for(&MenuId::new(ID_LIBRARY)),
            Some(Action::OpenLibrary)
        );
        assert_eq!(
            action_for(&MenuId::new(ID_SHORTCUTS)),
            Some(Action::Shortcuts)
        );
        assert_eq!(action_for(&MenuId::new(ID_UPDATE)), Some(Action::Update));
        assert_eq!(action_for(&MenuId::new(ID_QUIT)), Some(Action::Quit));
        assert_eq!(
            action_for(&MenuId::new("theme-omarchy")),
            Some(Action::Theme(Choice::Omarchy))
        );
        assert_eq!(
            action_for(&MenuId::new("theme-tokyo-night")),
            Some(Action::Theme(Choice::Named("tokyo-night")))
        );
        assert_eq!(action_for(&MenuId::new("theme-sepia")), None);
        assert_eq!(action_for(&MenuId::new("other")), None);
        assert_eq!(
            action_for(&MenuId::new(ID_SYSTEM_AUDIO)),
            Some(Action::Recording(RecordingChoice::SystemAudio))
        );
        assert_eq!(
            action_for(&MenuId::new(ID_MICROPHONE)),
            Some(Action::Recording(RecordingChoice::Microphone))
        );
        assert_eq!(
            action_for(&MenuId::new("record-fps-30")),
            Some(Action::Recording(RecordingChoice::Fps(FrameRate::Fixed(
                30
            ))))
        );
        assert_eq!(
            action_for(&MenuId::new("record-fps-display")),
            Some(Action::Recording(RecordingChoice::Fps(FrameRate::Display)))
        );
        assert_eq!(
            action_for(&MenuId::new("record-quality-maximum")),
            Some(Action::Recording(RecordingChoice::Quality(
                Quality::Maximum
            )))
        );
        assert_eq!(action_for(&MenuId::new("record-quality-ultra")), None);
    }

    #[test]
    fn icon_has_corners_and_dot_but_clear_edges() {
        let px = icon_rgba(ICON_SIZE, TEMPLATE);
        assert_eq!(px.len(), 32 * 32 * 4);
        let alpha = |x: u32, y: u32| px[((y * 32 + x) * 4 + 3) as usize];
        assert_eq!(alpha(0, 0), 0, "outer margin is transparent");
        assert_eq!(alpha(4, 4), 255, "top-left corner is drawn");
        assert_eq!(alpha(27, 27), 255, "bottom-right corner is drawn");
        assert_eq!(alpha(16, 4), 0, "top edge has a gap between corners");
        assert_eq!(alpha(16, 16), 255, "centre dot is drawn");
    }
}
