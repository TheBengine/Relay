//! Event-driven WASAPI shared-mode streams. All COM objects stay on their owner
//! thread; unsupported devices/formats are rejected before CPAL fallback opens.
use crate::audio_wake::Event;
use std::{
    sync::{mpsc, Arc},
    thread::{self, JoinHandle},
};
use windows::{
    core::Result,
    Win32::{
        Devices::FunctionDiscovery::PKEY_Device_FriendlyName,
        Foundation::WAIT_OBJECT_0,
        Media::Audio::*,
        System::{
            Com::{
                CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize,
                StructuredStorage::PropVariantToStringAlloc, CLSCTX_ALL, COINIT_MULTITHREADED,
                STGM_READ,
            },
            Threading::{WaitForMultipleObjects, INFINITE},
        },
    },
};

#[derive(Clone, Copy, Debug)]
pub(crate) struct PeriodInfo {
    pub engine_frames: u32,
    pub default_frames: u32,
    /// The smallest period the device offers, and the step between periods.
    pub min_frames: u32,
    pub fundamental_frames: u32,
    pub buffer_frames: u32,
    /// What the driver reports for the stream's own latency (100 ns units).
    pub stream_latency_hns: i64,
    pub sample_rate: u32,
    pub channels: u16,
}
impl PeriodInfo {
    pub(crate) fn period_ms(self) -> f32 {
        self.frames_ms(self.engine_frames)
    }
    pub(crate) fn frames_ms(self, frames: u32) -> f32 {
        frames as f32 * 1000.0 / self.sample_rate as f32
    }
    pub(crate) fn stream_latency_ms(self) -> f32 {
        self.stream_latency_hns as f32 / 10_000.0
    }
}
type CaptureCallback = Box<dyn FnMut(&[f32]) + Send>;
type RenderCallback = Box<dyn FnMut(&mut [f32]) + Send>;
enum Callback {
    Capture(CaptureCallback),
    Render(RenderCallback),
}
pub(crate) struct Stream {
    stop: Arc<Event>,
    play: Arc<Event>,
    worker: Option<JoinHandle<()>>,
    pub info: PeriodInfo,
}
impl Stream {
    pub(crate) fn input(
        name: String,
        rate: u32,
        channels: u16,
        callback: impl FnMut(&[f32]) + Send + 'static,
        error: impl FnMut(String) + Send + 'static,
    ) -> std::result::Result<Self, String> {
        Self::open(
            name,
            rate,
            channels,
            Callback::Capture(Box::new(callback)),
            Box::new(error),
        )
    }
    pub(crate) fn output(
        name: String,
        rate: u32,
        channels: u16,
        callback: impl FnMut(&mut [f32]) + Send + 'static,
        error: impl FnMut(String) + Send + 'static,
    ) -> std::result::Result<Self, String> {
        Self::open(
            name,
            rate,
            channels,
            Callback::Render(Box::new(callback)),
            Box::new(error),
        )
    }
    fn open(
        name: String,
        rate: u32,
        channels: u16,
        callback: Callback,
        mut error: Box<dyn FnMut(String) + Send>,
    ) -> std::result::Result<Self, String> {
        let stop = Arc::new(Event::new(true).map_err(|e| e.to_string())?);
        let play = Arc::new(Event::new(true).map_err(|e| e.to_string())?);
        let worker_stop = stop.clone();
        let worker_play = play.clone();
        let (ready, receiver) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("relay-wasapi-shared".into())
            .spawn(move || {
                let _audio_priority = crate::thread_priority::AudioThreadGuard::new("Pro Audio");
                // COM lifetimes and acquired buffers are unwound on callback panic.
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run(
                        &name,
                        rate,
                        channels,
                        callback,
                        &worker_stop,
                        &worker_play,
                        &ready,
                    )
                }));
                match result {
                    Ok(Err(e)) => {
                        let text = format!("Windows audio: {e}");
                        if ready.try_send(Err(text.clone())).is_err() {
                            error(text);
                        }
                    }
                    Err(_) => {
                        let text = "Windows audio callback stopped unexpectedly".to_string();
                        if ready.try_send(Err(text.clone())).is_err() {
                            error(text);
                        }
                    }
                    _ => {}
                }
            })
            .map_err(|e| e.to_string())?;
        match receiver.recv() {
            Ok(Ok(info)) => Ok(Self {
                stop,
                play,
                worker: Some(worker),
                info,
            }),
            result => {
                stop.signal();
                let _ = worker.join();
                Err(result
                    .ok()
                    .and_then(|r| r.err())
                    .unwrap_or_else(|| "Windows audio initialization stopped".into()))
            }
        }
    }
    pub(crate) fn play(&self) -> std::result::Result<(), String> {
        self.play.signal();
        Ok(())
    }
}
impl Drop for Stream {
    fn drop(&mut self) {
        self.stop.signal();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
struct Com;
impl Drop for Com {
    fn drop(&mut self) {
        unsafe {
            CoUninitialize();
        }
    }
}
struct Running<'a>(&'a IAudioClient3);
impl Drop for Running<'_> {
    fn drop(&mut self) {
        unsafe {
            let _ = self.0.Stop();
        }
    }
}
struct CaptureLease<'a>(&'a IAudioCaptureClient, u32);
impl Drop for CaptureLease<'_> {
    fn drop(&mut self) {
        unsafe {
            let _ = self.0.ReleaseBuffer(self.1);
        }
    }
}
struct RenderLease<'a>(&'a IAudioRenderClient, u32);
impl Drop for RenderLease<'_> {
    fn drop(&mut self) {
        unsafe {
            let _ = self.0.ReleaseBuffer(self.1, 0);
        }
    }
}

