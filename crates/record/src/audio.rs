//! Sound for MP4 recordings: what the computer plays (loopback) and the microphone.
//!
//! Each [`AudioSource`] pushes samples at its own rate and channel count into an
//! [`AudioFeed`], which converts them to 48 kHz stereo. The capture thread's [`Mixer`] sums
//! the feeds into one track on the recording's clock: it takes sound up to a little behind
//! the latest video frame, and a source that has nothing yet (a loopback device while
//! nothing plays sends no packets at all) is silence there. Sound that arrives before the
//! recording starts or while it is paused is dropped, so the track keeps the video's time.
//!
//! Devices: cpal on macOS (Core Audio; the system's output through a process tap, macOS
//! 14.6+) and Windows (WASAPI; loopback on the default output device). On Linux the
//! installed `ffmpeg` reads PulseAudio or PipeWire (`-f pulse`), its monitor source for the
//! system's sound.

use std::any::Any;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::Result;

/// The track's sample rate.
pub const SAMPLE_RATE: u32 = 48_000;
/// The track's channels (interleaved left, right).
pub const CHANNELS: usize = 2;

/// How far behind the latest video frame the mix is taken, so a source that delivers in
/// bursts (10 to 100 ms on most devices) is not cut into silence and sound.
const LATENCY: Duration = Duration::from_millis(250);
/// A source further ahead than this loses its oldest sound (a clock running fast, or a burst
/// queued before the start), so it cannot drift away from the video.
const MAX_BACKLOG: Duration = Duration::from_millis(500);

/// Which sounds an MP4 records. GIFs have no sound.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Audio {
    /// What the computer plays: games, videos, calls.
    pub system: bool,
    /// The default microphone.
    pub microphone: bool,
}

impl Audio {
    /// Whether any sound is recorded.
    pub fn any(self) -> bool {
        self.system || self.microphone
    }
}

/// Where sound comes from.
pub trait AudioSource: Send + 'static {
    /// A name for messages, such as "microphone".
    fn name(&self) -> String;

    /// Starts sending sound into `feed`. Sound flows until the returned value is dropped;
    /// it is created and dropped on the same thread, so it need not be `Send`.
    fn start(self: Box<Self>, feed: AudioFeed) -> Result<Box<dyn Any>>;
}

/// The devices `audio` asks for on this platform.
pub fn device_sources(audio: Audio) -> Vec<Box<dyn AudioSource>> {
    let mut out: Vec<Box<dyn AudioSource>> = Vec::new();
    if audio.system {
        out.push(Box::new(crate::audio_device::Device::system()));
    }
    if audio.microphone {
        out.push(Box::new(crate::audio_device::Device::microphone()));
    }
    out
}

/// Takes sound from one source, from any thread (normally the device's callback).
#[derive(Clone)]
pub struct AudioFeed(Arc<Mutex<Pending>>);

impl AudioFeed {
    /// Adds interleaved samples with `channels` channels at `rate` Hz. Mono goes to both
    /// sides; past two channels only the first two (front left and right) are kept.
    pub fn push(&self, samples: &[f32], channels: u16, rate: u32) {
        if channels == 0 || rate == 0 {
            return;
        }
        let Ok(mut pending) = self.0.lock() else {
            return;
        };
        pending.push(samples, usize::from(channels), rate);
    }
}

/// One source's converted sound waiting to be mixed, interleaved stereo at 48 kHz.
#[derive(Default)]
struct Pending {
    queue: VecDeque<f32>,
    resampler: Resampler,
}

impl Pending {
    fn push(&mut self, samples: &[f32], channels: usize, rate: u32) {
        for frame in samples.chunks_exact(channels) {
            let left = frame[0];
            let right = if channels > 1 { frame[1] } else { left };
            self.resampler.push(rate, [left, right], &mut self.queue);
        }
        let max = frames(MAX_BACKLOG) as usize * CHANNELS;
        if self.queue.len() > max {
            let excess = self.queue.len() - max;
            self.queue.drain(..excess);
        }
    }
}

/// Linear interpolation from any rate to [`SAMPLE_RATE`]: plenty for speech and game
/// sound, and devices are nearly always at 48 or 44.1 kHz.
#[derive(Default)]
struct Resampler {
    rate: u32,
    /// Where the next output sample falls between `prev` (0) and the next input (1).
    t: f64,
    prev: Option<[f32; 2]>,
}

