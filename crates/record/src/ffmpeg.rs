//! MP4 on Linux through an `ffmpeg` process (libx264).
//!
//! Linux has no system H.264 encoder to call, and bundling one brings patent licensing with
//! it, so recordings use the `ffmpeg` the user has installed (`SNAPWRIGHT_FFMPEG` names
//! another binary). Raw BGRA frames go down a pipe at a constant rate; a frame that arrives
//! late is repeated to fill the gap, so the video keeps wall-clock time.
//!
//! With sound, the video goes to a file beside the target and the sound to a raw 48 kHz
//! float file; at the end a second, quick ffmpeg run copies the video and encodes the sound
//! to AAC into the target.

use std::fs::File;
use std::io::{BufWriter, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::Duration;

use snapwright_capture::Frame;

use crate::audio::{CHANNELS, SAMPLE_RATE};
use crate::encoder::{Encoder, Video, crf};
use crate::{RecordError, Result};

/// Sound written so far, to be muxed in at the end.
struct Sound {
    raw: PathBuf,
    file: BufWriter<File>,
    /// Sample frames written.
    frames: u64,
}

pub(crate) struct FfmpegEncoder {
    child: Child,
    stdin: Option<ChildStdin>,
    /// The final file.
    path: PathBuf,
    /// Where ffmpeg writes the video: `path`, or a file beside it when sound is muxed later.
    video_path: PathBuf,
    sound: Option<Sound>,
    width: u32,
    height: u32,
    fps: u32,
    /// Frames written so far.
    written: u64,
    last: Option<Frame>,
}

/// The ffmpeg binary to run.
pub(crate) fn binary() -> PathBuf {
    std::env::var_os("SNAPWRIGHT_FFMPEG")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("ffmpeg"))
}

impl FfmpegEncoder {
    pub(crate) fn create(path: &Path, video: Video) -> Result<Self> {
        let Video {
            width,
            height,
            fps,
            quality,
            audio,
        } = video;
        let io = |p: &Path| {
            let p = p.to_path_buf();
            move |source| RecordError::Io { path: p, source }
        };
        let (video_path, sound) = if audio {
            let raw = path.with_extension("sound.f32");
            let file = File::create(&raw).map_err(io(&raw))?;
            (
                path.with_extension("video.mp4"),
                Some(Sound {
                    raw,
                    file: BufWriter::new(file),
                    frames: 0,
                }),
            )
        } else {
            (path.to_path_buf(), None)
        };
        let crf = crf(quality).to_string();
        let mut child = Command::new(binary())
            .args(["-hide_banner", "-loglevel", "error", "-y"])
            .args(["-f", "rawvideo", "-pix_fmt", "bgra"])
            .args(["-video_size", &format!("{width}x{height}")])
            .args(["-framerate", &fps.to_string(), "-i", "pipe:0"])
            .args(["-c:v", "libx264", "-preset", "veryfast", "-crf", &crf])
            // BT.709 in limited range, tagged, so players show the colours the screen had.
            .args([
                "-vf",
                "scale=out_color_matrix=bt709:out_range=tv,format=yuv420p",
            ])
            .args(["-colorspace", "bt709", "-color_primaries", "bt709"])
            .args(["-color_trc", "bt709", "-color_range", "tv"])
            .args(["-movflags", "+faststart"])
            .arg(&video_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .inspect_err(|_| {
                if let Some(sound) = &sound {
                    let _ = std::fs::remove_file(&sound.raw);
                }
            })
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => RecordError::Unsupported(
                    "MP4 recording on Linux needs ffmpeg; install it, or record a GIF".into(),
                ),
                _ => RecordError::Encoder {
                    api: "ffmpeg",
                    message: e.to_string(),
                },
            })?;
        let stdin = child.stdin.take();
        Ok(Self {
            child,
            stdin,
            path: path.to_path_buf(),
            video_path,
            sound,
            width,
            height,
            fps,
            written: 0,
            last: None,
        })
    }

    /// Writes `frame` until `count` frames are in the pipe.
    fn fill(&mut self, frame: &Frame, count: u64) -> Result<()> {
        while self.written < count {
            let Some(stdin) = self.stdin.as_mut() else {
                break;
            };
            if let Err(e) = stdin.write_all(&frame.data) {
                // ffmpeg exited; its own message says why.
                self.stdin = None;
                return Err(self.failure(e.to_string()));
            }
            self.written += 1;
        }
        Ok(())
    }

    /// Reaps ffmpeg and builds an error from what it printed.
    fn failure(&mut self, fallback: String) -> RecordError {
        let _ = self.child.wait();
        let mut stderr = String::new();
        if let Some(mut err) = self.child.stderr.take() {
            let _ = err.read_to_string(&mut stderr);
        }
        let message = stderr.trim();
        RecordError::Encoder {
            api: "ffmpeg",
            message: if message.is_empty() {
                fallback
            } else {
                message.to_owned()
            },
        }
    }

    fn slot(&self, at: Duration) -> u64 {
        (at.as_secs_f64() * f64::from(self.fps)).round() as u64
    }
}

