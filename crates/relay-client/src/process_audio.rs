//! Explicit selected-process loopback. Never captures microphones or the desktop mix.
use crate::capture::{CaptureSource, SourceKind};
use std::{
    collections::VecDeque,
    mem::size_of,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use windows::{
    core::{implement, Interface, HRESULT, PROPVARIANT},
    Win32::{
        Foundation::HWND,
        Media::Audio::*,
        System::{Com::*, Variant::VT_BLOB},
        UI::WindowsAndMessaging::{GetWindowThreadProcessId, IsWindow},
    },
};

pub struct ProcessAudioFrame {
    pub samples: Vec<i16>,
    pub timestamp_us: i64,
}
pub struct ProcessAudioSession {
    stop: Arc<AtomicBool>,
    ready: Arc<AtomicBool>,
    errors: Arc<Mutex<Option<String>>>,
    owner: Option<JoinHandle<()>>,
}
#[implement(IActivateAudioInterfaceCompletionHandler, IAgileObject)]
struct Completion {
    sender: Mutex<Option<mpsc::Sender<Result<IAudioClient, String>>>>,
}
impl IAgileObject_Impl for Completion_Impl {}
impl IActivateAudioInterfaceCompletionHandler_Impl for Completion_Impl {
    fn ActivateCompleted(
        &self,
        operation: Option<&IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        let result = (|| -> Result<IAudioClient, String> {
            let operation = operation.ok_or("Loopback activation callback was empty")?;
            let mut status = HRESULT(0);
            let mut activated = None;
            unsafe { operation.GetActivateResult(&mut status, &mut activated) }
                .map_err(|e| format!("Activate selected app audio: {e}"))?;
            status.ok().map_err(|e| {
                format!("Selected app audio unavailable (requires Windows build20348+): {e}")
            })?;
            activated
                .ok_or("Selected app audio activation returned no interface")?
                .cast()
                .map_err(|e| format!("Open selected app audio client: {e}"))
        })();
        if let Ok(mut sender) = self.sender.lock() {
            if let Some(sender) = sender.take() {
                let _ = sender.send(result);
            }
        }
        Ok(())
    }
}
impl ProcessAudioSession {
    #[cfg(test)]
    pub(crate) fn synthetic() -> Self {
        Self {
            stop: Arc::new(AtomicBool::new(false)),
            ready: Arc::new(AtomicBool::new(true)),
            errors: Arc::new(Mutex::new(None)),
            owner: None,
        }
    }
    pub fn start(
        source: &CaptureSource,
        sender: SyncSender<ProcessAudioFrame>,
    ) -> Result<Self, String> {
        let (window, pid) = selected_process(source)?;
        let stop = Arc::new(AtomicBool::new(false));
        let ready = Arc::new(AtomicBool::new(false));
        let errors = Arc::new(Mutex::new(None));
        let (worker_stop, worker_ready, worker_errors) =
            (stop.clone(), ready.clone(), errors.clone());
        let owner = thread::Builder::new()
            .name("relay-selected-app-audio".into())
            .spawn(move || {
                let result = capture(window, pid, sender, &worker_stop, &worker_ready);
                worker_ready.store(false, Ordering::Release);
                if !worker_stop.load(Ordering::Acquire) {
                    if let Err(error) = result {
                        if let Ok(mut slot) = worker_errors.lock() {
                            *slot = Some(error);
                        }
                    }
                }
            })
            .map_err(|e| format!("Start selected app audio: {e}"))?;
        Ok(Self {
            stop,
            ready,
            errors,
            owner: Some(owner),
        })
    }
    pub fn is_active(&self) -> bool {
        self.ready.load(Ordering::Acquire) && !self.stop.load(Ordering::Acquire)
    }
    pub fn take_error(&self) -> Option<String> {
        self.errors.lock().ok()?.take()
    }
    pub fn cancel(&self) {
        self.stop.store(true, Ordering::Release);
        self.ready.store(false, Ordering::Release);
    }
}
impl Drop for ProcessAudioSession {
    fn drop(&mut self) {
        self.cancel();
        self.owner.take();
    }
}
fn selected_process(source: &CaptureSource) -> Result<(isize, u32), String> {
    if source.kind != SourceKind::Window {
        return Err(
            "App audio requires a selected window; monitor and desktop audio are not captured"
                .into(),
        );
    }
    let mut parts = source.id.split(':');
    if parts.next() != Some("window") {
        return Err("Invalid selected audio window".into());
    }
    let hwnd = isize::from_str_radix(parts.next().ok_or("Missing window identity")?, 16)
        .map_err(|_| "Invalid window identity")?;
    let pid = parts
        .next()
        .ok_or("Missing selected app identity")?
        .parse::<u32>()
        .map_err(|_| "Invalid selected app identity")?;
    if parts.next().is_some() || pid == 0 || !matches_window(hwnd, pid) {
        return Err("The selected app window is no longer available".into());
    }
    Ok((hwnd, pid))
}
fn matches_window(window: isize, pid: u32) -> bool {
    unsafe {
        let hwnd = HWND(window as _);
        let mut actual = 0;
        IsWindow(hwnd).as_bool()
            && GetWindowThreadProcessId(hwnd, Some(&mut actual)) != 0
            && actual == pid
    }
}
struct Apartment;
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe {
            CoUninitialize();
        }
    }
}
struct AudioClient(IAudioClient);
impl Drop for AudioClient {
    fn drop(&mut self) {
        unsafe {
            let _ = self.0.Stop();
        }
    }
}
fn capture(
    window: isize,
    pid: u32,
    sender: SyncSender<ProcessAudioFrame>,
    stop: &AtomicBool,
    ready: &AtomicBool,
) -> Result<(), String> {
    if stop.load(Ordering::Acquire) {
        return Ok(());
    }
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok() }
        .map_err(|e| format!("Initialize app audio: {e}"))?;
    let _apartment = Apartment;
    let params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
            },
        },
    };
    // Owned CoTaskMem memory matches PROPVARIANT's VT_BLOB destructor contract.
    let variant = unsafe {
        let allocation = CoTaskMemAlloc(size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>()) as *mut u8;
        if allocation.is_null() {
            return Err("Allocate selected app activation parameters".into());
        }
        std::ptr::copy_nonoverlapping(
            &params as *const _ as *const u8,
            allocation,
            size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>(),
        );
        let mut raw: windows::core::imp::PROPVARIANT = std::mem::zeroed();
        raw.Anonymous.Anonymous.vt = VT_BLOB.0;
        raw.Anonymous.Anonymous.Anonymous.blob = windows::core::imp::BLOB {
            cbSize: size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
            pBlobData: allocation,
        };
        PROPVARIANT::from_raw(raw)
    };
    let (tx, rx) = mpsc::channel();
    let completion: IActivateAudioInterfaceCompletionHandler = Completion {
        sender: Mutex::new(Some(tx)),
    }
    .into();
    let _operation = unsafe {
        ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&variant),
            &completion,
        )
    }
    .map_err(|e| format!("Request selected app audio: {e}"))?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let client = loop {
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        match rx.recv_timeout(Duration::from_millis(10)) {
            Ok(result) => break AudioClient(result?),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if Instant::now() > deadline {
                    return Err("Selected app audio activation timed out".into());
                }
            }
            Err(_) => return Err("Selected app audio activation was interrupted".into()),
        }
    };
    if stop.load(Ordering::Acquire) || !matches_window(window, pid) {
        return Ok(());
    }
    let format = WAVEFORMATEX {
        wFormatTag: 1,
        nChannels: 2,
        nSamplesPerSec: 48_000,
        nAvgBytesPerSec: 192_000,
        nBlockAlign: 4,
        wBitsPerSample: 16,
        cbSize: 0,
    };
    // Request one codec-frame of loopback buffering; older drivers may need the default.
    let initialize = |duration| unsafe {
        client.0.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK
                | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
            duration,
            0,
            &format,
            None,
        )
    };
    initialize(200_000)
        .or_else(|_| initialize(1_000_000))
        .map_err(|e| format!("Configure selected app loopback: {e}"))?;
    let capture: IAudioCaptureClient = unsafe { client.0.GetService() }
        .map_err(|e| format!("Open selected app loopback buffer: {e}"))?;
    if stop.load(Ordering::Acquire) {
        return Ok(());
    }
    unsafe { client.0.Start() }.map_err(|e| format!("Start selected app audio: {e}"))?;
    if stop.load(Ordering::Acquire) {
        return Ok(());
    }
    ready.store(true, Ordering::Release);
    let mut samples = VecDeque::with_capacity(3840);
    let mut timestamp_us = 0_i64;
    while !stop.load(Ordering::Acquire) {
        if !matches_window(window, pid) {
            return Err("The selected audio app window closed".into());
        }
        for _ in 0..8 {
            let available = unsafe { capture.GetNextPacketSize() }
                .map_err(|e| format!("Read selected app audio: {e}"))?;
            if available == 0 {
                break;
            }
            let mut data = std::ptr::null_mut();
            let mut frames = 0;
            let mut flags = 0;
            let mut qpc = 0;
            unsafe { capture.GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc)) }
                .map_err(|e| format!("Read selected app audio samples: {e}"))?;
            if frames > 48_000 {
                unsafe {
                    let _ = capture.ReleaseBuffer(frames);
                }
                return Err("Selected app audio packet exceeds bound".into());
            }
            if samples.is_empty() {
                timestamp_us = (qpc / 10).min(i64::MAX as u64) as i64;
            }
            let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
            if !silent && data.is_null() {
                unsafe {
                    let _ = capture.ReleaseBuffer(frames);
                }
                return Err("Invalid selected app audio packet".into());
            }
            let input = if silent {
                None
            } else {
                Some(unsafe { std::slice::from_raw_parts(data as *const i16, frames as usize * 2) })
            };
            let mut skipped_samples = 0_i64;
            for index in 0..frames as usize * 2 {
                if samples.len() == 3840 {
                    samples.pop_front();
                    skipped_samples += 1;
                }
                samples.push_back(input.map_or(0, |input| input[index]));
            }
            // Advance in stereo frames, retaining alignment when an oversized callback is trimmed.
            timestamp_us = timestamp_us.saturating_add((skipped_samples / 2) * 1_000_000 / 48_000);
            unsafe { capture.ReleaseBuffer(frames) }
                .map_err(|e| format!("Release app audio samples: {e}"))?;
            while samples.len() >= 1920 {
                let frame = ProcessAudioFrame {
                    samples: samples.drain(..1920).collect(),
                    timestamp_us,
                };
                timestamp_us = timestamp_us.saturating_add(20_000);
                if stop.load(Ordering::Acquire) {
                    return Ok(());
                }
                match sender.try_send(frame) {
                    Ok(()) | Err(mpsc::TrySendError::Full(_)) => {}
                    Err(mpsc::TrySendError::Disconnected(_)) => return Ok(()),
                }
            }
        }
        thread::sleep(Duration::from_millis(5));
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn app_audio_requires_current_owned_window_identity() {
        let mut source = CaptureSource {
            id: "monitor:0".into(),
            name: "Test".into(),
            kind: SourceKind::Monitor,
            width: 10,
            height: 10,
        };
        assert!(selected_process(&source).is_err());
        source.kind = SourceKind::Window;
        source.id = "window:0:1".into();
        assert!(selected_process(&source).is_err());
    }
}