impl Resampler {
    fn push(&mut self, rate: u32, frame: [f32; 2], out: &mut VecDeque<f32>) {
        if rate != self.rate {
            *self = Self {
                rate,
                ..Self::default()
            };
        }
        if rate == SAMPLE_RATE {
            out.extend(frame);
            return;
        }
        let Some(prev) = self.prev.replace(frame) else {
            out.extend(frame);
            self.t = f64::from(rate) / f64::from(SAMPLE_RATE);
            return;
        };
        let step = f64::from(rate) / f64::from(SAMPLE_RATE);
        while self.t < 1.0 {
            let t = self.t as f32;
            out.push_back(prev[0] + (frame[0] - prev[0]) * t);
            out.push_back(prev[1] + (frame[1] - prev[1]) * t);
            self.t += step;
        }
        self.t -= 1.0;
    }
}

/// Sample frames (one per channel) in `d`.
pub(crate) fn frames(d: Duration) -> u64 {
    (d.as_secs_f64() * f64::from(SAMPLE_RATE)).round() as u64
}

/// Sums the sources into one track on the recording's clock.
#[derive(Default)]
pub(crate) struct Mixer {
    inputs: Vec<AudioFeed>,
    /// Frames handed out so far.
    emitted: u64,
}

impl Mixer {
    /// A new input to the mix.
    pub(crate) fn feed(&mut self) -> AudioFeed {
        let feed = AudioFeed(Arc::default());
        self.inputs.push(feed.clone());
        feed
    }

    /// Drops what the sources have sent so far: sound from before the clock started, or
    /// from while the recording is paused.
    pub(crate) fn discard(&self) {
        for input in &self.inputs {
            if let Ok(mut pending) = input.0.lock() {
                pending.queue.clear();
            }
        }
    }

    /// The mix from where the last one ended up to `until` (recording time), or `None` when
    /// that is less than `min`. Sources short of it are silent for the rest.
    pub(crate) fn take(&mut self, until: Duration, min: Duration) -> Option<(Vec<f32>, Duration)> {
        let end = frames(until);
        if end < self.emitted + frames(min).max(1) {
            return None;
        }
        let at = Duration::from_secs_f64(self.emitted as f64 / f64::from(SAMPLE_RATE));
        let len = (end - self.emitted) as usize * CHANNELS;
        let mut mix = vec![0f32; len];
        for input in &self.inputs {
            let Ok(mut pending) = input.0.lock() else {
                continue;
            };
            let n = pending.queue.len().min(len);
            for (out, s) in mix.iter_mut().zip(pending.queue.drain(..n)) {
                *out += s;
            }
        }
        for s in &mut mix {
            *s = s.clamp(-1.0, 1.0);
        }
        self.emitted = end;
        Some((mix, at))
    }

    /// The mix up to where it can safely be taken while recording, `LATENCY` behind `now`.
    pub(crate) fn take_live(&mut self, now: Duration) -> Option<(Vec<f32>, Duration)> {
        self.take(now.saturating_sub(LATENCY), Duration::from_millis(20))
    }
}

/// A source that never started is reported, and the recording goes on without it.
pub(crate) struct Started {
    /// Keeps the sources that started running; dropped on the audio thread.
    pub(crate) stop: std::sync::mpsc::Sender<()>,
    pub(crate) thread: std::thread::JoinHandle<()>,
}

impl Started {
    /// Stops the sources and waits for their thread.
    pub(crate) fn stop(self) {
        drop(self.stop);
        let _ = self.thread.join();
    }
}

