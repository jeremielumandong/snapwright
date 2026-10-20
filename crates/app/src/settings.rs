//! Preferences that outlive a run, in `settings.toml` in the platform config folder
//! (`~/.config/snapwright` on Linux). One `key = "value"` per line: the theme, and the
//! shortcuts and recording options that differ from the defaults. Read once before the app starts and written
//! off the UI thread.

use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Context as _, Result};

use snapwright_record::Quality;

use crate::recording::{FrameRate, Prefs};
use crate::shortcuts::{self, Shortcut, Shortcuts};
use crate::theme::Choice;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Settings {
    /// The theme picked from the tray; `None` until the user picks one.
    pub theme: Option<Choice>,
    pub shortcuts: Shortcuts,
    /// Sound, frame rate and quality for recordings.
    pub recording: Prefs,
}

/// Saves after reading, so two changes saved at once both land.
static WRITING: Mutex<()> = Mutex::new(());

/// Changes the saved settings with `change`. Disk I/O: run it off the UI thread.
pub fn update(change: impl FnOnce(&mut Settings)) -> Result<()> {
    let _guard = WRITING
        .lock()
        .map_err(|_| anyhow::anyhow!("settings lock poisoned"))?;
    let mut settings = load();
    change(&mut settings);
    save(&settings)
}

fn path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "Snapwright")
        .map(|d| d.config_dir().join("settings.toml"))
}

/// The saved settings, or the defaults when there are none.
pub fn load() -> Settings {
    path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| parse(&s))
        .unwrap_or_default()
}

/// Writes the settings.
fn save(settings: &Settings) -> Result<()> {
    let path = path().context("no home directory")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("toml.part");
    std::fs::write(&tmp, render(settings))?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

fn parse(src: &str) -> Settings {
    let mut out = Settings::default();
    for line in src.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches('"');
        let key = key.trim();
        let on = || match value.to_ascii_lowercase().as_str() {
            "on" | "true" | "yes" => Some(true),
            "off" | "false" | "no" => Some(false),
            _ => None,
        };
        if key == "theme" {
            out.theme = Choice::from_key(value);
        } else if key == "recording.system-audio" {
            out.recording.audio.system = on().unwrap_or(out.recording.audio.system);
        } else if key == "recording.microphone" {
            out.recording.audio.microphone = on().unwrap_or(out.recording.audio.microphone);
        } else if key == "recording.fps" {
            if let Some(fps) = FrameRate::from_key(value) {
                out.recording.fps = fps;
            }
        } else if key == "recording.quality" {
            if let Some(q) = Quality::from_key(value) {
                out.recording.quality = q;
            }
        } else if let Some(action) = key
            .strip_prefix("shortcut.")
            .and_then(shortcuts::from_setting_name)
        {
            if value.eq_ignore_ascii_case("off") {
                out.shortcuts.set(action, None);
            } else if let Some(s) = Shortcut::parse(value) {
                out.shortcuts.set(action, Some(s));
            }
        }
    }
    out
}

fn render(settings: &Settings) -> String {
    let mut out = String::new();
    if let Some(theme) = settings.theme {
        out.push_str(&format!("theme = \"{}\"\n", theme.key()));
    }
    let defaults = Shortcuts::default();
    for (action, shortcut) in settings.shortcuts.iter() {
        if shortcut != defaults.get(action) {
            let value = shortcut.map_or_else(|| "off".to_owned(), |s| s.label());
            let name = shortcuts::setting_name(action);
            out.push_str(&format!("shortcut.{name} = \"{value}\"\n"));
        }
    }
    let rec = settings.recording;
    let defaults = Prefs::default();
    let on = |b: bool| if b { "on" } else { "off" };
    if rec.audio.system != defaults.audio.system {
        out.push_str(&format!(
            "recording.system-audio = \"{}\"\n",
            on(rec.audio.system)
        ));
    }
    if rec.audio.microphone != defaults.audio.microphone {
        out.push_str(&format!(
            "recording.microphone = \"{}\"\n",
            on(rec.audio.microphone)
        ));
    }
    if rec.fps != defaults.fps {
        out.push_str(&format!("recording.fps = \"{}\"\n", rec.fps.key()));
    }
    if rec.quality != defaults.quality {
        out.push_str(&format!("recording.quality = \"{}\"\n", rec.quality.key()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hotkey::Action;
    use crate::shortcuts::{Key, SHIFT, SUPER};

    #[test]
    fn round_trips() {
        for theme in [
            None,
            Some(Choice::Omarchy),
            Some(Choice::Light),
            Some(Choice::Named("rose-pine")),
        ] {
            let s = Settings {
                theme,
                ..Settings::default()
            };
            assert_eq!(parse(&render(&s)), s);
        }
        let mut shortcuts = Shortcuts::default();
        shortcuts.set(
            Action::Capture,
            Some(Shortcut::new(SUPER | SHIFT, Key::Letter('S'))),
        );
        shortcuts.set(Action::RecordGif, None);
        let s = Settings {
            theme: None,
            shortcuts,
            ..Settings::default()
        };
        let text = render(&s);
        assert_eq!(text.lines().count(), 2, "only the changed ones: {text}");
        assert!(text.contains("shortcut.record-gif = \"off\""), "{text}");
        assert_eq!(parse(&text), s);
        // Unreadable shortcuts keep the default.
        assert_eq!(
            parse("shortcut.capture = \"Ctrl+Nope\"\nshortcut.other = \"F1\"").shortcuts,
            Shortcuts::default()
        );
        assert_eq!(parse("# nothing\ntheme = \"sepia\"\n").theme, None);
        assert_eq!(parse("theme=dark").theme, Some(Choice::Dark));
    }

    #[test]
    fn recording_options_round_trip() {
        use snapwright_record::Audio;
        assert_eq!(render(&Settings::default()), "", "defaults are not written");
        let s = Settings {
            recording: Prefs {
                fps: FrameRate::Fixed(30),
                quality: Quality::Maximum,
                audio: Audio {
                    system: true,
                    microphone: true,
                },
            },
            ..Settings::default()
        };
        let text = render(&s);
        assert!(text.contains("recording.system-audio = \"on\""), "{text}");
        assert!(text.contains("recording.quality = \"maximum\""), "{text}");
        assert_eq!(parse(&text), s);
        // Nonsense keeps the defaults.
        let odd = parse(
            "recording.fps = \"500\"\nrecording.quality = \"ultra\"\nrecording.microphone = \"maybe\"",
        );
        assert_eq!(odd.recording, Prefs::default());
    }
}
