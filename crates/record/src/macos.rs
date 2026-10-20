//! MP4 on macOS: AVAssetWriter with VideoToolbox's H.264 encoder (the media engine on Apple
//! silicon and on Intel Macs with a T2 or recent GPU). Frames go in as BGRA pixel buffers
//! from the adaptor's pool; VideoToolbox converts them to YUV on the GPU. Sound goes in as
//! 48 kHz float PCM sample buffers on a second input, which encodes it to AAC.

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_av_foundation::{
    AVAssetWriter, AVAssetWriterInput, AVAssetWriterInputPixelBufferAdaptor, AVAssetWriterStatus,
    AVFileTypeMPEG4, AVMediaTypeAudio, AVMediaTypeVideo, AVVideoAverageBitRateKey, AVVideoCodecKey,
    AVVideoCodecTypeH264, AVVideoCompressionPropertiesKey, AVVideoExpectedSourceFrameRateKey,
    AVVideoHeightKey, AVVideoProfileLevelH264HighAutoLevel, AVVideoProfileLevelKey,
    AVVideoWidthKey,
};
use objc2_core_audio_types::{
    AudioStreamBasicDescription, kAudioFormatFlagIsFloat, kAudioFormatFlagIsPacked,
    kAudioFormatLinearPCM, kAudioFormatMPEG4AAC,
};
use objc2_core_foundation::{CFRetained, CFString};
use objc2_core_media::{
    CMAudioFormatDescriptionCreate, CMAudioSampleBufferCreateReadyWithPacketDescriptions,
    CMBlockBuffer, CMFormatDescription, CMSampleBuffer, CMTime, kCMBlockBufferAssureMemoryNowFlag,
};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferPool,
    CVPixelBufferUnlockBaseAddress, kCVPixelBufferHeightKey, kCVPixelBufferPixelFormatTypeKey,
    kCVPixelBufferWidthKey, kCVPixelFormatType_32BGRA, kCVReturnSuccess,
};
use objc2_foundation::{NSDictionary, NSNumber, NSString, NSURL};
use snapwright_capture::Frame;

use crate::audio::{CHANNELS, SAMPLE_RATE};
use crate::encoder::{AUDIO_BITRATE, Encoder, Video};
use crate::{RecordError, Result};

/// How long `finishWriting` may take before the recording is reported as failed.
const FINISH_TIMEOUT: Duration = Duration::from_secs(30);
/// How long to wait for the input to accept a frame before dropping it.
const READY_TIMEOUT: Duration = Duration::from_millis(100);

fn fail(api: &'static str, message: impl Into<String>) -> RecordError {
    RecordError::Encoder {
        api,
        message: message.into(),
    }
}

/// Microsecond timestamps.
fn cm_time(d: Duration) -> CMTime {
    // SAFETY: CMTimeMake only builds a struct.
    unsafe { CMTime::new(d.as_micros() as i64, 1_000_000) }
}

/// A CoreFoundation string constant as the NSString it is toll-free bridged to.
fn ns(s: &CFString) -> &NSString {
    // SAFETY: CFString and NSString are toll-free bridged: the same object under two names.
    unsafe { &*(s as *const CFString).cast::<NSString>() }
}

/// A framework string constant, which the bindings expose as optional.
fn constant<T: ?Sized>(c: Option<&'static T>, name: &'static str) -> Result<&'static T> {
    c.ok_or_else(|| fail(name, "missing from AVFoundation"))
}

/// The sound input and the format of the PCM handed to it.
struct Sound {
    input: Retained<AVAssetWriterInput>,
    format: CFRetained<CMFormatDescription>,
}

pub(crate) struct AvEncoder {
    writer: Retained<AVAssetWriter>,
    input: Retained<AVAssetWriterInput>,
    adaptor: Retained<AVAssetWriterInputPixelBufferAdaptor>,
    sound: Option<Sound>,
    width: u32,
    height: u32,
    path: PathBuf,
}

/// AVFAudio's settings keys. They live in another framework; their values are their names.
fn audio_key(name: &str) -> Retained<NSString> {
    NSString::from_str(name)
}

