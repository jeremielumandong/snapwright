#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use super::*;

/// A synthetic screen: a grey background with a square moving one step per frame.
struct Moving {
    width: u32,
    height: u32,
    n: u32,
}

impl FrameSource for Moving {
    fn next_frame(&mut self) -> Result<Frame> {
        let (w, h) = (self.width, self.height);
        let mut data = [60u8, 60, 60, 255].repeat((w * h) as usize);
        let side = (h / 4).max(2).min(w);
        let x0 = (self.n * 4) % (w - side + 1);
        for y in h / 3..(h / 3 + side).min(h) {
            for x in x0..x0 + side {
                let i = ((y * w + x) * 4) as usize;
                data[i..i + 4].copy_from_slice(&[40, 120, 240, 255]);
            }
        }
        self.n += 1;
        Ok(Frame {
            display_id: 0,
            width: w,
            height: h,
            data,
        })
    }
}

fn record_for(
    format: Format,
    size: (u32, u32),
    seconds: f32,
) -> Result<(Summary, tempfile::TempDir)> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(format!("clip.{}", format.extension()));
    let source = Moving {
        width: size.0,
        height: size.1,
        n: 0,
    };
    let recorder = Recorder::start(source, Options::new(format), &path)?;
    std::thread::sleep(Duration::from_secs_f32(seconds));
    Ok((recorder.stop()?, dir))
}

/// MP4 boxes at the top level of the file, in order.
fn top_level_boxes(bytes: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at + 8 <= bytes.len() {
        let size = u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
        out.push(String::from_utf8_lossy(&bytes[at + 4..at + 8]).into_owned());
        if size == 1 && at + 16 > bytes.len() {
            break;
        }
        let size = match size {
            1 => u64::from_be_bytes(bytes[at + 8..at + 16].try_into().unwrap()) as usize,
            0 => bytes.len() - at,
            s => s,
        };
        if size < 8 {
            break;
        }
        at += size;
    }
    out
}

#[test]
fn mp4_has_its_index_first_and_the_right_size() {
    // An odd size checks the even-size rounding H.264 needs.
    let (summary, _dir) = match record_for(Format::Mp4, (321, 241), 1.2) {
        Err(RecordError::Unsupported(why)) if cfg!(target_os = "linux") => {
            eprintln!("skipped: {why}");
            return;
        }
        other => other.expect("record"),
    };
    assert_eq!((summary.width, summary.height), (320, 240));
    assert_eq!((summary.poster.width, summary.poster.height), (321, 241));
    // The capture side keeps its pace; the encoder may shed frames while it warms up (CI's
    // macOS VMs have no hardware encoder), but most of them must reach the file.
    let (frames, dropped) = (summary.frames, summary.dropped);
    assert!(frames + dropped >= 18, "{frames} frames, {dropped} dropped");
    assert!(frames >= 8, "{frames} frames, {dropped} dropped");
    assert!(summary.duration >= Duration::from_secs(1));
    let bytes = std::fs::read(&summary.path).unwrap();
    let boxes = top_level_boxes(&bytes);
    let layout = format!(
        "{boxes:?} in {} bytes, starting {:02x?}",
        bytes.len(),
        &bytes[..bytes.len().min(48)]
    );
    let moov = boxes.iter().position(|b| b == "moov");
    let mdat = boxes.iter().position(|b| b == "mdat");
    let (Some(moov), Some(mdat)) = (moov, mdat) else {
        panic!("no moov or mdat box: {layout}");
    };
    assert_eq!(boxes.first().map(String::as_str), Some("ftyp"), "{layout}");
    assert!(moov < mdat, "index after the data: {boxes:?}");
    // The track says H.264.
    assert!(
        bytes.windows(4).any(|w| w == b"avc1"),
        "no avc1 sample entry"
    );
    if let Some(probe) = ffprobe(&summary.path) {
        assert!(probe.contains("codec_name=h264"), "{probe}");
        assert!(
            probe.contains("width=320") && probe.contains("height=240"),
            "{probe}"
        );
        let duration: f64 = probe
            .lines()
            .find_map(|l| l.strip_prefix("duration="))
            .and_then(|d| d.parse().ok())
            .expect("duration");
        assert!((1.0..2.0).contains(&duration), "duration {duration}");
    }
}