fn period(min: u32, fundamental: u32, max: u32) -> Option<u32> {
    if min == 0 || fundamental == 0 || min > max {
        return None;
    }
    let value = min
        .checked_add(fundamental - 1)?
        .checked_div(fundamental)?
        .checked_mul(fundamental)?;
    (value <= max).then_some(value)
}
fn frames_to_samples(frames: u32, channels: u16) -> Option<usize> {
    let samples = (frames as usize).checked_mul(channels as usize)?;
    (frames > 0 && channels > 0 && samples <= 1_048_576).then_some(samples)
}
unsafe fn endpoint(name: &str, capture: bool) -> Result<IMMDevice> {
    let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
    let endpoints = enumerator.EnumAudioEndpoints(
        if capture { eCapture } else { eRender },
        DEVICE_STATE_ACTIVE,
    )?;
    let mut selected = None;
    for index in 0..endpoints.GetCount()? {
        let device = endpoints.Item(index)?;
        let value = device
            .OpenPropertyStore(STGM_READ)?
            .GetValue(&PKEY_Device_FriendlyName)?;
        let text = PropVariantToStringAlloc(&value)?;
        let friendly = text.to_string();
        CoTaskMemFree(Some(text.0.cast()));
        if friendly?.as_str() == name {
            // CPAL exposes a friendly name. Never choose a different endpoint
            // silently if two active devices have the same name.
            if selected.is_some() {
                return Err(windows::core::Error::from_hresult(windows::core::HRESULT(
                    0x80070057u32 as i32,
                )));
            }
            selected = Some(device);
        }
    }
    selected.ok_or_else(|| {
        windows::core::Error::from_hresult(windows::core::HRESULT(0x80070490u32 as i32))
    })
}
pub(crate) fn is_bluetooth(name: &str, capture: bool) -> bool {
    unsafe {
        let Ok(()) = CoInitializeEx(None, COINIT_MULTITHREADED).ok() else {
            return false;
        };
        let _com = Com;
        let read = || -> Result<bool> {
            let value = endpoint(name, capture)?
                .OpenPropertyStore(STGM_READ)?
                .GetValue(
                    &windows::Win32::Devices::FunctionDiscovery::PKEY_Device_EnumeratorName,
                )?;
            let text = PropVariantToStringAlloc(&value)?;
            let name = text.to_string();
            CoTaskMemFree(Some(text.0.cast()));
            Ok(crate::audio_settings::bluetooth_enumerator(&name?))
        };
        read().unwrap_or(false)
    }
}

fn invalid() -> windows::core::Error {
    windows::core::Error::from_hresult(windows::core::HRESULT(0x80070057u32 as i32))
}

