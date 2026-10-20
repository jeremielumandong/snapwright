//! The system's sound and the microphone as [`AudioSource`]s: cpal on macOS and Windows,
//! `ffmpeg -f pulse` on Linux.

use std::any::Any;

use crate::Result;
use crate::audio::{AudioFeed, AudioSource};

/// The default output device's sound (loopback) or the default microphone.
pub(crate) struct Device {
    system: bool,
}

impl Device {
    pub(crate) fn system() -> Self {
        Self { system: true }
    }

    pub(crate) fn microphone() -> Self {
        Self { system: false }
    }
}

impl AudioSource for Device {
    fn name(&self) -> String {
        if self.system {
            "system audio".into()
        } else {
            "microphone".into()
        }
    }

    fn start(self: Box<Self>, feed: AudioFeed) -> Result<Box<dyn Any>> {
        platform::start(self.system, feed)
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
mod platform {
    use std::any::Any;

    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::{FromSample, SampleFormat, SizedSample};

    use crate::audio::AudioFeed;
    use crate::{RecordError, Result};

    fn fail(api: &'static str, e: impl std::fmt::Display) -> RecordError {
        RecordError::Encoder {
            api,
            message: e.to_string(),
        }
    }

    /// Opens the default output device for loopback (`system`) or the default input, and
    /// starts it. On macOS loopback needs 14.6 or later and the System Audio Recording
    /// permission; the microphone needs the Microphone permission.
    pub(super) fn start(system: bool, feed: AudioFeed) -> Result<Box<dyn Any>> {
        let host = cpal::default_host();
        let device = if system {
            host.default_output_device()
        } else {
            host.default_input_device()
        }
        .ok_or_else(|| {
            RecordError::Unsupported(if system {
                "no output device to record".into()
            } else {
                "no microphone".into()
            })
        })?;
        let config = if system {
            device.default_output_config()
        } else {
            device.default_input_config()
        }
        .map_err(|e| fail("default stream config", e))?;
        let channels = config.channels();
        let rate = config.sample_rate();
        let stream = match config.sample_format() {
            SampleFormat::F32 => build::<f32>(&device, config.into(), feed, channels, rate),
            SampleFormat::I16 => build::<i16>(&device, config.into(), feed, channels, rate),
            SampleFormat::I32 => build::<i32>(&device, config.into(), feed, channels, rate),
            SampleFormat::U16 => build::<u16>(&device, config.into(), feed, channels, rate),
            SampleFormat::F64 => build::<f64>(&device, config.into(), feed, channels, rate),
            SampleFormat::I8 => build::<i8>(&device, config.into(), feed, channels, rate),
            SampleFormat::U8 => build::<u8>(&device, config.into(), feed, channels, rate),
            other => Err(RecordError::Unsupported(format!(
                "audio sample format {other}"
            ))),
        }?;
        stream.play().map_err(|e| fail("play", e))?;
        Ok(Box::new(stream))
    }

    fn build<T>(
        device: &cpal::Device,
        config: cpal::StreamConfig,
        feed: AudioFeed,
        channels: u16,
        rate: u32,
    ) -> Result<cpal::Stream>
    where
        T: SizedSample + Send + 'static,
        f32: FromSample<T>,
    {
        let mut buffer = Vec::new();
        device
            .build_input_stream(
                config,
                move |data: &[T], _: &cpal::InputCallbackInfo| {
                    buffer.clear();
                    buffer.extend(data.iter().map(|s| s.to_sample::<f32>()));
                    feed.push(&buffer, channels, rate);
                },
                |e| tracing::warn!(error = %e, "audio stream"),
                None,
            )
            .map_err(|e| fail("build_input_stream", e))
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use std::any::Any;
    use std::io::Read as _;
    use std::process::{Child, Command, Stdio};
    use std::sync::mpsc;
    use std::time::Duration;

    use crate::audio::{AudioFeed, CHANNELS, SAMPLE_RATE};
    use crate::{RecordError, Result};

    /// How long PulseAudio (or PipeWire's Pulse server) gets to send the first samples.
    const FIRST_DATA: Duration = Duration::from_secs(2);

    /// ffmpeg reading a Pulse source, killed when dropped.
    struct Running {
        child: Child,
        reader: Option<std::thread::JoinHandle<()>>,
    }

    impl Drop for Running {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            if let Some(reader) = self.reader.take() {
                let _ = reader.join();
            }
        }
    }

    /// Runs `ffmpeg -f pulse` on the default sink's monitor (`system`) or the default source,
    /// as 48 kHz stereo floats on a pipe.
    pub(super) fn start(system: bool, feed: AudioFeed) -> Result<Box<dyn Any>> {
        let device = if system {
            "@DEFAULT_MONITOR@"
        } else {
            "default"
        };
        let mut child = Command::new(crate::ffmpeg::binary())
            .args(["-hide_banner", "-loglevel", "error", "-nostdin"])
            .args(["-f", "pulse", "-fragment_size", "3840", "-i", device])
            .args(["-f", "f32le", "-ac", "2", "-ar", &SAMPLE_RATE.to_string()])
            .arg("pipe:1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => {
                    RecordError::Unsupported("recording sound on Linux needs ffmpeg".into())
                }
                _ => RecordError::Encoder {
                    api: "ffmpeg",
                    message: e.to_string(),
                },
            })?;
        let Some(mut stdout) = child.stdout.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(RecordError::Panicked);
        };
        let (first_tx, first_rx) = mpsc::sync_channel(1);
        let reader = std::thread::Builder::new()
            .name("snapwright-pulse".into())
            .spawn(move || {
                let mut bytes = vec![0u8; 3840];
                let mut samples = Vec::new();
                let mut first = Some(first_tx);
                let mut carry = 0usize;
                loop {
                    let n = match stdout.read(&mut bytes[carry..]) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n + carry,
                    };
                    let whole = n / 4 * 4;
                    samples.clear();
                    samples.extend(
                        bytes[..whole]
                            .as_chunks::<4>()
                            .0
                            .iter()
                            .map(|b| f32::from_le_bytes(*b)),
                    );
                    feed.push(&samples, CHANNELS as u16, SAMPLE_RATE);
                    bytes.copy_within(whole..n, 0);
                    carry = n - whole;
                    if let Some(tx) = first.take() {
                        let _ = tx.send(());
                    }
                }
            })
            .map_err(|_| RecordError::Panicked)?;
        let mut running = Running {
            child,
            reader: Some(reader),
        };
        if first_rx.recv_timeout(FIRST_DATA).is_ok() {
            return Ok(Box::new(running));
        }
        // No sound came: no Pulse server, or ffmpeg without Pulse. Say what ffmpeg said.
        let _ = running.child.kill();
        let _ = running.child.wait();
        let mut stderr = String::new();
        if let Some(mut err) = running.child.stderr.take() {
            let _ = err.read_to_string(&mut stderr);
        }
        let message = stderr.trim();
        Err(RecordError::Encoder {
            api: "ffmpeg -f pulse",
            message: if message.is_empty() {
                "no sound from PulseAudio".into()
            } else {
                message.to_owned()
            },
        })
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
mod platform {
    use std::any::Any;

    use crate::audio::AudioFeed;
    use crate::{RecordError, Result};

    pub(super) fn start(_: bool, _: AudioFeed) -> Result<Box<dyn Any>> {
        Err(RecordError::Unsupported(
            "recording sound is not available on this platform".into(),
        ))
    }
}