/// `ffprobe`'s view of the video stream, when ffprobe is installed.
fn ffprobe(path: &std::path::Path) -> Option<String> {
    let out = std::process::Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0"])
        .args(["-show_entries", "stream=codec_name,width,height,duration"])
        .args(["-of", "default=noprint_wrappers=1"])
        .arg(path)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

#[test]
fn gif_loops_keeps_time_and_writes_only_changes() {
    let (summary, _dir) = record_for(Format::Gif, (200, 120), 1.0).expect("record");
    let file = std::fs::File::open(&summary.path).unwrap();
    let mut options = ::gif::DecodeOptions::new();
    options.set_color_output(::gif::ColorOutput::Indexed);
    let mut decoder = options.read_info(file).unwrap();
    assert_eq!((decoder.width(), decoder.height()), (200, 120));
    let mut frames = 0;
    let mut centis = 0u32;
    let mut partial = 0;
    while let Some(frame) = decoder.read_next_frame().unwrap() {
        frames += 1;
        centis += u32::from(frame.delay);
        if frame.width < 200 || frame.height < 120 {
            partial += 1;
        }
    }
    assert!(frames >= 8, "{frames} frames");
    assert_eq!(
        partial,
        frames - 1,
        "every frame after the first is a changed rectangle"
    );
    let seconds = centis as f32 / 100.0;
    assert!((0.9..1.6).contains(&seconds), "{seconds} s");
}

#[test]
fn gif_scales_wide_frames_down() {
    let (summary, _dir) = record_for(Format::Gif, (2600, 100), 0.3).expect("record");
    // 2600 wide needs a factor of 3 to fit 1280.
    assert_eq!((summary.width, summary.height), (866, 33));
}

#[test]
fn pause_leaves_time_out() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("paused.gif");
    let source = Moving {
        width: 64,
        height: 64,
        n: 0,
    };
    // Sleeps overshoot on busy CI runners (macOS by hundreds of ms), so the expected
    // length comes from the measured windows, not the requested sleeps.
    let before_start = Instant::now();
    let recorder = Recorder::start(source, Options::new(Format::Gif), &path).expect("start");
    let after_start = Instant::now();
    std::thread::sleep(Duration::from_millis(300));
    let before_pause = Instant::now();
    recorder.pause();
    let after_pause = Instant::now();
    assert!(recorder.is_paused());
    std::thread::sleep(Duration::from_millis(200));
    let during = recorder.elapsed();
    std::thread::sleep(Duration::from_millis(400));
    assert!(recorder.elapsed() - during < Duration::from_millis(100));
    let before_resume = Instant::now();
    recorder.resume();
    let after_resume = Instant::now();
    std::thread::sleep(Duration::from_millis(300));
    let before_stop = Instant::now();
    let summary = recorder.stop().expect("stop");
    let after_stop = Instant::now();
    assert!(after_stop - before_stop < Duration::from_secs(5));

    // Pause and resume take effect on the next tick, and the length ends one frame
    // after the last tick, so allow a few frame intervals (15 fps GIF) either way.
    let slack = Duration::from_millis(250);
    let shortest = (before_pause - after_start) + (before_stop - after_resume);
    let longest = (after_pause - before_start) + (after_stop - before_resume);
    let paused = before_resume - after_pause;
    let d = summary.duration;
    assert!(
        d + slack >= shortest && d <= longest + slack,
        "recorded {d:?}, expected {shortest:?}..{longest:?} with {paused:?} paused"
    );
    // The pause window is far wider than the slack, so a recorder that kept counting
    // through it would fail the upper bound.
    assert!(paused >= Duration::from_millis(600) && paused > slack * 2);
}

#[test]
fn a_source_failing_at_start_is_reported() {
    struct Broken;
    impl FrameSource for Broken {
        fn next_frame(&mut self) -> Result<Frame> {
            Err(RecordError::Unsupported("no screen".into()))
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let err = Recorder::start(Broken, Options::default(), dir.path().join("x.mp4"))
        .err()
        .expect("fails");
    assert!(
        matches!(err, RecordError::Unsupported(ref m) if m == "no screen"),
        "{err}"
    );
}

#[test]
fn a_one_pixel_region_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let source = Moving {
        width: 1,
        height: 40,
        n: 0,
    };
    let err = Recorder::start(source, Options::default(), dir.path().join("x.mp4"))
        .err()
        .expect("fails");
    assert!(matches!(err, RecordError::EmptyRegion(_)), "{err}");
}

#[test]
fn formats_from_paths() {
    use std::path::Path;
    assert_eq!(Format::from_path(Path::new("a.MP4")), Some(Format::Mp4));
    assert_eq!(Format::from_path(Path::new("a.gif")), Some(Format::Gif));
    assert_eq!(Format::from_path(Path::new("a.mov")), None);
}

#[test]
fn bitrate_scales_with_pixels_and_quality() {
    use Quality::*;
    assert_eq!(encoder::bitrate(1920, 1080, 30, Standard), 6_220_800);
    assert_eq!(encoder::bitrate(1920, 1080, 60, High), 18_662_400);
    assert_eq!(encoder::bitrate(1920, 1080, 60, Maximum), 37_324_800);
    assert_eq!(encoder::bitrate(100, 100, 30, High), 1_000_000);
    // 4K at 60 fps stays within what H.264 hardware takes.
    assert_eq!(encoder::bitrate(3840, 2160, 60, Maximum), 149_299_200);
    assert_eq!(encoder::bitrate(7680, 4320, 60, Maximum), 160_000_000);
    assert_eq!(Quality::from_key("MAXIMUM"), Some(Maximum));
    assert_eq!(Options::new(Format::Mp4).fps, 60);
}

#[test]
fn display_fps_follows_the_refresh_rate() {
    let mut d = Display {
        id: 0,
        name: "gaming".into(),
        bounds: Rect::new(0, 0, 2560, 1440),
        scale: 1.0,
        primary: true,
        refresh_hz: Some(144),
    };
    assert_eq!(display_fps(&d), 144);
    d.refresh_hz = Some(500);
    assert_eq!(display_fps(&d), MAX_FPS);
    d.refresh_hz = None;
    assert_eq!(display_fps(&d), 60);
}

/// A 440 Hz tone at 44.1 kHz mono, pushed in 10 ms pieces like a device would.
struct Tone;

struct ToneGuard {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for ToneGuard {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            t.join().unwrap();
        }
    }
}

