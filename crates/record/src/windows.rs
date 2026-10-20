//! MP4 on Windows: a Media Foundation sink writer with the system H.264 encoder, which is
//! the GPU's (Intel Quick Sync, NVENC, AMD VCE) when one is present and Microsoft's software
//! encoder otherwise. Frames go in as NV12, converted here, so no colour converter is needed
//! in the pipeline and the colours are BT.709 like the other platforms'. Sound goes to a
//! second stream as 16-bit PCM, which the system AAC encoder compresses.

use std::path::{Path, PathBuf};
use std::time::Duration;

use snapwright_capture::Frame;
use windows::Win32::Media::MediaFoundation::{
    IMFAttributes, IMFMediaType, IMFSinkWriter, MF_MT_AUDIO_AVG_BYTES_PER_SECOND,
    MF_MT_AUDIO_BITS_PER_SAMPLE, MF_MT_AUDIO_BLOCK_ALIGNMENT, MF_MT_AUDIO_NUM_CHANNELS,
    MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_AVG_BITRATE, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE,
    MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MPEG2_PROFILE,
    MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_YUV_MATRIX,
    MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, MF_VERSION, MFAudioFormat_AAC, MFAudioFormat_PCM,
    MFCreateAttributes, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample,
    MFCreateSinkWriterFromURL, MFMediaType_Audio, MFMediaType_Video, MFNominalRange_16_235,
    MFSTARTUP_FULL, MFShutdown, MFStartup, MFVideoFormat_H264, MFVideoFormat_NV12,
    MFVideoInterlace_Progressive, MFVideoTransferMatrix_BT709, eAVEncH264VProfile_High,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::core::HSTRING;

use crate::audio::{CHANNELS, SAMPLE_RATE};
use crate::encoder::{AUDIO_BITRATE, Encoder, Video};
use crate::{RecordError, Result};

fn err(api: &'static str, e: windows::core::Error) -> RecordError {
    RecordError::Encoder {
        api,
        message: e.to_string(),
    }
}

/// 100-nanosecond units, Media Foundation's clock.
fn hns(d: Duration) -> i64 {
    (d.as_nanos() / 100) as i64
}

/// Undoes `CoInitializeEx` and `MFStartup` on the encoder thread, last.
struct Runtime;

impl Drop for Runtime {
    fn drop(&mut self) {
        // SAFETY: paired with the successful MFStartup and CoInitializeEx in `create`, on the
        // same thread; every Media Foundation object is released before this runs.
        unsafe {
            let _ = MFShutdown();
            CoUninitialize();
        }
    }
}

pub(crate) struct MfEncoder {
    // Field order is drop order: the writer goes before the runtime shuts down.
    writer: IMFSinkWriter,
    stream: u32,
    /// The sound stream, when there is one.
    sound: Option<u32>,
    pcm: Vec<u8>,
    width: u32,
    height: u32,
    path: PathBuf,
    nv12: Vec<u8>,
    /// The last frame's time, written when the next frame (or the end) gives its duration.
    pending: Option<(Vec<u8>, Duration)>,
    _runtime: Runtime,
}

impl MfEncoder {
    pub(crate) fn create(path: &Path, video: Video) -> Result<Self> {
        let Video {
            width, height, fps, ..
        } = video;
        // SAFETY: COM and Media Foundation start-up for this thread, undone by `Runtime`.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .ok()
            .map_err(|e| err("CoInitializeEx", e))?;
        // SAFETY: as above.
        if let Err(e) = unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) } {
            // SAFETY: undoes the CoInitializeEx above.
            unsafe { CoUninitialize() };
            return Err(err("MFStartup", e));
        }
        let runtime = Runtime;
        let size = (u64::from(width) << 32) | u64::from(height);
        let rate = (u64::from(fps) << 32) | 1;
        // SAFETY: plain Media Foundation object creation and attribute setters on objects
        // this function owns; GUID pointers are to statics.
        let (writer, stream, sound) = unsafe {
            let mut attrs: Option<IMFAttributes> = None;
            MFCreateAttributes(&mut attrs, 2).map_err(|e| err("MFCreateAttributes", e))?;
            let attrs = attrs.ok_or_else(|| RecordError::Encoder {
                api: "MFCreateAttributes",
                message: "no attributes".into(),
            })?;
            attrs
                .SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)
                .map_err(|e| err("SetUINT32", e))?;
            let url = HSTRING::from(path.as_os_str());
            let writer = MFCreateSinkWriterFromURL(&url, None, &attrs)
                .map_err(|e| err("MFCreateSinkWriterFromURL", e))?;

            let out = MFCreateMediaType().map_err(|e| err("MFCreateMediaType", e))?;
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(|e| err("SetGUID", e))?;
            out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)
                .map_err(|e| err("SetGUID", e))?;
            out.SetUINT32(&MF_MT_AVG_BITRATE, video.bitrate())
                .map_err(|e| err("SetUINT32", e))?;
            out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)
                .map_err(|e| err("SetUINT32", e))?;
            set_video(&out, size, rate)?;
            let stream = writer
                .AddStream(&out)
                .map_err(|e| err("IMFSinkWriter::AddStream", e))?;

            let input = MFCreateMediaType().map_err(|e| err("MFCreateMediaType", e))?;
            input
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(|e| err("SetGUID", e))?;
            input
                .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)
                .map_err(|e| err("SetGUID", e))?;
            input
                .SetUINT32(&MF_MT_DEFAULT_STRIDE, width)
                .map_err(|e| err("SetUINT32", e))?;
            set_video(&input, size, rate)?;
            writer
                .SetInputMediaType(stream, &input, None)
                .map_err(|e| err("IMFSinkWriter::SetInputMediaType", e))?;
            let sound = if video.audio {
                Some(add_sound(&writer)?)
            } else {
                None
            };
            writer
                .BeginWriting()
                .map_err(|e| err("IMFSinkWriter::BeginWriting", e))?;
            (writer, stream, sound)
        };
        Ok(Self {
            writer,
            stream,
            sound,
            pcm: Vec::new(),
            width,
            height,
            path: path.to_path_buf(),
            nv12: Vec::new(),
            pending: None,
            _runtime: runtime,
        })
    }

    /// Writes one NV12 picture shown from `at` for `duration`.
    fn write(&self, nv12: &[u8], at: Duration, duration: Duration) -> Result<()> {
        self.write_sample(self.stream, nv12, at, duration)
    }

    /// Writes `bytes` to `stream` as one sample shown from `at` for `duration`.
    fn write_sample(
        &self,
        stream: u32,
        bytes: &[u8],
        at: Duration,
        duration: Duration,
    ) -> Result<()> {
        let len = bytes.len() as u32;
        // SAFETY: the buffer is created with `len` bytes, locked for the copy and unlocked
        // before it is handed to the sample; the pointer is only used while locked.
        unsafe {
            let buffer = MFCreateMemoryBuffer(len).map_err(|e| err("MFCreateMemoryBuffer", e))?;
            let mut data = std::ptr::null_mut();
            buffer
                .Lock(&mut data, None, None)
                .map_err(|e| err("IMFMediaBuffer::Lock", e))?;
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), data, bytes.len());
            buffer
                .Unlock()
                .map_err(|e| err("IMFMediaBuffer::Unlock", e))?;
            buffer
                .SetCurrentLength(len)
                .map_err(|e| err("IMFMediaBuffer::SetCurrentLength", e))?;
            let sample = MFCreateSample().map_err(|e| err("MFCreateSample", e))?;
            sample
                .AddBuffer(&buffer)
                .map_err(|e| err("IMFSample::AddBuffer", e))?;
            sample
                .SetSampleTime(hns(at))
                .map_err(|e| err("IMFSample::SetSampleTime", e))?;
            sample
                .SetSampleDuration(hns(duration).max(1))
                .map_err(|e| err("IMFSample::SetSampleDuration", e))?;
            self.writer
                .WriteSample(stream, &sample)
                .map_err(|e| err("IMFSinkWriter::WriteSample", e))
        }
    }
}

