//! The encoder behind each format and platform.

use std::path::Path;
use std::time::Duration;

use snapwright_capture::Frame;

use crate::{Format, Options, Quality, Result};

/// Turns timestamped frames into a file. Lives on the encoder thread for its whole life, so
/// implementations need not be `Send` (COM and AVFoundation objects stay on one thread).
pub(crate) trait Encoder {
    /// Adds a frame shown from `at` until the next one. Frames have the size passed to
    /// [`open`] and arrive in time order.
    fn push(&mut self, frame: Frame, at: Duration) -> Result<()>;

    /// Adds sound: interleaved stereo at [`crate::audio::SAMPLE_RATE`] starting at `at`,
    /// right after the previous chunk. Only called when the encoder was opened with sound.
    fn push_audio(&mut self, samples: &[f32], at: Duration) -> Result<()> {
        let _ = (samples, at);
        Ok(())
    }

    /// Size of the encoded picture.
    fn size(&self) -> (u32, u32);

    /// Writes everything out; the last frame lasts until `end`.
    fn finish(self: Box<Self>, end: Duration) -> Result<()>;
}

/// What an MP4 encoder is asked for.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Video {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) fps: u32,
    pub(crate) quality: Quality,
    /// Whether there is a sound track ([`crate::audio`]'s format).
    pub(crate) audio: bool,
}

impl Video {
    /// Average bit rate for this video ([`bitrate`]).
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    pub(crate) fn bitrate(&self) -> u32 {
        bitrate(self.width, self.height, self.fps, self.quality)
    }
}

/// The sound track's AAC bit rate.
#[cfg_attr(target_os = "linux", allow(dead_code))]
pub(crate) const AUDIO_BITRATE: u32 = 192_000;

/// Opens the encoder for `options.format` writing to `path`, replacing any file there.
/// `audio` adds a sound track to an MP4.
pub(crate) fn open(
    options: Options,
    path: &Path,
    width: u32,
    height: u32,
    fps: u32,
    audio: bool,
) -> Result<Box<dyn Encoder>> {
    if path.exists() {
        std::fs::remove_file(path).map_err(|source| crate::RecordError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    }
    match options.format {
        Format::Gif => Ok(Box::new(crate::gif::GifEncoder::create(
            path,
            width,
            height,
            options.gif_max_width,
        )?)),
        Format::Mp4 => mp4(
            path,
            Video {
                width,
                height,
                fps,
                quality: options.quality,
                audio,
            },
        ),
    }
}

/// Average bit rate for H.264 screen video. Standard is about 0.1 bit per pixel per frame
/// (6 Mbit/s for 1080p at 30 fps), which keeps text crisp; High and Maximum give fast motion
/// (games, scrolling) more. Never below 1 Mbit/s, and capped where hardware encoders and
/// H.264 levels stop.
#[cfg_attr(target_os = "linux", allow(dead_code))]
pub(crate) fn bitrate(width: u32, height: u32, fps: u32, quality: Quality) -> u32 {
    let (tenths_of_bits, cap) = match quality {
        Quality::Standard => (10, 50_000_000),
        Quality::High => (15, 100_000_000),
        Quality::Maximum => (30, 160_000_000),
    };
    let bits = u64::from(width) * u64::from(height) * u64::from(fps) * tenths_of_bits / 100;
    bits.clamp(1_000_000, cap) as u32
}

/// The x264 constant-quality level for `quality` (lower is better), for ffmpeg on Linux.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn crf(quality: Quality) -> u32 {
    match quality {
        Quality::Standard => 20,
        Quality::High => 17,
        Quality::Maximum => 14,
    }
}

#[cfg(target_os = "macos")]
fn mp4(path: &Path, video: Video) -> Result<Box<dyn Encoder>> {
    Ok(Box::new(crate::macos::AvEncoder::create(path, video)?))
}

#[cfg(target_os = "windows")]
fn mp4(path: &Path, video: Video) -> Result<Box<dyn Encoder>> {
    Ok(Box::new(crate::windows::MfEncoder::create(path, video)?))
}

#[cfg(target_os = "linux")]
fn mp4(path: &Path, video: Video) -> Result<Box<dyn Encoder>> {
    Ok(Box::new(crate::ffmpeg::FfmpegEncoder::create(path, video)?))
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
fn mp4(_: &Path, _: Video) -> Result<Box<dyn Encoder>> {
    Err(crate::RecordError::Unsupported(
        "MP4 recording is not available on this platform".into(),
    ))
}
