//! Windows hardware H.264 transforms, opened only by an explicit video publication.
//! Input is bounded NV12; this path still uses CPU staging, not zero-copy capture.
use crate::capture::valid_video_dimensions;
use std::{collections::VecDeque, mem::ManuallyDrop, ptr};
use windows::{
    core::{Interface, VARIANT},
    Win32::{Media::MediaFoundation::*, System::Com::*},
};

const MAX_ACCESS_UNIT: usize = 4 * 1024 * 1024;
pub struct AccessUnit {
    pub bytes: Vec<u8>,
    pub timestamp_us: i64,
    pub key: bool,
}
struct PendingFrame {
    bytes: Vec<u8>,
    timestamp_us: i64,
}
struct Foundation;
impl Foundation {
    fn start() -> Result<Self, String> {
        // Each encoder stays on its creating audio/video owner thread.
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED)
                .ok()
                .map_err(|e| format!("Initialize encoder COM: {e}"))?;
            if let Err(error) = MFStartup(MF_VERSION, MFSTARTUP_FULL) {
                CoUninitialize();
                return Err(format!("Initialize Media Foundation: {error}"));
            }
        }
        Ok(Self)
    }
}
impl Drop for Foundation {
    fn drop(&mut self) {
        unsafe {
            let _ = MFShutdown();
            CoUninitialize();
        }
    }
}
#[derive(Default)]
struct OutputBuffer(MFT_OUTPUT_DATA_BUFFER);
impl Drop for OutputBuffer {
    fn drop(&mut self) {
        // COM replaces these owning fields; Windows bindings intentionally use
        // ManuallyDrop because their raw ABI structure has no implicit destructor.
        unsafe {
            drop(ManuallyDrop::take(&mut self.0.pSample));
            drop(ManuallyDrop::take(&mut self.0.pEvents));
        }
    }
}
pub struct HardwareEncoder {
    transform: IMFTransform,
    events: IMFMediaEventGenerator,
    activate: IMFActivate,
    codec: Option<ICodecAPI>,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    configured_bitrate: u32,
    need_input: u32,
    pending: Option<PendingFrame>,
    ready: VecDeque<AccessUnit>,
    // Drop COM objects before uninitializing their apartment and MF reference.
    _foundation: Foundation,
}
impl HardwareEncoder {
    pub fn new(width: u32, height: u32, fps: u32, bitrate: u32) -> Result<Self, String> {
        if width < 2
            || height < 2
            || !valid_video_dimensions(width, height)
            || !width.is_multiple_of(2)
            || !height.is_multiple_of(2)
            || !matches!(fps, 15 | 24 | 30 | 60)
        {
            return Err("Hardware H.264 requires an even, bounded video resolution".into());
        }
        let foundation = Foundation::start()?;
        let mut raw = ptr::null_mut();
        let mut count = 0;
        let output = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_H264,
        };
        unsafe {
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_ENCODER,
                MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
                None,
                Some(&output),
                &mut raw,
                &mut count,
            )
        }
        .map_err(|e| format!("Find hardware H.264 encoder: {e}"))?;
        let mut candidates = Vec::new();
        if !raw.is_null() {
            unsafe {
                for entry in std::slice::from_raw_parts_mut(raw, count as usize) {
                    if let Some(activate) = entry.take() {
                        candidates.push(activate);
                    }
                }
                CoTaskMemFree(Some(raw as _));
            }
        }
        let mut last_error = "No hardware H.264 encoder is installed".to_string();
        for activate in candidates {
            match Self::open(&activate, width, height, fps, bitrate) {
                Ok((transform, events, codec, name)) => {
                    return Ok(Self {
                        transform,
                        events,
                        activate,
                        codec,
                        name,
                        width,
                        height,
                        fps,
                        configured_bitrate: bitrate.clamp(100_000, 32_000_000),
                        need_input: 0,
                        pending: None,
                        ready: VecDeque::new(),
                        _foundation: foundation,
                    })
                }
                Err(error) => {
                    last_error = error;
                    unsafe {
                        let _ = activate.ShutdownObject();
                    }
                }
            }
        }
        Err(last_error)
    }
    fn open(
        activate: &IMFActivate,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
    ) -> Result<
        (
            IMFTransform,
            IMFMediaEventGenerator,
            Option<ICodecAPI>,
            String,
        ),
        String,
    > {
        unsafe {
            let transform: IMFTransform = activate
                .ActivateObject()
                .map_err(|e| format!("Activate hardware encoder: {e}"))?;
            let attributes = transform
                .GetAttributes()
                .map_err(|e| format!("Read encoder attributes: {e}"))?;
            attributes
                .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
                .map_err(|e| format!("Enable asynchronous hardware encoder: {e}"))?;
            let _ = attributes.SetUINT32(&MF_LOW_LATENCY, 1);
            let codec: Option<ICodecAPI> = transform.cast().ok();
            if let Some(codec) = &codec {
                let _ = codec.SetValue(&CODECAPI_AVEncCommonRealTime, &VARIANT::from(true));
                let _ = codec.SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(true));
                let _ = codec.SetValue(
                    &CODECAPI_AVEncMPVDefaultBPictureCount,
                    &VARIANT::from(0_u32),
                );
            }
            let output = media_type(MFVideoFormat_H264, width, height, fps)?;
            output
                .SetUINT32(&MF_MT_AVG_BITRATE, bitrate.clamp(100_000, 32_000_000))
                .map_err(|e| format!("Set video bitrate: {e}"))?;
            output
                .SetUINT32(&MF_MT_MPEG2_PROFILE, 66)
                .map_err(|e| format!("Set H.264 profile: {e}"))?;
            transform
                .SetOutputType(0, &output, 0)
                .map_err(|e| format!("Set hardware H.264 output: {e}"))?;
            let input = media_type(MFVideoFormat_NV12, width, height, fps)?;
            transform
                .SetInputType(0, &input, 0)
                .map_err(|e| format!("Set hardware NV12 input: {e}"))?;
            let events: IMFMediaEventGenerator = transform
                .cast()
                .map_err(|e| format!("Open hardware encoder events: {e}"))?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(|e| format!("Prepare hardware encoder: {e}"))?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(|e| format!("Start hardware encoder: {e}"))?;
            let mut text = windows::core::PWSTR::null();
            let mut length = 0;
            let name = if activate
                .GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut text, &mut length)
                .is_ok()
            {
                let name = text.to_string().unwrap_or_else(|_| "Hardware H.264".into());
                CoTaskMemFree(Some(text.0 as _));
                name
            } else {
                "Hardware H.264".into()
            };
            Ok((transform, events, codec, name))
        }
    }
    /// Keeps at most the newest unsubmitted frame; driver backpressure cannot grow RAM.
    pub fn submit_nv12(&mut self, bytes: Vec<u8>, timestamp_us: i64) -> Result<(), String> {
        if bytes.len() != self.width as usize * self.height as usize * 3 / 2 || timestamp_us < 0 {
            return Err("Invalid NV12 frame".into());
        }
        self.pending = Some(PendingFrame {
            bytes,
            timestamp_us,
        });
        self.pump()
    }
    pub fn poll(&mut self) -> Result<Vec<AccessUnit>, String> {
        self.pump()?;
        Ok(self.ready.drain(..).collect())
    }
    pub fn request_keyframe(&self) {
        if let Some(codec) = &self.codec {
            unsafe {
                let _ = codec.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(true));
            }
        }
    }
    pub fn set_bitrate(&mut self, bitrate: u64) {
        let bitrate = bitrate.clamp(100_000, 32_000_000) as u32;
        if self.configured_bitrate == bitrate {
            return;
        }
        if let Some(codec) = &self.codec {
            unsafe {
                if codec
                    .SetValue(&CODECAPI_AVEncCommonMeanBitRate, &VARIANT::from(bitrate))
                    .is_ok()
                {
                    self.configured_bitrate = bitrate;
                }
            }
        }
    }
    fn pump(&mut self) -> Result<(), String> {
        // Poll only a bounded event batch; no GetEvent call waits for the driver.
        for _ in 0..32 {
            let event = match unsafe { self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(event) => event,
                Err(error) if error.code() == MF_E_NO_EVENTS_AVAILABLE => break,
                Err(error) => return Err(format!("Hardware encoder event: {error}")),
            };
            let kind =
                unsafe { event.GetType() }.map_err(|e| format!("Read encoder event: {e}"))?;
            unsafe { event.GetStatus() }
                .map_err(|e| format!("Hardware encoding failed: {e}"))?
                .ok()
                .map_err(|e| format!("Hardware encoding failed: {e}"))?;
            if kind == METransformNeedInput.0 as u32 {
                self.need_input = (self.need_input + 1).min(4);
            } else if kind == METransformHaveOutput.0 as u32 {
                self.read_output()?;
            }
        }
        if self.need_input > 0 {
            if let Some(frame) = self.pending.take() {
                unsafe {
                    let buffer = MFCreateMemoryBuffer(frame.bytes.len() as u32)
                        .map_err(|e| format!("Allocate encoder input: {e}"))?;
                    let mut data = ptr::null_mut();
                    buffer
                        .Lock(&mut data, None, None)
                        .map_err(|e| format!("Lock encoder input: {e}"))?;
                    ptr::copy_nonoverlapping(frame.bytes.as_ptr(), data, frame.bytes.len());
                    buffer
                        .Unlock()
                        .map_err(|e| format!("Unlock encoder input: {e}"))?;
                    buffer
                        .SetCurrentLength(frame.bytes.len() as u32)
                        .map_err(|e| format!("Set encoder input length: {e}"))?;
                    let sample = MFCreateSample()
                        .map_err(|e| format!("Create encoder input sample: {e}"))?;
                    sample
                        .AddBuffer(&buffer)
                        .map_err(|e| format!("Attach encoder input: {e}"))?;
                    sample
                        .SetSampleTime(
                            frame
                                .timestamp_us
                                .checked_mul(10)
                                .ok_or("Video clock overflow")?,
                        )
                        .map_err(|e| format!("Set capture clock: {e}"))?;
                    sample
                        .SetSampleDuration(10_000_000 / self.fps as i64)
                        .map_err(|e| format!("Set frame duration: {e}"))?;
                    self.transform
                        .ProcessInput(0, &sample, 0)
                        .map_err(|e| format!("Submit hardware frame: {e}"))?;
                    self.need_input -= 1;
                }
            }
        }
        Ok(())
    }
    fn read_output(&mut self) -> Result<(), String> {
        let mut output = OutputBuffer::default();
        let mut status = 0;
        let info = unsafe { self.transform.GetOutputStreamInfo(0) }
            .map_err(|e| format!("Read encoder output size: {e}"))?;
        if info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 == 0 {
            if info.cbSize as usize > MAX_ACCESS_UNIT {
                return Err("Hardware encoder output exceeds limit".into());
            }
            unsafe {
                let sample = MFCreateSample().map_err(|e| format!("Create encoder output: {e}"))?;
                let buffer = MFCreateMemoryBuffer(info.cbSize.max(1))
                    .map_err(|e| format!("Allocate encoder output: {e}"))?;
                sample
                    .AddBuffer(&buffer)
                    .map_err(|e| format!("Attach encoder output: {e}"))?;
                output.0.pSample = ManuallyDrop::new(Some(sample));
            }
        }
        unsafe {
            self.transform
                .ProcessOutput(0, std::slice::from_mut(&mut output.0), &mut status)
        }
        .map_err(|e| format!("Receive encoded hardware frame: {e}"))?;
        if let Some(sample) = output.0.pSample.as_ref() {
            unsafe {
                let buffer = sample
                    .ConvertToContiguousBuffer()
                    .map_err(|e| format!("Read H.264 output: {e}"))?;
                let length = buffer
                    .GetCurrentLength()
                    .map_err(|e| format!("Read H.264 size: {e}"))?
                    as usize;
                if length == 0 || length > MAX_ACCESS_UNIT {
                    return Err("Hardware H.264 frame exceeds limit".into());
                }
                let mut data = ptr::null_mut();
                buffer
                    .Lock(&mut data, None, None)
                    .map_err(|e| format!("Lock H.264 frame: {e}"))?;
                let bytes = std::slice::from_raw_parts(data, length).to_vec();
                buffer
                    .Unlock()
                    .map_err(|e| format!("Unlock H.264 frame: {e}"))?;
                let bytes = annex_b(bytes)?;
                let key = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0
                    || has_idr(&bytes);
                let timestamp_us = sample
                    .GetSampleTime()
                    .map_err(|e| format!("Read encoded capture clock: {e}"))?
                    / 10;
                if self.ready.len() >= 8 {
                    self.ready.pop_front();
                }
                self.ready.push_back(AccessUnit {
                    bytes,
                    timestamp_us,
                    key,
                });
            }
        }
        Ok(())
    }
}
impl Drop for HardwareEncoder {
    fn drop(&mut self) {
        self.pending = None;
        self.ready.clear();
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            let _ = self.activate.ShutdownObject();
        }
    }
}
unsafe fn media_type(
    subtype: windows::core::GUID,
    width: u32,
    height: u32,
    fps: u32,
) -> Result<IMFMediaType, String> {
    let media = MFCreateMediaType().map_err(|e| format!("Create video media type: {e}"))?;
    let result = (|| -> windows::core::Result<()> {
        media.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        media.SetGUID(&MF_MT_SUBTYPE, &subtype)?;
        media.SetUINT32(&MF_MT_INTERLACE_MODE, 2)?;
        media.SetUINT64(&MF_MT_FRAME_SIZE, ((width as u64) << 32) | height as u64)?;
        media.SetUINT64(&MF_MT_FRAME_RATE, ((fps as u64) << 32) | 1)?;
        media.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1_u64 << 32) | 1)?;
        Ok(())
    })();
    result.map_err(|e| format!("Configure video media type: {e}"))?;
    Ok(media)
}
fn annex_b(bytes: Vec<u8>) -> Result<Vec<u8>, String> {
    if bytes.starts_with(&[0, 0, 1]) || bytes.starts_with(&[0, 0, 0, 1]) {
        return Ok(bytes);
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut offset = 0;
    while offset < bytes.len() {
        let header: [u8; 4] = bytes
            .get(offset..offset + 4)
            .ok_or("Invalid H.264 length header")?
            .try_into()
            .unwrap();
        offset += 4;
        let length = u32::from_be_bytes(header) as usize;
        if length == 0 || length > bytes.len() - offset {
            return Err("Invalid H.264 access unit".into());
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&bytes[offset..offset + length]);
        offset += length;
    }
    Ok(out)
}
fn has_idr(bytes: &[u8]) -> bool {
    bytes
        .windows(4)
        .any(|v| v[..3] == [0, 0, 1] && v[3] & 31 == 5)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn access_units_are_bounded_and_normalized() {
        assert!(HardwareEncoder::new(0, 720, 30, 2_000_000).is_err());
        // These invalid cases return before COM startup or device enumeration.
        for (width, height, fps) in [
            (3842, 2160, 60),
            (3840, 2162, 60),
            (3839, 2160, 60),
            (3840, 2160, 120),
        ] {
            assert!(HardwareEncoder::new(width, height, fps, 32_000_000).is_err());
        }
        let bytes = annex_b(vec![0, 0, 0, 2, 0x65, 0x80, 0, 0, 0, 1, 0x41]).unwrap();
        assert!(has_idr(&bytes));
        assert!(annex_b(vec![0, 0, 0, 99, 1]).is_err());
    }
    #[test]
    #[ignore = "Explicit synthetic GPU codec acceptance; no capture device"]
    fn actual_hardware_h264_encodes_generated_nv12() {
        let mut encoder = HardwareEncoder::new(1280, 720, 30, 2_000_000)
            .expect("Hardware H.264 needed for this opt-in acceptance");
        encoder.request_keyframe();
        encoder.set_bitrate(1_500_000);
        let start = std::time::Instant::now();
        let mut frames = 0;
        let mut bytes = 0;
        let mut key = false;
        for index in 0..30 {
            let mut nv12 = vec![128; 1280 * 720 * 3 / 2];
            nv12[..1280 * 720].fill(70 + index);
            encoder.submit_nv12(nv12, index as i64 * 33_333).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(33));
            for frame in encoder.poll().unwrap() {
                frames += 1;
                bytes += frame.bytes.len();
                key |= frame.key;
            }
        }
        for _ in 0..100 {
            for frame in encoder.poll().unwrap() {
                frames += 1;
                bytes += frame.bytes.len();
                key |= frame.key;
            }
            if frames >= 20 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(
            frames >= 20 && bytes > 0 && key,
            "Hardware encoder failed usable output"
        );
        println!("Hardware H.264 {}: {frames}/30 generated720pframes, {bytes}bytes, {:.3}s; CPU-staged NV12, no physical capture",encoder.name,start.elapsed().as_secs_f64());
    }
}