/// Adds an AAC stream fed with 16-bit PCM at 48 kHz stereo (the AAC encoder takes no
/// floats). Returns its index.
///
/// # Safety
/// `writer` must not have begun writing.
unsafe fn add_sound(writer: &IMFSinkWriter) -> Result<u32> {
    let block = (CHANNELS * 2) as u32;
    // SAFETY: plain Media Foundation object creation and attribute setters on objects this
    // function owns; GUID pointers are to statics.
    unsafe {
        let audio = |t: &IMFMediaType, subtype| -> Result<()> {
            t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)
                .map_err(|e| err("SetGUID", e))?;
            t.SetGUID(&MF_MT_SUBTYPE, subtype)
                .map_err(|e| err("SetGUID", e))?;
            t.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, SAMPLE_RATE)
                .map_err(|e| err("SetUINT32", e))?;
            t.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, CHANNELS as u32)
                .map_err(|e| err("SetUINT32", e))?;
            t.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)
                .map_err(|e| err("SetUINT32", e))
        };
        let out = MFCreateMediaType().map_err(|e| err("MFCreateMediaType", e))?;
        audio(&out, &MFAudioFormat_AAC)?;
        // The encoder takes 12000, 16000, 20000 or 24000 bytes a second.
        out.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, AUDIO_BITRATE / 8)
            .map_err(|e| err("SetUINT32", e))?;
        let stream = writer
            .AddStream(&out)
            .map_err(|e| err("IMFSinkWriter::AddStream (audio)", e))?;
        let input = MFCreateMediaType().map_err(|e| err("MFCreateMediaType", e))?;
        audio(&input, &MFAudioFormat_PCM)?;
        input
            .SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, block)
            .map_err(|e| err("SetUINT32", e))?;
        input
            .SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, SAMPLE_RATE * block)
            .map_err(|e| err("SetUINT32", e))?;
        writer
            .SetInputMediaType(stream, &input, None)
            .map_err(|e| err("IMFSinkWriter::SetInputMediaType (audio)", e))?;
        Ok(stream)
    }
}