/// An AAC input for 48 kHz stereo, and the format of the float PCM it is fed.
///
/// # Safety
/// `writer` must not have started writing yet.
unsafe fn sound_input(writer: &AVAssetWriter) -> Result<Sound> {
    let settings = NSDictionary::<NSString, AnyObject>::from_slices(
        &[
            &*audio_key("AVFormatIDKey"),
            &*audio_key("AVSampleRateKey"),
            &*audio_key("AVNumberOfChannelsKey"),
            &*audio_key("AVEncoderBitRateKey"),
        ],
        &[
            &NSNumber::new_u32(kAudioFormatMPEG4AAC),
            &NSNumber::new_f64(f64::from(SAMPLE_RATE)),
            &NSNumber::new_u32(CHANNELS as u32),
            &NSNumber::new_u32(AUDIO_BITRATE),
        ],
    );
    let bytes_per_frame = (CHANNELS * size_of::<f32>()) as u32;
    let asbd = AudioStreamBasicDescription {
        mSampleRate: f64::from(SAMPLE_RATE),
        mFormatID: kAudioFormatLinearPCM,
        mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
        mBytesPerPacket: bytes_per_frame,
        mFramesPerPacket: 1,
        mBytesPerFrame: bytes_per_frame,
        mChannelsPerFrame: CHANNELS as u32,
        mBitsPerChannel: 32,
        mReserved: 0,
    };
    // SAFETY: the settings hold the value types AVFoundation documents for each key; the
    // ASBD is copied by CMAudioFormatDescriptionCreate, and the out pointer is checked.
    unsafe {
        let media_type = constant(AVMediaTypeAudio, "AVMediaTypeAudio")?;
        let input = AVAssetWriterInput::assetWriterInputWithMediaType_outputSettings(
            media_type,
            Some(&settings),
        );
        input.setExpectsMediaDataInRealTime(true);
        if !writer.canAddInput(&input) {
            return Err(fail("AVAssetWriter", "cannot add an AAC audio input"));
        }
        writer.addInput(&input);
        let mut out: *const CMFormatDescription = std::ptr::null();
        let status = CMAudioFormatDescriptionCreate(
            None,
            NonNull::from(&asbd),
            0,
            std::ptr::null(),
            0,
            std::ptr::null(),
            None,
            NonNull::from(&mut out),
        );
        let format = NonNull::new(out.cast_mut())
            .filter(|_| status == 0)
            .ok_or_else(|| fail("CMAudioFormatDescriptionCreate", status.to_string()))?;
        Ok(Sound {
            input,
            format: CFRetained::from_raw(format),
        })
    }
}

