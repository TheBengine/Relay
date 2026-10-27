//! Explicit native capture with selectable sources, bounded frames and measured pacing.
//! Enumeration does not capture pixels or start a camera stream.

use image::{imageops::FilterType, DynamicImage};
use relay_protocol::MAX_FRAME_BYTES;
use serde::{Deserialize, Serialize};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub const MAX_VIDEO_WIDTH: u32 = 3840;
pub const MAX_VIDEO_HEIGHT: u32 = 2160;
pub const MAX_VIDEO_PIXELS: u64 = MAX_VIDEO_WIDTH as u64 * MAX_VIDEO_HEIGHT as u64;

pub fn valid_video_dimensions(width: u32, height: u32) -> bool {
    width > 0
        && height > 0
        && width <= MAX_VIDEO_WIDTH
        && height <= MAX_VIDEO_HEIGHT
        && u64::from(width) * u64::from(height) <= MAX_VIDEO_PIXELS
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceKind {
    Monitor,
    Window,
    Camera,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureSource {
    pub id: String,
    pub name: String,
    pub kind: SourceKind,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum CaptureQuality {
    P480,
    #[default]
    P720,
    P1080,
    P1440,
    P2160,
    /// Preserve the source dimensions and aspect ratio within the 4K ceiling.
    Source,
}

impl CaptureQuality {
    #[cfg_attr(not(any(test, feature = "native-rtc")), allow(dead_code))]
    pub fn dimensions(self) -> (u32, u32) {
        match self {
            Self::P480 => (854, 480),
            Self::P720 => (1280, 720),
            Self::P1080 => (1920, 1080),
            Self::P1440 => (2560, 1440),
            Self::P2160 | Self::Source => (MAX_VIDEO_WIDTH, MAX_VIDEO_HEIGHT),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum VideoBitrate {
    #[default]
    Auto,
    Mbps2,
    Mbps4,
    Mbps8,
    Mbps12,
    Mbps20,
    Mbps32,
}

fn normalized_fps(fps: u32) -> u32 {
    match fps {
        0..=15 => 15,
        16..=24 => 24,
        25..=30 => 30,
        _ => 60,
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum VideoCodecPreference {
    #[default]
    Auto,
    Software,
    Hardware,
}

/// What the encoder protects first when upload or CPU runs short.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum VideoPriority {
    /// Unchanged behaviour: smooth camera, sharp screen share.
    #[default]
    Auto,
    /// Keep resolution and lower the frame rate first: text and slides stay readable.
    Detail,
    /// Keep the frame rate and lower the resolution first: games and video stay fluid.
    Motion,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureOptions {
    pub quality: CaptureQuality,
    pub fps: u32,
    pub jpeg_quality: u8,
    #[serde(default)]
    pub codec: VideoCodecPreference,
    #[serde(default)]
    pub application_audio: bool,
    #[serde(default)]
    pub adaptive_video: bool,
    #[serde(default)]
    pub bitrate: VideoBitrate,
    #[serde(default)]
    pub priority: VideoPriority,
}

impl Default for CaptureOptions {
    fn default() -> Self {
        Self {
            quality: CaptureQuality::P720,
            fps: 30,
            jpeg_quality: 70,
            codec: VideoCodecPreference::Auto,
            application_audio: false,
            adaptive_video: true,
            bitrate: VideoBitrate::Auto,
            priority: VideoPriority::Auto,
        }
    }
}
impl CaptureOptions {
    #[cfg_attr(not(any(test, feature = "native-rtc")), allow(dead_code))]
    pub fn normalized(self) -> Self {
        Self {
            fps: normalized_fps(self.fps),
            jpeg_quality: self.jpeg_quality.clamp(10, 90),
            ..self
        }
    }

    /// Maximum encoder rate. The connection may still send less than this ceiling.
    #[cfg_attr(not(any(test, feature = "native-rtc")), allow(dead_code))]
    pub fn target_bitrate(self) -> u32 {
        let (width, height) = self.quality.dimensions();
        self.target_bitrate_for(width, height, self.fps)
    }

    /// Use actual output dimensions for Source and adaptive resolution changes.
    #[cfg_attr(not(any(test, feature = "native-rtc")), allow(dead_code))]
    pub fn target_bitrate_for(self, width: u32, height: u32, fps: u32) -> u32 {
        let manual = match self.bitrate {
            VideoBitrate::Auto => None,
            VideoBitrate::Mbps2 => Some(2_000_000),
            VideoBitrate::Mbps4 => Some(4_000_000),
            VideoBitrate::Mbps8 => Some(8_000_000),
            VideoBitrate::Mbps12 => Some(12_000_000),
            VideoBitrate::Mbps20 => Some(20_000_000),
            VideoBitrate::Mbps32 => Some(32_000_000),
        };
        if let Some(bitrate) = manual {
            return bitrate;
        }
        let pixels = u64::from(width) * u64::from(height);
        let base: u64 = if pixels <= 854 * 480 {
            800_000
        } else if pixels <= 1920 * 1080 {
            4_000_000
        } else if pixels <= 2560 * 1440 {
            8_000_000
        } else {
            16_000_000
        };
        (base * u64::from(normalized_fps(fps)) / 30).clamp(100_000, 32_000_000) as u32
    }
}

#[derive(Clone, Debug, Default)]
pub struct CaptureDiagnostics {
    pub actual_width: u32,
    pub actual_height: u32,
    pub target_fps: u32,
    /// Frames accepted by the outgoing bounded queue per second, not receiver FPS.
    pub delivered_fps: f32,
    #[cfg_attr(not(feature = "native-rtc"), allow(dead_code))]
    pub frames: u64,
    #[cfg_attr(not(feature = "native-rtc"), allow(dead_code))]
    pub dropped_frames: u64,
    #[cfg_attr(not(feature = "native-rtc"), allow(dead_code))]
    pub encode_ms: f32,
    pub source: String,
    #[cfg_attr(not(feature = "native-rtc"), allow(dead_code))]
    pub device_format: String,
    pub video_codec: String,
    #[cfg_attr(not(feature = "native-rtc"), allow(dead_code))]
    pub adaptive_output: Option<String>,
    #[cfg_attr(not(feature = "native-rtc"), allow(dead_code))]
    pub adaptive_samples: u64,
    /// Recent outbound RTP payload rate, sampled locally; not the configured ceiling.
    #[cfg_attr(not(feature = "native-rtc"), allow(dead_code))]
    pub outbound_mbps: Option<f64>,
    #[cfg_attr(not(feature = "native-rtc"), allow(dead_code))]
    pub outbound_sample_at: Option<Instant>,
    /// What the encoder actually sent recently, from the same RTC samples.
    #[cfg_attr(not(feature = "native-rtc"), allow(dead_code))]
    pub sent: Option<crate::adaptive_video::SentVideo>,
    /// The options the live publication started with, to compare `sent` against.
    #[cfg_attr(not(feature = "native-rtc"), allow(dead_code))]
    pub sent_options: Option<CaptureOptions>,
}

#[derive(Debug)]
pub struct CaptureFrame {
    pub kind: String,
    pub generation: u64,
    pub jpeg: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

pub fn enumerate_screen_sources() -> Result<Vec<CaptureSource>, String> {
    let monitors = screenshots::Screen::all().map_err(|e| format!("Enumerate displays: {e}"))?;
    let mut sources: Vec<_> = monitors
        .iter()
        .enumerate()
        .map(|(index, screen)| {
            let display = screen.display_info;
            CaptureSource {
                id: format!("monitor:{}", display.id),
                name: format!(
                    "Display {}{}",
                    index + 1,
                    if display.is_primary { " (primary)" } else { "" }
                ),
                kind: SourceKind::Monitor,
                width: display.width,
                height: display.height,
            }
        })
        .collect();
    #[cfg(windows)]
    sources.extend(native_windows::enumerate()?);
    Ok(sources)
}

pub fn enumerate_cameras() -> Result<Vec<CaptureSource>, String> {
    #[cfg(windows)]
    {
        nokhwa::query(nokhwa::utils::ApiBackend::MediaFoundation)
            .map(|cameras| {
                cameras
                    .into_iter()
                    .map(|camera| CaptureSource {
                        id: format!("camera:{}", camera.index()),
                        name: camera.human_name(),
                        kind: SourceKind::Camera,
                        width: 0,
                        height: 0,
                    })
                    .collect()
            })
            .map_err(|e| format!("Enumerate cameras: {e}"))
    }
    #[cfg(not(windows))]
    {
        Ok(Vec::new())
    }
}

#[cfg(feature = "native-rtc")]
pub struct RawCaptureFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub timestamp_us: i64,
}

#[cfg_attr(not(any(test, feature = "native-rtc")), allow(dead_code))]
enum FrameSink {
    #[cfg_attr(not(test), allow(dead_code))]
    Jpeg(SyncSender<CaptureFrame>),
    #[cfg(feature = "native-rtc")]
    Raw(SyncSender<RawCaptureFrame>),
}
#[cfg_attr(not(any(test, feature = "native-rtc")), allow(dead_code))]
enum CapturedFrame {
    Jpeg(CaptureFrame),
    #[cfg(feature = "native-rtc")]
    Raw(RawCaptureFrame),
}
#[cfg_attr(not(any(test, feature = "native-rtc")), allow(dead_code))]
impl FrameSink {
    fn encode(
        &self,
        kind: &str,
        generation: u64,
        image: DynamicImage,
        options: CaptureOptions,
        timestamp_us: i64,
    ) -> Result<CapturedFrame, String> {
        #[cfg(not(feature = "native-rtc"))]
        let _ = timestamp_us;
        match self {
            Self::Jpeg(_) => jpeg_frame(kind, generation, image, options).map(CapturedFrame::Jpeg),
            #[cfg(feature = "native-rtc")]
            Self::Raw(_) => {
                let (w, h) = options.quality.dimensions();
                let image = if image.width() > w || image.height() > h {
                    image.resize(w, h, FilterType::Triangle)
                } else {
                    image
                };
                if !valid_video_dimensions(image.width(), image.height()) {
                    return Err("Raw video dimensions exceed the configured bound".into());
                }
                let rgba = image.into_rgba8();
                Ok(CapturedFrame::Raw(RawCaptureFrame {
                    width: rgba.width(),
                    height: rgba.height(),
                    rgba: rgba.into_raw(),
                    timestamp_us,
                }))
            }
        }
    }
    fn send(&self, frame: CapturedFrame) -> Result<(), mpsc::TrySendError<()>> {
        fn erased<T>(
            result: Result<(), mpsc::TrySendError<T>>,
        ) -> Result<(), mpsc::TrySendError<()>> {
            result.map_err(|e| match e {
                mpsc::TrySendError::Full(_) => mpsc::TrySendError::Full(()),
                mpsc::TrySendError::Disconnected(_) => mpsc::TrySendError::Disconnected(()),
            })
        }
        match (self, frame) {
            (Self::Jpeg(tx), CapturedFrame::Jpeg(frame)) => erased(tx.try_send(frame)),
            #[cfg(feature = "native-rtc")]
            (Self::Raw(tx), CapturedFrame::Raw(frame)) => erased(tx.try_send(frame)),
            #[cfg(feature = "native-rtc")]
            _ => Err(mpsc::TrySendError::Disconnected(())),
        }
    }
}
#[cfg_attr(not(any(test, feature = "native-rtc")), allow(dead_code))]
impl CapturedFrame {
    fn dimensions(&self) -> (u32, u32) {
        match self {
            Self::Jpeg(f) => (f.width, f.height),
            #[cfg(feature = "native-rtc")]
            Self::Raw(f) => (f.width, f.height),
        }
    }
}

pub struct CaptureSession {
    stop: Arc<AtomicBool>,
    ready: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    errors: Arc<Mutex<Option<String>>>,
    diagnostics: Arc<Mutex<CaptureDiagnostics>>,
}

impl CaptureSession {
    #[cfg(all(test, feature = "native-rtc"))]
    pub(crate) fn synthetic_raw(
        tx: SyncSender<RawCaptureFrame>,
        generation: u64,
        options: CaptureOptions,
    ) -> Result<Self, String> {
        let (width, height) = options.quality.dimensions();
        Self::start(
            FrameSink::Raw(tx),
            "screen",
            generation,
            options,
            "Owned generated fixture".into(),
            move |_stop| {
                let mut index = 0_u8;
                Ok((
                    move || {
                        index = index.wrapping_add(1);
                        Ok(DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
                            width,
                            height,
                            image::Rgba([190, index, 35, 255]),
                        )))
                    },
                    "Generated RGBA, no device".into(),
                ))
            },
        )
    }
    // Historical JPEG fixture path. Production publishes only protected RTC.
    #[cfg(test)]
    #[allow(dead_code)]
    pub fn start_screen_source(
        tx: SyncSender<CaptureFrame>,
        generation: u64,
        source: CaptureSource,
        options: CaptureOptions,
    ) -> Result<Self, String> {
        Self::start_screen_output(FrameSink::Jpeg(tx), generation, source, options)
    }
    #[cfg(feature = "native-rtc")]
    pub fn start_screen_raw(
        tx: SyncSender<RawCaptureFrame>,
        generation: u64,
        source: CaptureSource,
        options: CaptureOptions,
    ) -> Result<Self, String> {
        Self::start_screen_output(FrameSink::Raw(tx), generation, source, options)
    }
    #[cfg(any(test, feature = "native-rtc"))]
    fn start_screen_output(
        tx: FrameSink,
        generation: u64,
        source: CaptureSource,
        options: CaptureOptions,
    ) -> Result<Self, String> {
        let name = source.name.clone();
        #[cfg(all(windows, feature = "native-rtc"))]
        if matches!(&tx, FrameSink::Raw(_)) {
            return Self::start(tx, "screen", generation, options, name, move |stop| {
                let mut capture = crate::graphics_capture::GraphicsCapture::open(&source, stop)?;
                Ok((
                    move || capture.capture(),
                    "Windows.Graphics.Capture · D3D11".into(),
                ))
            });
        }
        match source.kind {
            SourceKind::Monitor => {
                Self::start(tx, "screen", generation, options, name, move |_stop| {
                    let monitor = screenshots::Screen::all()
                        .map_err(|e| format!("Enumerate displays: {e}"))?
                        .into_iter()
                        .find(|screen| format!("monitor:{}", screen.display_info.id) == source.id)
                        .ok_or("The selected display is no longer available")?;
                    let format = format!(
                        "{}×{} native display",
                        monitor.display_info.width, monitor.display_info.height
                    );
                    Ok((
                        move || {
                            monitor
                                .capture()
                                .map(DynamicImage::ImageRgba8)
                                .map_err(|e| format!("Display capture failed: {e}"))
                        },
                        format,
                    ))
                })
            }
            SourceKind::Window => {
                #[cfg(windows)]
                {
                    Self::start(tx, "screen", generation, options, name, move |_stop| {
                        let mut window = native_windows::WindowCapture::open(&source)?;
                        Ok((
                            move || window.capture(),
                            "Selected window · PrintWindow".into(),
                        ))
                    })
                }
                #[cfg(not(windows))]
                {
                    let _ = (tx, generation, source, options, name);
                    Err("Window capture requires this app's Windows build".into())
                }
            }
            SourceKind::Camera => Err("Select a display or window to share".into()),
        }
    }

    // Historical JPEG fixture path. Production publishes only protected RTC.
    #[cfg(test)]
    #[allow(dead_code)]
    pub fn start_camera_source(
        tx: SyncSender<CaptureFrame>,
        generation: u64,
        source: CaptureSource,
        options: CaptureOptions,
    ) -> Result<Self, String> {
        Self::start_camera_output(FrameSink::Jpeg(tx), generation, source, options)
    }
    #[cfg(feature = "native-rtc")]
    pub fn start_camera_raw(
        tx: SyncSender<RawCaptureFrame>,
        generation: u64,
        source: CaptureSource,
        options: CaptureOptions,
    ) -> Result<Self, String> {
        Self::start_camera_output(FrameSink::Raw(tx), generation, source, options)
    }
    #[cfg(any(test, feature = "native-rtc"))]
    fn start_camera_output(
        tx: FrameSink,
        generation: u64,
        source: CaptureSource,
        options: CaptureOptions,
    ) -> Result<Self, String> {
        if source.kind != SourceKind::Camera {
            return Err("Select a camera device".into());
        }
        #[cfg(windows)]
        {
            let name = source.name.clone();
            Self::start(tx, "camera", generation, options, name, move |stop| {
                use nokhwa::{
                    pixel_format::RgbFormat,
                    utils::{
                        ApiBackend, CameraFormat, FrameFormat, RequestedFormat, RequestedFormatType,
                    },
                };
                let options = options.normalized();
                let cameras = nokhwa::query(ApiBackend::MediaFoundation)
                    .map_err(|e| format!("Enumerate cameras: {e}"))?;
                let info = cameras
                    .into_iter()
                    .find(|camera| format!("camera:{}", camera.index()) == source.id)
                    .ok_or("The selected camera is no longer available")?;
                let (width, height) = options.quality.dimensions();
                let formats = [
                    FrameFormat::MJPEG,
                    FrameFormat::YUYV,
                    FrameFormat::NV12,
                    FrameFormat::RAWRGB,
                ];
                let request = RequestedFormat::with_formats(
                    RequestedFormatType::Closest(CameraFormat::new_from(
                        width,
                        height,
                        FrameFormat::MJPEG,
                        options.fps,
                    )),
                    &formats,
                );
                let mut camera = nokhwa::Camera::with_backend(info.index().clone(), request, ApiBackend::MediaFoundation)
                    .map_err(|e| format!("Open camera: {e}. Check camera access and whether another app is using it."))?;
                // Query the real device modes before choosing a supported resolution/FPS.
                if let Ok(available) = camera.compatible_camera_formats() {
                    if let Some(selected) = choose_camera_format(&available, options) {
                        let supported = [selected.format()];
                        camera
                            .set_camera_requset(RequestedFormat::with_formats(
                                RequestedFormatType::Exact(selected),
                                &supported,
                            ))
                            .map_err(|e| format!("Set supported camera mode: {e}"))?;
                    }
                }
                let actual = camera.camera_format();
                let format = format!(
                    "{}×{} · {} fps · {}",
                    actual.width(),
                    actual.height(),
                    actual.frame_rate(),
                    actual.format()
                );
                if stop.load(Ordering::Acquire) {
                    return Err("Camera request cancelled".into());
                }
                camera
                    .open_stream()
                    .map_err(|e| format!("Start camera: {e}"))?;
                Ok((
                    move || {
                        let buffer = camera.frame().map_err(|e| format!("Read camera: {e}"))?;
                        if buffer.source_frame_format() == FrameFormat::MJPEG {
                            let mut reader = image::io::Reader::with_format(
                                std::io::Cursor::new(buffer.buffer()),
                                image::ImageFormat::Jpeg,
                            );
                            let mut limits = image::io::Limits::default();
                            limits.max_image_width = Some(4096);
                            limits.max_image_height = Some(4096);
                            limits.max_alloc = Some(64 * 1024 * 1024);
                            reader.limits(limits);
                            reader
                                .decode()
                                .map_err(|e| format!("Decode camera MJPEG: {e}"))
                        } else {
                            let rgb = buffer
                                .decode_image::<RgbFormat>()
                                .map_err(|e| format!("Unsupported camera frame format: {e}"))?;
                            image::RgbImage::from_raw(rgb.width(), rgb.height(), rgb.into_raw())
                                .map(DynamicImage::ImageRgb8)
                                .ok_or("Invalid camera frame dimensions".into())
                        }
                    },
                    format,
                ))
            })
        }
        #[cfg(not(windows))]
        {
            let _ = (tx, generation, source, options);
            Err("Camera capture requires this app's Windows build".into())
        }
    }

    #[cfg_attr(not(any(test, feature = "native-rtc")), allow(dead_code))]
    fn start<F, G>(
        tx: FrameSink,
        kind: &'static str,
        generation: u64,
        options: CaptureOptions,
        source: String,
        setup: F,
    ) -> Result<Self, String>
    where
        F: FnOnce(Arc<AtomicBool>) -> Result<(G, String), String> + Send + 'static,
        G: FnMut() -> Result<DynamicImage, String> + 'static,
    {
        let mut options = options.normalized();
        if matches!(&tx, FrameSink::Jpeg(_)) {
            options.fps = options.fps.min(30);
        }
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let errors = Arc::new(Mutex::new(None));
        let worker_errors = errors.clone();
        let diagnostics = Arc::new(Mutex::new(CaptureDiagnostics {
            target_fps: options.fps,
            source,
            ..Default::default()
        }));
        let worker_stats = diagnostics.clone();
        let ready = Arc::new(AtomicBool::new(false));
        let worker_ready = ready.clone();
        let worker = thread::Builder::new()
            .name(format!("relay-{kind}"))
            .spawn(move || {
                if worker_stop.load(Ordering::Acquire) {
                    return;
                }
                let (mut capture, format) = match setup(worker_stop.clone()) {
                    Ok(value) => value,
                    Err(error) => {
                        if !worker_stop.load(Ordering::Acquire) {
                            if let Ok(mut slot) = worker_errors.lock() {
                                *slot = Some(error);
                            }
                        }
                        return;
                    }
                };
                if worker_stop.load(Ordering::Acquire) {
                    return;
                }
                if let Ok(mut stats) = worker_stats.lock() {
                    stats.device_format = format;
                }
                let target_interval = Duration::from_secs_f64(1.0 / options.fps as f64);
                let capture_clock = Instant::now();
                let mut measured_since = Instant::now();
                let mut measured_frames = 0;
                while !worker_stop.load(Ordering::Acquire) {
                    let started = Instant::now();
                    let result = capture().and_then(|image| {
                        let encode_started = Instant::now();
                        let frame = tx.encode(
                            kind,
                            generation,
                            image,
                            options,
                            capture_clock.elapsed().as_micros().min(i64::MAX as u128) as i64,
                        )?;
                        Ok((frame, encode_started.elapsed()))
                    });
                    let mut congestion = false;
                    match result {
                        Ok((frame, elapsed)) => {
                            if worker_stop.load(Ordering::Acquire) {
                                break;
                            }
                            worker_ready.store(true, Ordering::Release);
                            let (width, height) = frame.dimensions();
                            match tx.send(frame) {
                                Ok(()) => {
                                    measured_frames += 1;
                                    if let Ok(mut stats) = worker_stats.lock() {
                                        stats.frames += 1;
                                        stats.actual_width = width;
                                        stats.actual_height = height;
                                        stats.encode_ms = elapsed.as_secs_f32() * 1000.0;
                                    }
                                }
                                Err(mpsc::TrySendError::Full(_)) => {
                                    congestion = true;
                                    if let Ok(mut stats) = worker_stats.lock() {
                                        stats.dropped_frames += 1;
                                    }
                                }
                                Err(mpsc::TrySendError::Disconnected(_)) => break,
                            }
                        }
                        Err(error) => {
                            worker_ready.store(false, Ordering::Release);
                            if !worker_stop.load(Ordering::Acquire) {
                                if let Ok(mut slot) = worker_errors.lock() {
                                    *slot = Some(error);
                                }
                            }
                            break;
                        }
                    }
                    if measured_since.elapsed() >= Duration::from_secs(1) {
                        if let Ok(mut stats) = worker_stats.lock() {
                            stats.delivered_fps =
                                measured_frames as f32 / measured_since.elapsed().as_secs_f32();
                        }
                        measured_since = Instant::now();
                        measured_frames = 0;
                    }
                    // Pace to the requested ceiling. Expensive capture/encoding and a full network
                    // queue lower the actual rate; never accumulate frames or spin at full CPU.
                    let interval = if congestion {
                        target_interval * 2
                    } else {
                        target_interval
                    };
                    let delay = interval
                        .saturating_sub(started.elapsed())
                        .max(started.elapsed() / 2);
                    let until = Instant::now() + delay;
                    while !worker_stop.load(Ordering::Acquire) && Instant::now() < until {
                        thread::sleep(Duration::from_millis(2));
                    }
                }
            })
            .map_err(|e| format!("Start {kind} worker: {e}"))?;
        Ok(Self {
            stop,
            ready,
            worker: Some(worker),
            errors,
            diagnostics,
        })
    }

    pub fn is_starting(&self) -> bool {
        !self.stop.load(Ordering::Acquire)
            && !self.ready.load(Ordering::Acquire)
            && self
                .worker
                .as_ref()
                .is_some_and(|worker| !worker.is_finished())
    }
    pub fn is_active(&self) -> bool {
        !self.stop.load(Ordering::Acquire)
            && self.ready.load(Ordering::Acquire)
            && self
                .worker
                .as_ref()
                .is_some_and(|worker| !worker.is_finished())
    }

    pub fn diagnostics(&self) -> CaptureDiagnostics {
        self.diagnostics
            .lock()
            .map(|stats| stats.clone())
            .unwrap_or_default()
    }
    pub fn take_error(&self) -> Option<String> {
        self.errors.lock().ok()?.take()
    }
    /// Stop publication immediately; join/release native handles on a background thread.
    pub fn cancel(&self) {
        self.stop.store(true, Ordering::Release);
        self.ready.store(false, Ordering::Release);
    }
    pub fn stop(&mut self) {
        self.cancel();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
impl Drop for CaptureSession {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(windows)]
#[cfg_attr(not(any(test, feature = "native-rtc")), allow(dead_code))]
fn choose_camera_format(
    formats: &[nokhwa::utils::CameraFormat],
    options: CaptureOptions,
) -> Option<nokhwa::utils::CameraFormat> {
    use nokhwa::utils::FrameFormat;
    let (width, height) = options.quality.dimensions();
    formats
        .iter()
        .copied()
        .filter(|format| {
            format.width() > 0
                && format.height() > 0
                && format.width() <= 4096
                && format.height() <= 4096
                && format.frame_rate() > 0
                && matches!(
                    format.format(),
                    FrameFormat::MJPEG
                        | FrameFormat::YUYV
                        | FrameFormat::NV12
                        | FrameFormat::RAWRGB
                )
        })
        .min_by_key(|format| {
            (
                format.frame_rate().abs_diff(options.fps),
                format.width().abs_diff(width) as u64 + format.height().abs_diff(height) as u64,
                if format.format() == FrameFormat::MJPEG {
                    0
                } else {
                    1
                },
            )
        })
}

#[cfg_attr(not(any(test, feature = "native-rtc")), allow(dead_code))]
fn jpeg_frame(
    kind: &str,
    generation: u64,
    image: DynamicImage,
    options: CaptureOptions,
) -> Result<CaptureFrame, String> {
    let options = options.normalized();
    let (width, height) = options.quality.dimensions();
    let mut rgb = if image.width() > width || image.height() > height {
        image.resize(width, height, FilterType::Triangle).to_rgb8()
    } else {
        image.to_rgb8()
    };
    for _ in 0..4 {
        let mut quality = options.jpeg_quality;
        loop {
            let mut jpeg = Vec::new();
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, quality)
                .encode(
                    rgb.as_raw(),
                    rgb.width(),
                    rgb.height(),
                    image::ColorType::Rgb8,
                )
                .map_err(|e| format!("Encode shared frame: {e}"))?;
            if jpeg.len() <= MAX_FRAME_BYTES {
                return Ok(CaptureFrame {
                    kind: kind.into(),
                    generation,
                    jpeg,
                    width: rgb.width(),
                    height: rgb.height(),
                });
            }
            if quality <= 20 {
                break;
            }
            quality = ((u16::from(quality) * 3 / 4) as u8).max(20);
        }
        rgb = image::imageops::resize(
            &rgb,
            (rgb.width() * 3 / 4).max(1),
            (rgb.height() * 3 / 4).max(1),
            FilterType::Triangle,
        );
    }
    Err("This frame still exceeds the sharing limit. Choose a lower quality profile.".into())
}

#[cfg(windows)]
mod native_windows {
    use super::*;
    use windows::Win32::{
        Foundation::{BOOL, HWND, LPARAM, RECT},
        Graphics::{
            Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED},
            Gdi::*,
        },
        Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS},
        UI::WindowsAndMessaging::*,
    };

    unsafe fn owner_process(hwnd: HWND) -> Option<u32> {
        let mut process_id = 0;
        if GetWindowThreadProcessId(hwnd, Some(&mut process_id)) == 0 || process_id == 0 {
            None
        } else {
            Some(process_id)
        }
    }

    pub(super) fn enumerate() -> Result<Vec<CaptureSource>, String> {
        unsafe extern "system" fn visit(hwnd: HWND, data: LPARAM) -> BOOL {
            if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
                return BOOL(1);
            }
            let mut cloaked = 0u32;
            if DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, (&mut cloaked as *mut u32).cast(), 4)
                .is_ok()
                && cloaked != 0
            {
                return BOOL(1);
            }
            let len = GetWindowTextLengthW(hwnd);
            if len <= 0 {
                return BOOL(1);
            }
            let mut text = vec![0u16; (len as usize).min(512) + 1];
            let used = GetWindowTextW(hwnd, &mut text);
            let mut rect = RECT::default();
            if used == 0
                || GetWindowRect(hwnd, &mut rect).is_err()
                || rect.right <= rect.left
                || rect.bottom <= rect.top
            {
                return BOOL(1);
            }
            let Some(process_id) = owner_process(hwnd) else {
                return BOOL(1);
            };
            let sources = &mut *(data.0 as *mut Vec<CaptureSource>);
            sources.push(CaptureSource {
                id: format!("window:{:x}:{process_id}", hwnd.0 as usize),
                name: String::from_utf16_lossy(&text[..used as usize]),
                kind: SourceKind::Window,
                width: (rect.right - rect.left) as u32,
                height: (rect.bottom - rect.top) as u32,
            });
            BOOL(1)
        }
        let mut sources: Vec<CaptureSource> = Vec::new();
        unsafe {
            EnumWindows(
                Some(visit),
                LPARAM((&mut sources as *mut Vec<CaptureSource>) as isize),
            )
            .map_err(|e| format!("Enumerate windows: {e}"))?;
        }
        sources.sort_by_key(|a| a.name.to_lowercase());
        Ok(sources)
    }

    #[cfg_attr(not(any(test, feature = "native-rtc")), allow(dead_code))]
    struct Surface {
        dc: HDC,
        bitmap: HBITMAP,
        previous: HGDIOBJ,
        width: i32,
        height: i32,
    }
    #[cfg_attr(not(any(test, feature = "native-rtc")), allow(dead_code))]
    impl Surface {
        unsafe fn new(source: HDC, width: i32, height: i32) -> Result<Self, String> {
            let dc = CreateCompatibleDC(source);
            if dc.is_invalid() {
                return Err("Cannot create window capture context".into());
            }
            let bitmap = CreateCompatibleBitmap(source, width, height);
            if bitmap.is_invalid() {
                let _ = DeleteDC(dc);
                return Err("Cannot allocate window capture bitmap".into());
            }
            let previous = SelectObject(dc, bitmap);
            Ok(Self {
                dc,
                bitmap,
                previous,
                width,
                height,
            })
        }
    }
    impl Drop for Surface {
        fn drop(&mut self) {
            unsafe {
                SelectObject(self.dc, self.previous);
                let _ = DeleteObject(self.bitmap);
                let _ = DeleteDC(self.dc);
            }
        }
    }
    #[cfg_attr(not(any(test, feature = "native-rtc")), allow(dead_code))]
    pub(super) struct WindowCapture {
        hwnd: HWND,
        process_id: u32,
        surface: Option<Surface>,
    }
    #[cfg_attr(not(any(test, feature = "native-rtc")), allow(dead_code))]
    impl WindowCapture {
        pub(super) fn open(source: &CaptureSource) -> Result<Self, String> {
            if !enumerate()?.iter().any(|window| window.id == source.id) {
                return Err("The selected window is closed, replaced, hidden or minimized".into());
            }
            let (handle, owner) = source
                .id
                .strip_prefix("window:")
                .and_then(|value| value.split_once(':'))
                .ok_or("Invalid window source")?;
            let raw = usize::from_str_radix(handle, 16).map_err(|_| "Invalid window source")?;
            let process_id = owner.parse::<u32>().map_err(|_| "Invalid window owner")?;
            let hwnd = HWND(raw as *mut std::ffi::c_void);
            if !unsafe { IsWindow(hwnd).as_bool() && owner_process(hwnd) == Some(process_id) } {
                return Err("The selected window was closed or replaced. Choose it again.".into());
            }
            Ok(Self {
                hwnd,
                process_id,
                surface: None,
            })
        }
        pub(super) fn capture(&mut self) -> Result<DynamicImage, String> {
            unsafe {
                if !IsWindow(self.hwnd).as_bool()
                    || owner_process(self.hwnd) != Some(self.process_id)
                    || !IsWindowVisible(self.hwnd).as_bool()
                    || IsIconic(self.hwnd).as_bool()
                {
                    return Err("The shared window is closed, replaced, hidden or minimized. Restore it or choose another source.".into());
                }
                let mut affinity = 0u32;
                if GetWindowDisplayAffinity(self.hwnd, &mut affinity).is_ok() && affinity != 0 {
                    return Err("This window prevents screen capture".into());
                }
                let mut rect = RECT::default();
                GetWindowRect(self.hwnd, &mut rect)
                    .map_err(|e| format!("Read window dimensions: {e}"))?;
                let (width, height) = (rect.right - rect.left, rect.bottom - rect.top);
                if width <= 0
                    || height <= 0
                    || width > 8192
                    || height > 8192
                    || i64::from(width) * i64::from(height) > 32_000_000
                {
                    return Err("The selected window has unsupported capture dimensions".into());
                }
                if self
                    .surface
                    .as_ref()
                    .is_none_or(|surface| surface.width != width || surface.height != height)
                {
                    let source_dc = GetWindowDC(self.hwnd);
                    if source_dc.is_invalid() {
                        return Err("Cannot access the selected window".into());
                    }
                    let surface = Surface::new(source_dc, width, height);
                    ReleaseDC(self.hwnd, source_dc);
                    self.surface = Some(surface?);
                }
                let surface = self.surface.as_ref().unwrap();
                let mut pixels = vec![0u8; width as usize * height as usize * 4];
                let mut info = BITMAPINFO {
                    bmiHeader: BITMAPINFOHEADER {
                        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                        biWidth: width,
                        biHeight: -height,
                        biPlanes: 1,
                        biBitCount: 32,
                        biCompression: BI_RGB.0,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                // The compositor flag supports modern windows; classic WM_PRINT
                // renderers can require the plain flag. Both target this HWND only.
                for flags in [PW_RENDERFULLCONTENT, 0] {
                    if owner_process(self.hwnd) != Some(self.process_id) {
                        return Err(
                            "The shared window was closed or replaced. Choose another source."
                                .into(),
                        );
                    }
                    if !PrintWindow(self.hwnd, surface.dc, PRINT_WINDOW_FLAGS(flags)).as_bool() {
                        continue;
                    }
                    SelectObject(surface.dc, surface.previous);
                    let lines = GetDIBits(
                        surface.dc,
                        surface.bitmap,
                        0,
                        height as u32,
                        Some(pixels.as_mut_ptr().cast()),
                        &mut info,
                        DIB_RGB_COLORS,
                    );
                    SelectObject(surface.dc, surface.bitmap);
                    // A recycled HWND must not publish pixels from a replacement app.
                    if owner_process(self.hwnd) != Some(self.process_id) {
                        return Err(
                            "The shared window was closed or replaced. Choose another source."
                                .into(),
                        );
                    }
                    if lines != height {
                        return Err("Cannot read the selected window's frame".into());
                    }
                    if pixels.chunks_exact(4).all(|pixel| pixel[..3] == [0, 0, 0]) {
                        continue;
                    }
                    for pixel in pixels.chunks_exact_mut(4) {
                        pixel.swap(0, 2);
                        pixel[3] = 255;
                    }
                    return image::RgbaImage::from_raw(width as u32, height as u32, pixels)
                        .map(DynamicImage::ImageRgba8)
                        .ok_or("Invalid window frame dimensions".into());
                }
                Err("This window returned a blank or unsupported capture frame. Protected or GPU-rendered apps may require display sharing.".into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn quality_profiles_bound_dimensions_and_preserve_aspect_ratio() {
        for (quality, dimensions) in [
            (CaptureQuality::P480, (853, 480)),
            (CaptureQuality::P720, (1280, 720)),
            (CaptureQuality::P1080, (1920, 1080)),
            (CaptureQuality::P1440, (2560, 1440)),
            (CaptureQuality::P2160, (3840, 2160)),
            (CaptureQuality::Source, (3840, 2160)),
        ] {
            let frame = jpeg_frame(
                "screen",
                42,
                DynamicImage::new_rgb8(3840, 2160),
                CaptureOptions {
                    quality,
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!((frame.width, frame.height), dimensions);
            assert_eq!(frame.generation, 42);
            assert!(frame.jpeg.len() <= MAX_FRAME_BYTES);
            assert!(image::load_from_memory(&frame.jpeg).is_ok());
        }
    }
    #[test]
    fn options_clamp_frame_rate_and_image_quality() {
        let options = CaptureOptions {
            fps: 120,
            jpeg_quality: 255,
            ..Default::default()
        }
        .normalized();
        assert_eq!((options.fps, options.jpeg_quality), (60, 90));
        assert_eq!(
            CaptureOptions {
                fps: 0,
                jpeg_quality: 0,
                ..Default::default()
            }
            .normalized()
            .fps,
            15
        );
        for fps in [15, 24, 30, 60] {
            assert_eq!(
                CaptureOptions {
                    fps,
                    ..Default::default()
                }
                .normalized()
                .fps,
                fps
            );
        }
    }

    #[test]
    fn old_capture_preferences_default_to_automatic_bitrate_and_new_choices_round_trip() {
        let old: CaptureOptions =
            serde_json::from_str(r#"{"quality":"P1080","fps":30,"jpeg_quality":70}"#).unwrap();
        assert_eq!(old.bitrate, VideoBitrate::Auto);
        for quality in [
            CaptureQuality::P1440,
            CaptureQuality::P2160,
            CaptureQuality::Source,
        ] {
            let options = CaptureOptions {
                quality,
                fps: 24,
                bitrate: VideoBitrate::Mbps20,
                ..Default::default()
            };
            assert_eq!(
                serde_json::from_str::<CaptureOptions>(&serde_json::to_string(&options).unwrap())
                    .unwrap(),
                options
            );
        }
    }

    #[test]
    fn bitrate_tracks_actual_resolution_and_frame_rate_with_a_manual_ceiling() {
        for (quality, expected) in [
            (CaptureQuality::P480, 800_000),
            (CaptureQuality::P720, 4_000_000),
            (CaptureQuality::P1080, 4_000_000),
            (CaptureQuality::P1440, 8_000_000),
            (CaptureQuality::P2160, 16_000_000),
        ] {
            assert_eq!(
                CaptureOptions {
                    quality,
                    ..Default::default()
                }
                .target_bitrate(),
                expected
            );
        }
        let source = CaptureOptions {
            quality: CaptureQuality::Source,
            fps: 60,
            ..Default::default()
        };
        assert_eq!(source.target_bitrate(), 32_000_000);
        assert_eq!(source.target_bitrate_for(1280, 720, 30), 4_000_000);
        assert_eq!(source.target_bitrate_for(2560, 1440, 24), 6_400_000);
        assert_eq!(
            source.target_bitrate_for(u32::MAX, u32::MAX, u32::MAX),
            32_000_000
        );
        for (bitrate, expected) in [
            (VideoBitrate::Mbps2, 2_000_000),
            (VideoBitrate::Mbps4, 4_000_000),
            (VideoBitrate::Mbps8, 8_000_000),
            (VideoBitrate::Mbps12, 12_000_000),
            (VideoBitrate::Mbps20, 20_000_000),
            (VideoBitrate::Mbps32, 32_000_000),
        ] {
            assert_eq!(
                CaptureOptions { bitrate, ..source }.target_bitrate_for(3840, 2160, 60),
                expected
            );
        }
    }

    #[test]
    fn video_dimensions_reject_oversized_or_empty_untrusted_frames() {
        assert!(valid_video_dimensions(3840, 2160));
        assert!(valid_video_dimensions(2160, 2160));
        assert!(!valid_video_dimensions(0, 2160));
        assert!(!valid_video_dimensions(3840, 0));
        assert!(!valid_video_dimensions(3841, 2160));
        assert!(!valid_video_dimensions(3840, 2161));
        assert!(!valid_video_dimensions(u32::MAX, u32::MAX));
    }

    #[cfg(feature = "native-rtc")]
    #[test]
    fn raw_source_preserves_pixels_without_upscaling_and_fits_portrait_to_the_ceiling() {
        let (tx, _rx) = mpsc::sync_channel(1);
        let sink = FrameSink::Raw(tx);
        let options = CaptureOptions {
            quality: CaptureQuality::Source,
            ..Default::default()
        };
        let CapturedFrame::Raw(frame) = sink
            .encode("screen", 1, DynamicImage::new_rgba8(640, 480), options, 10)
            .unwrap()
        else {
            panic!("Expected a raw frame")
        };
        assert_eq!(
            (frame.width, frame.height, frame.timestamp_us),
            (640, 480, 10)
        );
        assert_eq!(frame.rgba.len(), 640 * 480 * 4);
        let CapturedFrame::Raw(frame) = sink
            .encode(
                "screen",
                1,
                DynamicImage::new_rgba8(2160, 3840),
                options,
                20,
            )
            .unwrap()
        else {
            panic!("Expected a raw frame")
        };
        assert_eq!((frame.width, frame.height), (1215, 2160));
        assert!(valid_video_dimensions(frame.width, frame.height));
        let CapturedFrame::Raw(frame) = sink
            .encode(
                "screen",
                1,
                DynamicImage::new_rgba8(3840, 2160),
                options,
                30,
            )
            .unwrap()
        else {
            panic!("Expected a raw frame")
        };
        assert_eq!((frame.width, frame.height), (3840, 2160));
        assert_eq!(frame.rgba.len() as u64, MAX_VIDEO_PIXELS * 4);
        assert!(sink
            .encode("screen", 1, DynamicImage::new_rgba8(0, 1), options, 40)
            .is_err());
    }

    #[test]
    fn noisy_high_quality_frames_are_adapted_to_the_transport_limit() {
        let mut seed = 0x1234_5678u32;
        let pixels = (0..1280 * 720 * 3)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                seed as u8
            })
            .collect();
        let image = DynamicImage::ImageRgb8(image::RgbImage::from_raw(1280, 720, pixels).unwrap());
        let frame = jpeg_frame(
            "camera",
            7,
            image,
            CaptureOptions {
                jpeg_quality: 90,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(frame.jpeg.len() <= MAX_FRAME_BYTES);
        assert!(frame.width <= 1280 && frame.height <= 720);
        let decoded = image::load_from_memory(&frame.jpeg).unwrap();
        assert_eq!(
            (decoded.width(), decoded.height()),
            (frame.width, frame.height)
        );
    }

    #[test]
    fn full_outgoing_queue_drops_frames_instead_of_blocking_capture() {
        let (tx, rx) = mpsc::sync_channel(1);
        let mut session = CaptureSession::start(
            FrameSink::Jpeg(tx),
            "screen",
            17,
            CaptureOptions::default(),
            "Synthetic test".into(),
            |_stop| Ok((|| Ok(DynamicImage::new_rgb8(1, 1)), "Test frames".into())),
        )
        .unwrap();
        thread::sleep(Duration::from_millis(250));
        session.cancel();
        session.stop();
        let diagnostics = session.diagnostics();
        assert_eq!(diagnostics.frames, 1);
        assert!(diagnostics.dropped_frames > 0);
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap().generation,
            17
        );
        assert!(rx.try_recv().is_err());
    }
    #[test]
    fn cancel_during_capture_prevents_late_publication_without_devices() {
        let (tx, rx) = mpsc::sync_channel(2);
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (resume_tx, resume_rx) = mpsc::sync_channel(1);
        let mut session = CaptureSession::start(
            FrameSink::Jpeg(tx),
            "screen",
            99,
            CaptureOptions::default(),
            "Synthetic test".into(),
            move |_stop| {
                let mut first = true;
                Ok((
                    move || {
                        if first {
                            first = false;
                        } else {
                            let _ = entered_tx.send(());
                            let _ = resume_rx.recv_timeout(Duration::from_secs(2));
                        }
                        Ok(DynamicImage::new_rgb8(1, 1))
                    },
                    "Test frames".into(),
                ))
            },
        )
        .unwrap();
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap().generation,
            99
        );
        entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        session.cancel();
        resume_tx.send(()).unwrap();
        session.stop();
        assert!(rx.try_recv().is_err());
        assert!(session.worker.is_none());
    }
    #[cfg(windows)]
    #[test]
    fn camera_mode_selection_uses_real_formats_and_requested_fps() {
        use nokhwa::utils::{CameraFormat, FrameFormat};
        let modes = [
            CameraFormat::new_from(1280, 720, FrameFormat::MJPEG, 15),
            CameraFormat::new_from(640, 480, FrameFormat::YUYV, 30),
            CameraFormat::new_from(1280, 720, FrameFormat::MJPEG, 30),
        ];
        assert_eq!(
            choose_camera_format(&modes, CaptureOptions::default()),
            Some(modes[2])
        );
        assert_eq!(
            choose_camera_format(&modes[..2], CaptureOptions::default()),
            Some(modes[1])
        );
        assert_eq!(
            choose_camera_format(
                &modes,
                CaptureOptions {
                    fps: 15,
                    ..Default::default()
                }
            ),
            Some(modes[0])
        );
        // A device that only advertises 15 FPS must not be reported as a 30 FPS mode.
        assert_eq!(
            choose_camera_format(&modes[..1], CaptureOptions::default()),
            Some(modes[0])
        );
        assert!(choose_camera_format(&[], CaptureOptions::default()).is_none());
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "briefly creates a controlled native window; captures only that test window"]
    fn actual_native_window_capture_and_minimized_rejection() {
        use windows::{
            core::w,
            Win32::{
                Foundation::{HWND, LPARAM, LRESULT, WPARAM},
                Graphics::Gdi::{
                    BeginPaint, EndPaint, FillRect, GetStockObject, UpdateWindow, HBRUSH, HDC,
                    PAINTSTRUCT, WHITE_BRUSH,
                },
                UI::WindowsAndMessaging::*,
            },
        };
        unsafe extern "system" fn paint(
            hwnd: HWND,
            message: u32,
            wparam: WPARAM,
            lparam: LPARAM,
        ) -> LRESULT {
            if message == WM_PRINT || message == WM_PRINTCLIENT || message == WM_PAINT {
                let mut rect = windows::Win32::Foundation::RECT::default();
                let mut paint = PAINTSTRUCT::default();
                let dc = if message == WM_PAINT {
                    BeginPaint(hwnd, &mut paint)
                } else {
                    HDC(wparam.0 as *mut std::ffi::c_void)
                };
                if GetClientRect(hwnd, &mut rect).is_ok() {
                    FillRect(dc, &rect, HBRUSH(GetStockObject(WHITE_BRUSH).0));
                }
                if message == WM_PAINT {
                    let _ = EndPaint(hwnd, &paint);
                }
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
        struct TestWindow(HWND);
        impl Drop for TestWindow {
            fn drop(&mut self) {
                unsafe {
                    let _ = DestroyWindow(self.0);
                }
            }
        }
        // A small white native window, painted before capture and closed on return.
        // No existing app, desktop, camera or microphone is captured by this test.
        unsafe {
            RegisterClassW(&WNDCLASSW {
                lpfnWndProc: Some(paint),
                lpszClassName: w!("RelayCaptureControlledWindow"),
                ..Default::default()
            });
        }
        let test = unsafe {
            TestWindow(
                CreateWindowExW(
                    WS_EX_TOOLWINDOW,
                    w!("RelayCaptureControlledWindow"),
                    w!("Relay controlled capture test"),
                    WS_POPUP | WS_VISIBLE,
                    0,
                    0,
                    320,
                    240,
                    None,
                    None,
                    None,
                    None,
                )
                .unwrap(),
            )
        };
        unsafe {
            let _ = UpdateWindow(test.0);
            let _ = windows::Win32::Graphics::Dwm::DwmFlush();
        }
        let source = native_windows::enumerate()
            .unwrap()
            .into_iter()
            .find(|source| {
                source.id == format!("window:{:x}:{}", test.0 .0 as usize, std::process::id())
            })
            .unwrap();
        let mut replaced = source.clone();
        replaced.id = format!(
            "window:{:x}:{}",
            test.0 .0 as usize,
            std::process::id().wrapping_add(1)
        );
        assert!(native_windows::WindowCapture::open(&replaced).is_err());
        let mut capture = native_windows::WindowCapture::open(&source).unwrap();
        let image = capture.capture().unwrap();
        assert_eq!((image.width(), image.height()), (320, 240));
        assert!(image
            .to_rgb8()
            .pixels()
            .any(|pixel| pixel.0 == [255, 255, 255]));
        #[cfg(feature = "native-rtc")]
        {
            let stop = Arc::new(AtomicBool::new(false));
            let mut graphics =
                crate::graphics_capture::GraphicsCapture::open(&source, stop.clone()).unwrap();
            let frame = graphics.capture().unwrap();
            assert!(frame.width() > 0 && frame.height() > 0);
            assert!(frame
                .to_rgb8()
                .pixels()
                .any(|pixel| pixel.0.iter().any(|c| *c > 100)));
            assert!(graphics.last_system_relative_time > 0);
            stop.store(true, Ordering::Release);
            assert!(graphics.capture().unwrap_err().contains("cancelled"));
            let (sender, _receiver) = mpsc::sync_channel(4);
            let audio = crate::process_audio::ProcessAudioSession::start(&source, sender).unwrap();
            let deadline = Instant::now() + Duration::from_secs(6);
            while !audio.is_active() {
                if let Some(error) = audio.take_error() {
                    panic!("Owned-process virtual loopback activation: {error}");
                }
                assert!(
                    Instant::now() < deadline,
                    "Owned-process loopback activation timed out"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            audio.cancel();
            assert!(!audio.is_active());
            drop(audio);
        }
        unsafe {
            let _ = ShowWindow(test.0, SW_MINIMIZE);
        }
        assert!(capture.capture().unwrap_err().contains("minimized"));
        unsafe {
            let _ = DestroyWindow(test.0);
        }
        assert!(capture.capture().unwrap_err().contains("closed"));
    }
}