/// Size, rate, progressive scan, square pixels and BT.709 limited range.
///
/// # Safety
/// `t` must be a live media type.
unsafe fn set_video(
    t: &windows::Win32::Media::MediaFoundation::IMFMediaType,
    size: u64,
    rate: u64,
) -> Result<()> {
    // SAFETY: attribute setters on a live media type; GUID pointers are to statics.
    unsafe {
        t.SetUINT64(&MF_MT_FRAME_SIZE, size)
            .map_err(|e| err("SetUINT64", e))?;
        t.SetUINT64(&MF_MT_FRAME_RATE, rate)
            .map_err(|e| err("SetUINT64", e))?;
        t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1 << 32) | 1)
            .map_err(|e| err("SetUINT64", e))?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
            .map_err(|e| err("SetUINT32", e))?;
        t.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)
            .map_err(|e| err("SetUINT32", e))?;
        t.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)
            .map_err(|e| err("SetUINT32", e))?;
    }
    Ok(())
}

impl Encoder for MfEncoder {
    fn push(&mut self, frame: Frame, at: Duration) -> Result<()> {
        let mut nv12 = std::mem::take(&mut self.nv12);
        crate::yuv::bgra_to_nv12(
            &frame.data,
            self.width as usize,
            self.height as usize,
            &mut nv12,
        );
        if let Some((prev, from)) = self.pending.take() {
            let written = self.write(&prev, from, at.saturating_sub(from));
            self.nv12 = prev;
            written?;
        }
        self.pending = Some((nv12, at));
        Ok(())
    }

    fn push_audio(&mut self, samples: &[f32], at: Duration) -> Result<()> {
        let Some(stream) = self.sound else {
            return Ok(());
        };
        let mut pcm = std::mem::take(&mut self.pcm);
        pcm.clear();
        pcm.extend(
            samples
                .iter()
                .flat_map(|s| ((s.clamp(-1.0, 1.0) * 32767.0).round() as i16).to_le_bytes()),
        );
        let frames = (samples.len() / CHANNELS) as u32;
        let duration = Duration::from_secs(u64::from(frames)) / SAMPLE_RATE;
        let written = self.write_sample(stream, &pcm, at, duration);
        self.pcm = pcm;
        written
    }

    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn finish(mut self: Box<Self>, end: Duration) -> Result<()> {
        if let Some((prev, from)) = self.pending.take() {
            self.write(&prev, from, end.saturating_sub(from))?;
        }
        // SAFETY: finalising a writer that began writing.
        unsafe { self.writer.Finalize() }.map_err(|e| RecordError::Encoder {
            api: "IMFSinkWriter::Finalize",
            message: format!("{}: {e}", self.path.display()),
        })?;
        // The index before the data, so the file plays while it downloads. Media Foundation's
        // own option for this (MF_MPEG4SINK_MOOV_BEFORE_MDAT) fails to finalise a file opened
        // by path, and through a read-write byte stream it drops the mdat header.
        // Release the writer and its file first, or Windows refuses to replace the file.
        let path = self.path.clone();
        drop(self);
        crate::faststart::faststart(&path)
    }
}
