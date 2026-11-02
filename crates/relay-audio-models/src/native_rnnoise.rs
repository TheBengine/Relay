use crate::BLOCK;
use std::ffi::c_void;
use std::ptr::NonNull;

unsafe extern "C" {
    fn rnnoise_create(model: *mut c_void) -> *mut c_void;
    fn rnnoise_destroy(state: *mut c_void);
    fn rnnoise_get_frame_size() -> i32;
    fn rnnoise_process_frame(state: *mut c_void, output: *mut f32, input: *const f32) -> f32;
}

pub(crate) struct Stream(NonNull<c_void>);
// The state is exclusively owned, is never used concurrently, and has no
// thread-affine resources. Its model tables are immutable.
unsafe impl Send for Stream {}
impl Stream {
    pub(crate) fn new() -> Result<Self, String> {
        if unsafe { rnnoise_get_frame_size() } != BLOCK as i32 {
            return Err("Unexpected native RNNoise block size".into());
        }
        NonNull::new(unsafe { rnnoise_create(std::ptr::null_mut()) })
            .map(Self)
            .ok_or_else(|| "Native RNNoise could not allocate its state".into())
    }
    pub(crate) fn process(&mut self, block: &mut [f32; BLOCK]) {
        let input = block.map(|sample| sample * 32768.0);
        let mut output = [0.0; BLOCK];
        unsafe {
            rnnoise_process_frame(self.0.as_ptr(), output.as_mut_ptr(), input.as_ptr());
        }
        for (sample, out) in block.iter_mut().zip(output) {
            *sample = out / 32768.0;
        }
    }
}
impl Drop for Stream {
    fn drop(&mut self) {
        unsafe {
            rnnoise_destroy(self.0.as_ptr());
        }
    }
}