/// Starts `sources` on a thread of their own (device objects stay on the thread that made
/// them), each into a new input of `mixer`. Returns the running thread, if any source
/// started, and a message for each that did not.
pub(crate) fn start(
    sources: Vec<Box<dyn AudioSource>>,
    mixer: &mut Mixer,
) -> (Option<Started>, Vec<String>) {
    if sources.is_empty() {
        return (None, Vec::new());
    }
    let jobs: Vec<_> = sources.into_iter().map(|s| (s, mixer.feed())).collect();
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let spawned = std::thread::Builder::new()
        .name("snapwright-audio".into())
        .spawn(move || {
            let mut running = Vec::new();
            let mut problems = Vec::new();
            for (source, feed) in jobs {
                let name = source.name();
                match source.start(feed) {
                    Ok(guard) => running.push(guard),
                    Err(e) => problems.push(format!("{name}: {e}")),
                }
            }
            let any = !running.is_empty();
            let _ = ready_tx.send((any, problems));
            if any {
                // Until the recording ends (the sender is dropped).
                let _ = stop_rx.recv();
            }
            drop(running);
        });
    let thread = match spawned {
        Ok(t) => t,
        Err(e) => return (None, vec![format!("audio thread: {e}")]),
    };
    match ready_rx.recv() {
        Ok((true, problems)) => (
            Some(Started {
                stop: stop_tx,
                thread,
            }),
            problems,
        ),
        Ok((false, problems)) => {
            let _ = thread.join();
            (None, problems)
        }
        Err(_) => {
            let _ = thread.join();
            (None, vec!["the audio thread stopped unexpectedly".into()])
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn mono_goes_to_both_sides_and_extra_channels_are_dropped() {
        let mut m = Mixer::default();
        let feed = m.feed();
        feed.push(&[0.25, -0.5], 1, SAMPLE_RATE);
        feed.push(&[0.1, 0.2, 0.9, 0.9], 4, SAMPLE_RATE);
        let (mix, at) = m
            .take(Duration::from_secs_f64(3.0 / 48_000.0), Duration::ZERO)
            .unwrap();
        assert_eq!(at, Duration::ZERO);
        assert_eq!(mix, vec![0.25, 0.25, -0.5, -0.5, 0.1, 0.2]);
    }

    #[test]
    fn sources_are_summed_clipped_and_padded_with_silence() {
        let mut m = Mixer::default();
        let a = m.feed();
        let b = m.feed();
        a.push(&[0.75; 8], 2, SAMPLE_RATE);
        b.push(&[0.5; 4], 2, SAMPLE_RATE);
        let (mix, _) = m
            .take(Duration::from_secs_f64(5.0 / 48_000.0), Duration::ZERO)
            .unwrap();
        assert_eq!(
            mix,
            vec![1.0, 1.0, 1.0, 1.0, 0.75, 0.75, 0.75, 0.75, 0.0, 0.0]
        );
        // The next chunk starts where this one ended.
        a.push(&[0.1; 2], 2, SAMPLE_RATE);
        let (mix, at) = m
            .take(Duration::from_secs_f64(6.0 / 48_000.0), Duration::ZERO)
            .unwrap();
        assert_eq!(at, Duration::from_secs_f64(5.0 / 48_000.0));
        assert_eq!(mix, vec![0.1, 0.1]);
    }

    #[test]
    fn resampling_keeps_time() {
        let mut m = Mixer::default();
        let feed = m.feed();
        // 0.4 s at 44.1 kHz (under the backlog cap) becomes 0.4 s at 48 kHz.
        let tone: Vec<f32> = (0..17_640)
            .flat_map(|i| {
                let s = (i as f32 * 440.0 * std::f32::consts::TAU / 44_100.0).sin() * 0.5;
                [s, s]
            })
            .collect();
        for chunk in tone.chunks(882) {
            feed.push(chunk, 2, 44_100);
        }
        let queued = feed.0.lock().unwrap().queue.len() / CHANNELS;
        assert!((19_195..=19_205).contains(&queued), "{queued} frames");
        // Still a smooth wave: no step bigger than the tone's own slope.
        let q = &feed.0.lock().unwrap().queue;
        let max_step = q
            .iter()
            .step_by(2)
            .zip(q.iter().step_by(2).skip(1))
            .map(|(a, b)| (a - b).abs())
            .fold(0f32, f32::max);
        assert!(max_step < 0.03, "step {max_step}");
    }

    #[test]
    fn backlog_is_capped_and_discard_empties() {
        let mut m = Mixer::default();
        let feed = m.feed();
        feed.push(&vec![0.5; 48_000 * 2 * 2], 2, SAMPLE_RATE);
        let queued = feed.0.lock().unwrap().queue.len() / CHANNELS;
        assert_eq!(queued as u64, frames(MAX_BACKLOG));
        m.discard();
        assert!(feed.0.lock().unwrap().queue.is_empty());
    }

    #[test]
    fn live_mix_stays_behind_and_waits_for_enough() {
        let mut m = Mixer::default();
        m.feed();
        assert!(m.take_live(Duration::from_millis(200)).is_none());
        let (mix, at) = m.take_live(Duration::from_millis(500)).unwrap();
        assert_eq!(at, Duration::ZERO);
        assert_eq!(mix.len() as u64, frames(Duration::from_millis(250)) * 2);
        assert!(m.take_live(Duration::from_millis(510)).is_none());
    }

    struct Failing;
    impl AudioSource for Failing {
        fn name(&self) -> String {
            "nothing".into()
        }
        fn start(self: Box<Self>, _: AudioFeed) -> Result<Box<dyn Any>> {
            Err(crate::RecordError::Unsupported("no device".into()))
        }
    }

    #[test]
    fn sources_that_fail_are_reported() {
        let mut m = Mixer::default();
        let (running, problems) = start(vec![Box::new(Failing)], &mut m);
        assert!(running.is_none());
        assert_eq!(problems, vec!["nothing: no device".to_owned()]);
    }
}