/// Waits briefly for `input` to take more data. `false` means it is still busy and the data
/// should be dropped, as a live source would.
fn wait_ready(writer: &AVAssetWriter, input: &AVAssetWriterInput) -> Result<bool> {
    let deadline = Instant::now() + READY_TIMEOUT;
    // SAFETY: plain getter.
    while !unsafe { input.isReadyForMoreMediaData() } {
        // SAFETY: plain getter.
        if unsafe { writer.status() } == AVAssetWriterStatus::Failed {
            return Err(writer_error(writer, "AVAssetWriterInput"));
        }
        if Instant::now() > deadline {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    Ok(true)
}

impl AvEncoder {
    pub(crate) fn create(path: &Path, video: Video) -> Result<Self> {
        let Video {
            width, height, fps, ..
        } = video;
        let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
        // SAFETY: framework constants read once; the dictionaries hold the value types
        // AVFoundation documents for each key.
        unsafe {
            let file_type = constant(AVFileTypeMPEG4, "AVFileTypeMPEG4")?;
            let writer = AVAssetWriter::assetWriterWithURL_fileType_error(&url, file_type)
                .map_err(|e| fail("AVAssetWriter", e.localizedDescription().to_string()))?;
            // The index before the data, so the file plays while it downloads.
            writer.setShouldOptimizeForNetworkUse(true);

            let compression = NSDictionary::<NSString, AnyObject>::from_slices(
                &[
                    constant(AVVideoAverageBitRateKey, "AVVideoAverageBitRateKey")?,
                    constant(
                        AVVideoExpectedSourceFrameRateKey,
                        "AVVideoExpectedSourceFrameRateKey",
                    )?,
                    constant(AVVideoProfileLevelKey, "AVVideoProfileLevelKey")?,
                ],
                &[
                    &NSNumber::new_u32(video.bitrate()),
                    &NSNumber::new_u32(fps),
                    constant(
                        AVVideoProfileLevelH264HighAutoLevel,
                        "AVVideoProfileLevelH264HighAutoLevel",
                    )?,
                ],
            );
            let codec: &AnyObject = constant(AVVideoCodecTypeH264, "AVVideoCodecTypeH264")?;
            let settings = NSDictionary::<NSString, AnyObject>::from_slices(
                &[
                    constant(AVVideoCodecKey, "AVVideoCodecKey")?,
                    constant(AVVideoWidthKey, "AVVideoWidthKey")?,
                    constant(AVVideoHeightKey, "AVVideoHeightKey")?,
                    constant(
                        AVVideoCompressionPropertiesKey,
                        "AVVideoCompressionPropertiesKey",
                    )?,
                ],
                &[
                    codec,
                    &NSNumber::new_u32(width),
                    &NSNumber::new_u32(height),
                    &compression,
                ],
            );
            let media_type = constant(AVMediaTypeVideo, "AVMediaTypeVideo")?;
            let input = AVAssetWriterInput::assetWriterInputWithMediaType_outputSettings(
                media_type,
                Some(&settings),
            );
            input.setExpectsMediaDataInRealTime(true);

            let attributes = NSDictionary::<NSString, AnyObject>::from_slices(
                &[
                    ns(kCVPixelBufferPixelFormatTypeKey),
                    ns(kCVPixelBufferWidthKey),
                    ns(kCVPixelBufferHeightKey),
                ],
                &[
                    &NSNumber::new_u32(kCVPixelFormatType_32BGRA),
                    &NSNumber::new_u32(width),
                    &NSNumber::new_u32(height),
                ],
            );
            let adaptor = AVAssetWriterInputPixelBufferAdaptor::assetWriterInputPixelBufferAdaptorWithAssetWriterInput_sourcePixelBufferAttributes(
                &input,
                Some(&attributes),
            );
            if !writer.canAddInput(&input) {
                return Err(fail("AVAssetWriter", "cannot add an H.264 video input"));
            }
            writer.addInput(&input);
            let sound = if video.audio {
                Some(sound_input(&writer)?)
            } else {
                None
            };
            if !writer.startWriting() {
                return Err(writer_error(&writer, "startWriting"));
            }
            writer.startSessionAtSourceTime(cm_time(Duration::ZERO));
            Ok(Self {
                writer,
                input,
                adaptor,
                sound,
                width,
                height,
                path: path.to_path_buf(),
            })
        }
    }

    /// A pixel buffer from the adaptor's pool holding `frame`.
    fn pixel_buffer(
        &self,
        frame: &Frame,
    ) -> Result<objc2_core_foundation::CFRetained<CVPixelBuffer>> {
        // SAFETY: the pool exists once writing started; the new buffer is locked while its
        // memory is written and unlocked before it is returned. Rows are copied within the
        // buffer's bytes-per-row and the frame's own row length.
        unsafe {
            let pool = self
                .adaptor
                .pixelBufferPool()
                .ok_or_else(|| writer_error(&self.writer, "pixelBufferPool"))?;
            let mut out: *mut CVPixelBuffer = std::ptr::null_mut();
            let status =
                CVPixelBufferPool::create_pixel_buffer(None, &pool, NonNull::from(&mut out));
            let buffer = NonNull::new(out)
                .filter(|_| status == kCVReturnSuccess)
                .ok_or_else(|| fail("CVPixelBufferPoolCreatePixelBuffer", status.to_string()))?;
            let buffer = objc2_core_foundation::CFRetained::from_raw(buffer);
            if CVPixelBufferLockBaseAddress(&buffer, CVPixelBufferLockFlags(0)) != kCVReturnSuccess
            {
                return Err(fail("CVPixelBufferLockBaseAddress", "could not lock"));
            }
            let base = CVPixelBufferGetBaseAddress(&buffer).cast::<u8>();
            let stride = CVPixelBufferGetBytesPerRow(&buffer);
            let row = self.width as usize * 4;
            if !base.is_null() && stride >= row {
                for (y, src) in frame
                    .data
                    .chunks_exact(row)
                    .take(self.height as usize)
                    .enumerate()
                {
                    std::ptr::copy_nonoverlapping(src.as_ptr(), base.add(y * stride), row);
                }
            }
            CVPixelBufferUnlockBaseAddress(&buffer, CVPixelBufferLockFlags(0));
            if base.is_null() || stride < row {
                return Err(fail(
                    "CVPixelBufferGetBaseAddress",
                    "unexpected buffer layout",
                ));
            }
            Ok(buffer)
        }
    }
}

fn writer_error(writer: &AVAssetWriter, api: &'static str) -> RecordError {
    // SAFETY: plain getter.
    let message = unsafe { writer.error() }
        .map(|e| e.localizedDescription().to_string())
        .unwrap_or_else(|| "failed without an error".into());
    fail(api, message)
}

impl Encoder for AvEncoder {
    fn push(&mut self, frame: Frame, at: Duration) -> Result<()> {
        // In real-time mode the input may briefly refuse more data while the encoder catches
        // up; a frame that still cannot go in is dropped, as a live source would.
        if !wait_ready(&self.writer, &self.input)? {
            tracing::debug!("encoder busy; frame dropped");
            return Ok(());
        }
        let buffer = self.pixel_buffer(&frame)?;
        // SAFETY: the input is ready, the time is numeric and after the previous frame's.
        if unsafe {
            self.adaptor
                .appendPixelBuffer_withPresentationTime(&buffer, cm_time(at))
        } {
            Ok(())
        } else {
            Err(writer_error(&self.writer, "appendPixelBuffer"))
        }
    }

    fn push_audio(&mut self, samples: &[f32], at: Duration) -> Result<()> {
        let Some(sound) = &self.sound else {
            return Ok(());
        };
        let frames = samples.len() / CHANNELS;
        if frames == 0 {
            return Ok(());
        }
        if !wait_ready(&self.writer, &sound.input)? {
            tracing::debug!("audio encoder busy; sound dropped");
            return Ok(());
        }
        let len = std::mem::size_of_val(samples);
        // SAFETY: the block buffer is created with `len` bytes of its own memory and filled
        // from `samples`, which has exactly `len` bytes; the out pointers are checked before
        // they are wrapped, and the sample buffer describes `frames` packed stereo floats.
        unsafe {
            let mut block: *mut CMBlockBuffer = std::ptr::null_mut();
            let status = CMBlockBuffer::create_with_memory_block(
                None,
                std::ptr::null_mut(),
                len,
                None,
                std::ptr::null(),
                0,
                len,
                kCMBlockBufferAssureMemoryNowFlag,
                NonNull::from(&mut block),
            );
            let block = NonNull::new(block)
                .filter(|_| status == 0)
                .ok_or_else(|| fail("CMBlockBufferCreateWithMemoryBlock", status.to_string()))?;
            let block = CFRetained::from_raw(block);
            let source = NonNull::new(samples.as_ptr().cast_mut().cast::<c_void>())
                .ok_or_else(|| fail("CMBlockBufferReplaceDataBytes", "no samples"))?;
            let status = CMBlockBuffer::replace_data_bytes(source, &block, 0, len);
            if status != 0 {
                return Err(fail("CMBlockBufferReplaceDataBytes", status.to_string()));
            }
            let mut sample: *mut CMSampleBuffer = std::ptr::null_mut();
            let status = CMAudioSampleBufferCreateReadyWithPacketDescriptions(
                None,
                &block,
                &sound.format,
                frames as _,
                CMTime::new(crate::audio::frames(at) as i64, SAMPLE_RATE as i32),
                std::ptr::null(),
                NonNull::from(&mut sample),
            );
            let sample = NonNull::new(sample)
                .filter(|_| status == 0)
                .ok_or_else(|| {
                    fail(
                        "CMAudioSampleBufferCreateReadyWithPacketDescriptions",
                        status.to_string(),
                    )
                })?;
            let sample = CFRetained::from_raw(sample);
            if sound.input.appendSampleBuffer(&sample) {
                Ok(())
            } else {
                Err(writer_error(&self.writer, "appendSampleBuffer (audio)"))
            }
        }
    }

    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn finish(self: Box<Self>, end: Duration) -> Result<()> {
        let (tx, rx) = mpsc::sync_channel(1);
        let done = RcBlock::new(move || {
            let _ = tx.send(());
        });
        // SAFETY: the session started at zero and `end` follows every frame; the completion
        // block only signals a channel, so it is fine to run on any queue.
        unsafe {
            self.input.markAsFinished();
            if let Some(sound) = &self.sound {
                sound.input.markAsFinished();
            }
            self.writer.endSessionAtSourceTime(cm_time(end));
            self.writer.finishWritingWithCompletionHandler(&done);
        }
        rx.recv_timeout(FINISH_TIMEOUT)
            .map_err(|_| fail("finishWriting", "timed out"))?;
        // SAFETY: plain getter.
        if unsafe { self.writer.status() } == AVAssetWriterStatus::Completed {
            Ok(())
        } else {
            Err(RecordError::Encoder {
                api: "finishWriting",
                message: format!(
                    "{}: {}",
                    self.path.display(),
                    writer_error(&self.writer, "finishWriting")
                ),
            })
        }
    }
}
