//! Owns one VAD instance from the already bundled native WebRTC library.
//! No device access, callbacks, audio queue, or worker thread.
use std::{ffi::c_void, ptr::NonNull};

unsafe extern "C" {
    fn WebRtcVad_Create() -> *mut c_void;
    fn WebRtcVad_Free(handle: *mut c_void);
    fn WebRtcVad_Init(handle: *mut c_void) -> i32;
    fn WebRtcVad_set_mode(handle: *mut c_void, mode: i32) -> i32;
    fn WebRtcVad_Process(handle: *mut c_void, rate: i32, frame: *const i16, length: usize) -> i32;
}

pub struct VoiceDetector(NonNull<c_void>);
impl VoiceDetector {
    pub fn new() -> Option<Self> {
        // SAFETY: this is the C API in the pinned SDK's webrtc_vad.h. The
        // returned allocation is owned exclusively by this wrapper and freed
        // on every successful or failed initialization path through Drop.
        let detector = Self(NonNull::new(unsafe { WebRtcVad_Create() })?);
        // Mode 2 balances quiet speech against steady fan/hum in our fixtures.
        // SAFETY: the live, unique handle is initialized before configuration.
        if unsafe { WebRtcVad_Init(detector.0.as_ptr()) } != 0
            || unsafe { WebRtcVad_set_mode(detector.0.as_ptr(), 2) } != 0
        {
            return None;
        }
        Some(detector)
    }
    pub fn is_speech(&mut self, frame: &[i16; 160]) -> bool {
        // SAFETY: initialized handle; exactly 10 ms of valid mono 16 kHz PCM.
        // The synchronous C function neither retains nor modifies the input.
        unsafe { WebRtcVad_Process(self.0.as_ptr(), 16_000, frame.as_ptr(), frame.len()) == 1 }
    }
}
impl Drop for VoiceDetector {
    fn drop(&mut self) {
        // SAFETY: this wrapper is the sole owner; the handle is freed once.
        unsafe { WebRtcVad_Free(self.0.as_ptr()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_activity_detector_repeated_create_use_drop_keeps_silence_closed() {
        for _ in 0..200 {
            let mut detector = VoiceDetector::new().expect("bundled native VAD initializes");
            for _ in 0..10 {
                assert!(!detector.is_speech(&[0; 160]));
            }
        }
    }
}