impl AudioSource for Tone {
    fn name(&self) -> String {
        "tone".into()
    }

    fn start(self: Box<Self>, feed: AudioFeed) -> Result<Box<dyn std::any::Any>> {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::spawn(move || {
            let mut n = 0u64;
            let started = Instant::now();
            while !flag.load(std::sync::atomic::Ordering::Relaxed) {
                // Everything due by now, so the tone keeps real time.
                let due = (started.elapsed().as_secs_f64() * 44_100.0) as u64;
                let chunk: Vec<f32> = (n..due)
                    .map(|i| (i as f32 * 440.0 * std::f32::consts::TAU / 44_100.0).sin() * 0.5)
                    .collect();
                n = due;
                feed.push(&chunk, 1, 44_100);
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        Ok(Box::new(ToneGuard {
            stop,
            thread: Some(thread),
        }))
    }
}

/// `ffprobe`'s view of the first sound stream, when ffprobe is installed.
fn ffprobe_audio(path: &std::path::Path) -> Option<String> {
    let out = std::process::Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "a:0"])
        .args([
            "-show_entries",
            "stream=codec_name,sample_rate,channels,duration",
        ])
        .args(["-of", "default=noprint_wrappers=1"])
        .arg(path)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The sound's mean volume in dB, when ffmpeg is installed.
fn mean_volume(path: &std::path::Path) -> Option<f64> {
    let out = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-nostdin", "-i"])
        .arg(path)
        .args(["-map", "0:a", "-af", "volumedetect", "-f", "null", "-"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stderr)
        .lines()
        .find_map(|l| l.split("mean_volume:").nth(1))
        .and_then(|v| v.trim().trim_end_matches("dB").trim().parse().ok())
}

#[test]
fn mp4_with_sound_has_an_aac_track_that_keeps_time() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sound.mp4");
    let source = Moving {
        width: 320,
        height: 240,
        n: 0,
    };
    let recorder = match Recorder::start_with_audio(
        source,
        vec![Box::new(Tone)],
        Options::new(Format::Mp4),
        &path,
    ) {
        Err(RecordError::Unsupported(why)) if cfg!(target_os = "linux") => {
            eprintln!("skipped: {why}");
            return;
        }
        other => other.expect("record"),
    };
    std::thread::sleep(Duration::from_millis(700));
    recorder.pause();
    std::thread::sleep(Duration::from_millis(400));
    recorder.resume();
    std::thread::sleep(Duration::from_millis(700));
    let summary = recorder.stop().expect("stop");
    assert!(summary.audio, "{:?}", summary.audio_problems);
    assert!(summary.audio_problems.is_empty());
    let bytes = std::fs::read(&summary.path).unwrap();
    assert!(
        bytes.windows(4).any(|w| w == b"mp4a"),
        "no AAC sample entry"
    );
    let boxes = top_level_boxes(&bytes);
    let moov = boxes.iter().position(|b| b == "moov");
    let mdat = boxes.iter().position(|b| b == "mdat");
    assert!(moov < mdat, "index after the data: {boxes:?}");
    // No parts left beside the file.
    let names: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, vec![std::ffi::OsString::from("sound.mp4")]);
    if let Some(probe) = ffprobe_audio(&summary.path) {
        assert!(probe.contains("codec_name=aac"), "{probe}");
        assert!(probe.contains("sample_rate=48000"), "{probe}");
        assert!(probe.contains("channels=2"), "{probe}");
        let duration: f64 = probe
            .lines()
            .find_map(|l| l.strip_prefix("duration="))
            .and_then(|d| d.parse().ok())
            .expect("duration");
        // The pause is left out of the sound as it is of the video.
        let video = summary.duration.as_secs_f64();
        assert!(
            (duration - video).abs() < 0.2,
            "sound {duration} s, video {video} s"
        );
    }
    if let Some(volume) = mean_volume(&summary.path) {
        // A 0.5 sine is about -9 dB; silence would be -91.
        assert!(volume > -20.0, "mean volume {volume} dB");
    }
}

#[test]
fn gifs_ignore_sound() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("quiet.gif");
    let source = Moving {
        width: 64,
        height: 48,
        n: 0,
    };
    let recorder = Recorder::start_with_audio(
        source,
        vec![Box::new(Tone)],
        Options::new(Format::Gif),
        &path,
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(200));
    let summary = recorder.stop().unwrap();
    assert!(!summary.audio);
}