fn run(
    name: &str,
    rate: u32,
    channels: u16,
    mut callback: Callback,
    stop: &Event,
    play: &Event,
    ready: &mpsc::SyncSender<std::result::Result<PeriodInfo, String>>,
) -> Result<()> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        let _com = Com;
        let capture = matches!(callback, Callback::Capture(_));
        let device = endpoint(name, capture)?;
        let client: IAudioClient3 = device.Activate(CLSCTX_ALL, None)?;
        let format = client.GetMixFormat()?;
        // Mix-format storage belongs to COM and is freed on every exit path.
        struct Format(*mut WAVEFORMATEX);
        impl Drop for Format {
            fn drop(&mut self) {
                unsafe {
                    CoTaskMemFree(Some(self.0.cast()));
                }
            }
        }
        let owned = Format(format);
        let wave = &*owned.0;
        let float = if wave.wFormatTag == 3 {
            true
        } else if wave.wFormatTag == 0xfffe && wave.cbSize >= 22 {
            let extended = &*(format as *const WAVEFORMATEXTENSIBLE);
            let subtype = extended.SubFormat;
            subtype == windows::core::GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71)
        } else {
            false
        };
        if !float
            || wave.wBitsPerSample != 32
            || wave.nSamplesPerSec != rate
            || wave.nChannels != channels
            || wave.nBlockAlign != channels.checked_mul(4).ok_or_else(invalid)?
        {
            return Err(invalid());
        }
        let (mut default, mut fundamental, mut min, mut max) = (0, 0, 0, 0);
        client.GetSharedModeEnginePeriod(
            format,
            &mut default,
            &mut fundamental,
            &mut min,
            &mut max,
        )?;
        let selected = period(min, fundamental, max).ok_or_else(invalid)?;
        let audio_event = Event::new(false)?;
        client.InitializeSharedAudioStream(
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            selected,
            format,
            None,
        )?;
        let frames = client.GetBufferSize()?;
        let samples = frames_to_samples(frames, channels).ok_or_else(invalid)?;
        client.SetEventHandle(audio_event.0)?;
        let info = PeriodInfo {
            engine_frames: selected,
            default_frames: default,
            min_frames: min,
            fundamental_frames: fundamental,
            buffer_frames: frames,
            // Diagnostic only: a driver that cannot say reports zero.
            stream_latency_hns: client.GetStreamLatency().unwrap_or(0),
            sample_rate: rate,
            channels,
        };
        let input: Option<IAudioCaptureClient> = if capture {
            Some(client.GetService()?)
        } else {
            None
        };
        let output: Option<IAudioRenderClient> = if capture {
            None
        } else {
            Some(client.GetService()?)
        };
        let silence = vec![0.0f32; samples];
        ready.send(Ok(info)).map_err(|_| invalid())?;
        if WaitForMultipleObjects(&[stop.0, play.0], false, INFINITE)
            != windows::Win32::Foundation::WAIT_EVENT(WAIT_OBJECT_0.0 + 1)
        {
            return Ok(());
        }
        if let Some(output) = &output {
            output.GetBuffer(frames)?;
            output.ReleaseBuffer(frames, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)?;
        }
        client.Start()?;
        let _running = Running(&client);
        loop {
            let event = WaitForMultipleObjects(&[stop.0, audio_event.0], false, INFINITE);
            if event == WAIT_OBJECT_0 {
                return Ok(());
            }
            if event != windows::Win32::Foundation::WAIT_EVENT(WAIT_OBJECT_0.0 + 1) {
                return Err(windows::core::Error::from_win32());
            }
            match &mut callback {
                Callback::Capture(callback) => {
                    let input = input.as_ref().unwrap();
                    for _ in 0..32 {
                        if input.GetNextPacketSize()? == 0 {
                            break;
                        }
                        let (mut data, mut count, mut flags) = (std::ptr::null_mut(), 0, 0);
                        input.GetBuffer(&mut data, &mut count, &mut flags, None, None)?;
                        if count == 0 {
                            // AUDCLNT_S_BUFFER_EMPTY is a successful HRESULT.
                            break;
                        }
                        let lease = CaptureLease(input, count);
                        let count_samples = frames_to_samples(count, channels)
                            .filter(|n| *n <= silence.len())
                            .ok_or_else(invalid)?;
                        if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                            callback(&silence[..count_samples]);
                        } else if !data.is_null() {
                            callback(std::slice::from_raw_parts(
                                data.cast::<f32>(),
                                count_samples,
                            ));
                        } else {
                            return Err(invalid());
                        }
                        drop(lease);
                    }
                }
                Callback::Render(callback) => {
                    let output = output.as_ref().unwrap();
                    let available = frames
                        .checked_sub(client.GetCurrentPadding()?)
                        .ok_or_else(invalid)?;
                    if available == 0 {
                        continue;
                    }
                    let count = frames_to_samples(available, channels).ok_or_else(invalid)?;
                    let data = output.GetBuffer(available)?;
                    let _lease = RenderLease(output, available);
                    if data.is_null() {
                        return Err(invalid());
                    }
                    let output = std::slice::from_raw_parts_mut(data.cast::<f32>(), count);
                    output.fill(0.0);
                    callback(output);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn engine_period_is_valid_aligned_and_minimal() {
        assert_eq!(period(120, 48, 960), Some(144));
        assert_eq!(period(128, 128, 512), Some(128));
        for invalid in [
            (0, 48, 960),
            (120, 0, 960),
            (128, 128, 64),
            (u32::MAX, 2, u32::MAX),
        ] {
            assert_eq!(period(invalid.0, invalid.1, invalid.2), None);
        }
        let info = PeriodInfo {
            engine_frames: 120,
            default_frames: 480,
            min_frames: 120,
            fundamental_frames: 120,
            buffer_frames: 240,
            stream_latency_hns: 25_000,
            sample_rate: 48000,
            channels: 2,
        };
        assert_eq!(info.period_ms(), 2.5);
        assert_eq!(info.frames_ms(info.default_frames), 10.0);
        assert_eq!(info.stream_latency_ms(), 2.5);
    }

    /// Opens the default microphone, then the default speakers, for 3 s each
    /// and prints the period Windows negotiated and the callback interval
    /// actually seen. Audio is never stored: only callback times and sizes.
    /// `RELAY_MEASURE_INPUT` / `RELAY_MEASURE_OUTPUT` pick a device by name.
    #[test]
    #[ignore = "Opens the real default microphone and speakers; run by hand to measure timing"]
    fn actual_shared_streams_report_period_and_callback_interval() {
        use std::{
            sync::Mutex,
            time::{Duration, Instant},
        };
        fn default_endpoint(capture: bool) -> (String, u32, u16) {
            unsafe {
                CoInitializeEx(None, COINIT_MULTITHREADED).ok().unwrap();
                let _com = Com;
                let enumerator: IMMDeviceEnumerator =
                    CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).unwrap();
                let flow = if capture { eCapture } else { eRender };
                let device = enumerator.GetDefaultAudioEndpoint(flow, eConsole).unwrap();
                let value = device
                    .OpenPropertyStore(STGM_READ)
                    .unwrap()
                    .GetValue(&PKEY_Device_FriendlyName)
                    .unwrap();
                let text = PropVariantToStringAlloc(&value).unwrap();
                let name = text.to_string().unwrap();
                CoTaskMemFree(Some(text.0.cast()));
                let client: IAudioClient = device.Activate(CLSCTX_ALL, None).unwrap();
                let format = client.GetMixFormat().unwrap();
                let (rate, channels) = ((*format).nSamplesPerSec, (*format).nChannels);
                CoTaskMemFree(Some(format.cast()));
                (name, rate, channels)
            }
        }
        for capture in [true, false] {
            let (mut name, rate, channels) = default_endpoint(capture);
            let variable = if capture { "RELAY_MEASURE_INPUT" } else { "RELAY_MEASURE_OUTPUT" };
            if let Ok(chosen) = std::env::var(variable) {
                name = chosen;
            }
            let calls: Arc<Mutex<Vec<(Instant, usize)>>> =
                Arc::new(Mutex::new(Vec::with_capacity(4096)));
            let record = calls.clone();
            let errors = Arc::new(Mutex::new(Vec::new()));
            let failures = errors.clone();
            let on_error = move |error: String| failures.lock().unwrap().push(error);
            let frames = move |samples: usize| samples / channels as usize;
            let stream = if capture {
                Stream::input(name.clone(), rate, channels, move |data| {
                    record.lock().unwrap().push((Instant::now(), frames(data.len())));
                }, on_error)
            } else {
                Stream::output(name.clone(), rate, channels, move |data| {
                    data.fill(0.0);
                    record.lock().unwrap().push((Instant::now(), frames(data.len())));
                }, on_error)
            }
            .unwrap();
            stream.play().unwrap();
            thread::sleep(Duration::from_secs(3));
            let info = stream.info;
            drop(stream);
            assert!(errors.lock().unwrap().is_empty(), "{:?}", errors.lock().unwrap());
            // Skip the start-up burst: the first half second.
            let calls = calls.lock().unwrap();
            let start = calls[0].0 + Duration::from_millis(500);
            let steady: Vec<_> = calls.iter().filter(|(at, _)| *at >= start).collect();
            let mut gaps: Vec<f64> = steady
                .windows(2)
                .map(|pair| (pair[1].0 - pair[0].0).as_secs_f64() * 1000.0)
                .collect();
            gaps.sort_by(f64::total_cmp);
            let span = (steady.last().unwrap().0 - steady[0].0).as_secs_f64() * 1000.0;
            let delivered: usize = steady.iter().skip(1).map(|(_, n)| n).sum();
            let sizes = (
                steady.iter().map(|(_, n)| *n).min().unwrap(),
                steady.iter().map(|(_, n)| *n).max().unwrap(),
            );
            let at = |q: f64| gaps[((gaps.len() - 1) as f64 * q).round() as usize];
            println!(
                "{} \"{name}\": {rate} Hz, {channels} ch; negotiated {:.3} ms ({} frames), \
                 default {:.3} ms, minimum {:.3} ms, step {:.3} ms, buffer {:.3} ms, \
                 driver stream latency {:.3} ms",
                if capture { "capture" } else { "render" },
                info.period_ms(),
                info.engine_frames,
                info.frames_ms(info.default_frames),
                info.frames_ms(info.min_frames),
                info.frames_ms(info.fundamental_frames),
                info.frames_ms(info.buffer_frames),
                info.stream_latency_ms(),
            );
            println!(
                "  {} callbacks in {span:.0} ms: interval mean {:.3} ms, median {:.3}, \
                 p95 {:.3}, max {:.3}, min {:.3}; {sizes:?} frames per callback; \
                 {:.3} ms of audio per ms",
                steady.len(),
                span / gaps.len() as f64,
                at(0.5),
                at(0.95),
                gaps[gaps.len() - 1],
                gaps[0],
                delivered as f64 * 1000.0 / rate as f64 / span,
            );
            assert!(steady.len() > 50);
        }
    }
    #[test]
    fn untrusted_driver_sizes_are_bounded_before_slices_or_allocations() {
        assert_eq!(frames_to_samples(480, 2), Some(960));
        assert_eq!(frames_to_samples(u32::MAX, u16::MAX), None);
        assert_eq!(frames_to_samples(480, 0), None);
        assert_eq!(frames_to_samples(0, 2), None);
    }

    #[test]
    fn rejected_native_endpoint_returns_to_fallback_without_a_runtime_error() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let called = Arc::new(AtomicBool::new(false));
        let report = called.clone();
        let result = Stream::input(
            "Relay-owned-missing-endpoint-736d45ab".into(),
            48000,
            2,
            |_| panic!("A rejected endpoint must never capture"),
            move |_| {
                report.store(true, Ordering::Release);
            },
        );
        assert!(result.is_err());
        assert!(!called.load(Ordering::Acquire));
    }

    #[test]
    #[ignore = "Explicit synthetic silent render acceptance; never opens a microphone"]
    fn actual_shared_output_reports_negotiated_period_and_stops() {
        use std::{
            sync::atomic::{AtomicU64, Ordering},
            time::{Duration, Instant},
        };
        let (name, rate, channels) = unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok().unwrap();
            let _com = Com;
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).unwrap();
            let device = enumerator
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .unwrap();
            let value = device
                .OpenPropertyStore(STGM_READ)
                .unwrap()
                .GetValue(&PKEY_Device_FriendlyName)
                .unwrap();
            let text = PropVariantToStringAlloc(&value).unwrap();
            let name = text.to_string().unwrap();
            CoTaskMemFree(Some(text.0.cast()));
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None).unwrap();
            let format = client.GetMixFormat().unwrap();
            let rate = (*format).nSamplesPerSec;
            let channels = (*format).nChannels;
            CoTaskMemFree(Some(format.cast()));
            (name, rate, channels)
        };
        let callbacks = Arc::new(AtomicU64::new(0));
        let rendered = callbacks.clone();
        let failures = Arc::new(std::sync::Mutex::new(Vec::new()));
        let errors = failures.clone();
        let stream = Stream::output(
            name,
            rate,
            channels,
            move |output| {
                output.fill(0.0);
                rendered.fetch_add(1, Ordering::Relaxed);
            },
            move |error| errors.lock().unwrap().push(error),
        )
        .unwrap();
        assert!(stream.info.engine_frames <= stream.info.default_frames);
        println!("WASAPI shared output: negotiated={:.3}ms, default={}frames, actual-buffer={}frames, sample-rate={}, channels={}", stream.info.period_ms(), stream.info.default_frames, stream.info.buffer_frames, stream.info.sample_rate, stream.info.channels);
        stream.play().unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while callbacks.load(Ordering::Acquire) < 3 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(callbacks.load(Ordering::Acquire) >= 3);
        drop(stream);
        assert!(failures.lock().unwrap().is_empty());
        let count = callbacks.load(Ordering::Acquire);
        thread::sleep(Duration::from_millis(50));
        assert_eq!(callbacks.load(Ordering::Acquire), count);
    }
}