impl Encoder for FfmpegEncoder {
    fn push(&mut self, frame: Frame, at: Duration) -> Result<()> {
        // Repeat the previous frame up to this one's slot, then write this one.
        let slot = self.slot(at);
        if let Some(last) = self.last.take() {
            self.fill(&last, slot)?;
        }
        if self.written <= slot {
            self.fill(&frame, slot + 1)?;
        }
        self.last = Some(frame);
        Ok(())
    }

    fn push_audio(&mut self, samples: &[f32], at: Duration) -> Result<()> {
        let Some(sound) = self.sound.as_mut() else {
            return Ok(());
        };
        let io = |source| RecordError::Io {
            path: sound.raw.clone(),
            source,
        };
        // Chunks follow each other; a gap (there should be none) is filled with silence so
        // the sound keeps its place against the video.
        let start = crate::audio::frames(at);
        let silence = start.saturating_sub(sound.frames) as usize * CHANNELS;
        for _ in 0..silence {
            sound.file.write_all(&0f32.to_le_bytes()).map_err(io)?;
        }
        for s in samples {
            sound.file.write_all(&s.to_le_bytes()).map_err(io)?;
        }
        sound.frames = sound.frames.max(start) + (samples.len() / CHANNELS) as u64;
        Ok(())
    }

    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn finish(mut self: Box<Self>, end: Duration) -> Result<()> {
        let end = self.slot(end);
        if let Some(last) = self.last.take() {
            self.fill(&last, end)?;
        }
        // Closing stdin tells ffmpeg the input ended; it then writes the index and exits.
        drop(self.stdin.take());
        let status = self.child.wait().map_err(|e| RecordError::Encoder {
            api: "ffmpeg",
            message: e.to_string(),
        })?;
        if !status.success() {
            let path = self.path.display().to_string();
            return Err(self.failure(format!("{path}: ffmpeg exited with {status}")));
        }
        match self.sound.take() {
            Some(sound) => self.mux(sound),
            None => Ok(()),
        }
    }
}

impl FfmpegEncoder {
    /// Copies the video and encodes the sound to AAC into the final file, then removes the
    /// two parts.
    fn mux(&mut self, sound: Sound) -> Result<()> {
        let Sound { raw, file, .. } = sound;
        let flushed = file.into_inner().map_err(|e| RecordError::Io {
            path: raw.clone(),
            source: e.into_error(),
        });
        let result = flushed.and_then(|_| {
            let output = Command::new(binary())
                .args(["-hide_banner", "-loglevel", "error", "-y", "-nostdin"])
                .arg("-i")
                .arg(&self.video_path)
                .args(["-f", "f32le", "-ar", &SAMPLE_RATE.to_string()])
                .args(["-ac", &CHANNELS.to_string(), "-i"])
                .arg(&raw)
                .args(["-map", "0:v", "-map", "1:a", "-c:v", "copy"])
                .args(["-c:a", "aac", "-b:a"])
                .arg(crate::encoder::AUDIO_BITRATE.to_string())
                .args(["-shortest", "-movflags", "+faststart"])
                .arg(&self.path)
                .stdin(Stdio::null())
                .output()
                .map_err(|e| RecordError::Encoder {
                    api: "ffmpeg",
                    message: e.to_string(),
                })?;
            if output.status.success() {
                Ok(())
            } else {
                let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
                Err(RecordError::Encoder {
                    api: "ffmpeg (adding sound)",
                    message: if message.is_empty() {
                        format!("exited with {}", output.status)
                    } else {
                        message
                    },
                })
            }
        });
        let _ = std::fs::remove_file(&raw);
        match result {
            Ok(()) => {
                let _ = std::fs::remove_file(&self.video_path);
                Ok(())
            }
            Err(e) => {
                // Keep the video: a recording without sound beats none.
                tracing::warn!(error = %e, "could not add the sound; keeping the video");
                std::fs::rename(&self.video_path, &self.path).map_err(|source| RecordError::Io {
                    path: self.path.clone(),
                    source,
                })
            }
        }
    }
}

impl Drop for FfmpegEncoder {
    fn drop(&mut self) {
        // Only reached with ffmpeg still running when `finish` was skipped (an error).
        drop(self.stdin.take());
        let _ = self.child.wait();
        if let Some(sound) = self.sound.take() {
            drop(sound.file);
            let _ = std::fs::remove_file(&sound.raw);
        }
    }
}
