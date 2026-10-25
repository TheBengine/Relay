//! Explicit, device-free-until-selected native WebRTC video sessions.
use crate::adaptive_video::{AdaptiveVideo, VideoPressure, VideoProfile};
use crate::application_audio::ApplicationAudioSink;
use crate::capture::{
    valid_video_dimensions, CaptureDiagnostics, CaptureOptions, CaptureSession, CaptureSource,
    RawCaptureFrame, VideoCodecPreference, MAX_VIDEO_PIXELS,
};
use crate::e2ee::CallKeys;
#[cfg(target_os = "windows")]
use crate::hardware_encoder::HardwareEncoder;
#[cfg(target_os = "windows")]
use crate::process_audio::{ProcessAudioFrame, ProcessAudioSession};
use eframe::egui::{ColorImage, Context};
use futures_util::{FutureExt, StreamExt};
use livekit::e2ee::{
    key_provider::{KeyProvider, KeyProviderOptions},
    E2eeOptions, EncryptionType,
};
#[cfg(target_os = "windows")]
use livekit::webrtc::video_frame::{EncodedFrameType, EncodedVideoCodec, EncodedVideoFrame};
use livekit::webrtc::{
    audio_frame::AudioFrame,
    audio_source::{native::NativeAudioSource, AudioSourceOptions, RtcAudioSource},
    audio_stream::native::{NativeAudioStream, NativeAudioStreamOptions},
};
use livekit::webrtc::{
    native::yuv_helper,
    prelude::{
        I420Buffer, RtcVideoSource, VideoFormatType, VideoFrame, VideoResolution, VideoRotation,
    },
    video_source::native::NativeVideoSource,
    video_stream::native::NativeVideoStream,
};
use livekit::{
    options::{TrackPublishOptions, VideoCodec, VideoEncoderBackend, VideoEncoding},
    prelude::*,
};
use relay_protocol::RtcTicket;
use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const MAX_VIDEO_BYTES: usize = 64 * 1024 * 1024;
#[cfg(test)]
#[path = "rtc_embedded_tests.rs"]
mod embedded_tests;
const MAX_STREAMS: usize = 16;
static NEXT_CONNECTION: AtomicU64 = AtomicU64::new(1);
#[path = "rtc_handover.rs"]
pub(crate) mod handover_probe;
const RTC_ENDED: &str = "Video connection closed. Choose Connect Video to reconnect, then select your camera or screen again.";
const RTC_RECOVERY_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_SOURCE_GENERATION: u64 = u64::MAX >> 2;

// Incoming I420 buffers use ceil-sized chroma planes, so older senders may
// retain odd source dimensions. Bound their packed RGBA output before conversion.
fn rgba_bytes(width: u32, height: u32) -> Option<usize> {
    if !valid_video_dimensions(width, height) {
        return None;
    }
    (width as usize)
        .checked_mul(height as usize)?
        .checked_mul(4)
}

// Outgoing hardware NV12 uses even dimensions and tightly packed chroma rows.
fn rtc_rgba_bytes(width: u32, height: u32) -> Option<usize> {
    if width < 2 || height < 2 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        return None;
    }
    rgba_bytes(width, height)
}

fn publication_dimensions(options: CaptureOptions, source: &CaptureSource) -> (u32, u32) {
    let (max_width, max_height) = options.quality.dimensions();
    if source.width < 2 || source.height < 2 {
        return (max_width, max_height);
    }
    let scale = (f64::from(max_width) / f64::from(source.width))
        .min(f64::from(max_height) / f64::from(source.height))
        .min(1.0);
    let width = (f64::from(source.width) * scale).floor() as u32 & !1;
    let height = (f64::from(source.height) * scale).floor() as u32 & !1;
    if rtc_rgba_bytes(width, height).is_some() {
        (width, height)
    } else {
        (max_width, max_height)
    }
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn hardware_video_bitrate(
    options: CaptureOptions,
    width: u32,
    height: u32,
    fps: u32,
    requested: Option<u64>,
) -> u32 {
    let ceiling = u64::from(options.target_bitrate_for(width, height, fps));
    requested.unwrap_or(ceiling).clamp(100_000, ceiling) as u32
}

// Phase and ownership change atomically: an old worker cannot restore sharing
// after Stop, or clear the status of a newer source request.
fn source_phase(state: &AtomicU64, generation: u64) -> u64 {
    let value = state.load(Ordering::Acquire);
    if value >> 2 == generation {
        value & 3
    } else {
        0
    }
}

fn set_source_phase(state: &AtomicU64, generation: u64, phase: u64) -> bool {
    debug_assert!(generation <= MAX_SOURCE_GENERATION && phase <= 3);
    state
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
            (value != 0 && value >> 2 == generation).then_some(if phase == 0 {
                0
            } else {
                (generation << 2) | phase
            })
        })
        .is_ok()
}

fn recovery_expired(since: Option<Instant>, now: Instant) -> bool {
    since.is_some_and(|since| now.saturating_duration_since(since) >= RTC_RECOVERY_TIMEOUT)
}

fn rtc_end_error(result: Result<(), String>, cancelled: bool) -> Option<String> {
    if cancelled {
        None
    } else {
        Some(result.err().unwrap_or_else(|| RTC_ENDED.into()))
    }
}

#[derive(Clone)]
pub struct RtcFrame {
    pub user_id: String,
    pub kind: String,
    pub image: Arc<ColorImage>,
    pub timestamp_us: i64,
}
struct CachedFrame {
    frame: RtcFrame,
    received: Instant,
    #[cfg(test)]
    source_sid: Option<livekit::id::TrackSid>,
}
#[derive(Default)]
struct LatestFrames {
    frames: HashMap<String, CachedFrame>,
}
impl LatestFrames {
    fn insert(&mut self, key: String, frame: RtcFrame) {
        let [width, height] = frame.image.size;
        let Some(bytes) = u32::try_from(width)
            .ok()
            .zip(u32::try_from(height).ok())
            .and_then(|(width, height)| rgba_bytes(width, height))
        else {
            return;
        };
        if frame.image.pixels.len().saturating_mul(4) != bytes
            || bytes > MAX_VIDEO_PIXELS as usize * 4
            || bytes > MAX_VIDEO_BYTES
        {
            return;
        }
        self.frames.remove(&key);
        let mut total: usize = self
            .frames
            .values()
            .map(|f| f.frame.image.pixels.len() * 4)
            .sum();
        while self.frames.len() >= MAX_STREAMS || total + bytes > MAX_VIDEO_BYTES {
            let Some(oldest) = self
                .frames
                .iter()
                .min_by_key(|(_, f)| f.received)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some(old) = self.frames.remove(&oldest) {
                total -= old.frame.image.pixels.len() * 4;
            }
        }
        self.frames.insert(
            key,
            CachedFrame {
                frame,
                received: Instant::now(),
                #[cfg(test)]
                source_sid: None,
            },
        );
    }
}
enum Command {
    Start {
        camera: bool,
        generation: u64,
        source: CaptureSource,
        options: CaptureOptions,
    },
    Stop(bool),
    #[cfg(test)]
    InspectRoom(tokio::sync::oneshot::Sender<Arc<Room>>),
    #[cfg(test)]
    TestRecovery {
        camera: bool,
        before_retire: Option<RecoveryGate>,
        after_publish: Option<RecoveryPause>,
        reply: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    #[cfg(all(test, target_os = "windows"))]
    TestApplicationAudio {
        receiver: Receiver<ProcessAudioFrame>,
        reply: tokio::sync::oneshot::Sender<Result<livekit::id::TrackSid, String>>,
    },
    #[cfg(test)]
    TestReplayLocalPublication {
        publication: LocalTrackPublication,
        track: LocalTrack,
        republished: bool,
        reply: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
}

/// Owns a lazy RTC runtime; construction does not enumerate or open physical devices.
pub struct RtcSession {
    stop: Arc<AtomicBool>,
    ready: Arc<AtomicBool>,
    reconnecting: Arc<AtomicBool>,
    video_visible: Arc<AtomicBool>,
    errors: Arc<Mutex<Option<String>>>,
    frames: Arc<Mutex<LatestFrames>>,
    decoded_frames: Arc<AtomicU64>,
    epochs: [Arc<AtomicU64>; 2],
    source_states: [Arc<AtomicU64>; 2],
    tracks: Arc<Mutex<HashMap<bool, LocalVideoTrack>>>,
    audio_tracks: Arc<Mutex<HashMap<bool, LocalAudioTrack>>>,
    audio_sink: Arc<Mutex<Option<ApplicationAudioSink>>>,
    application_audio_active: Arc<AtomicBool>,
    diagnostics: Arc<Mutex<HashMap<bool, CaptureDiagnostics>>>,
    commands: tokio::sync::mpsc::Sender<Command>,
    wake: Arc<tokio::sync::Notify>,
    owner: Option<JoinHandle<()>>,
}
impl RtcSession {
    pub fn connect(
        ticket: RtcTicket,
        ctx: Context,
        call_keys: Arc<CallKeys>,
    ) -> Result<Self, String> {
        crate::media::validate_call_keys(&call_keys)?;
        if !(ticket.url.starts_with("ws://") || ticket.url.starts_with("wss://"))
            || ticket.token.is_empty()
            || ticket.room.is_empty()
        {
            return Err("Server returned an invalid RTC ticket".into());
        }
        let stop = Arc::new(AtomicBool::new(false));
        let ready = Arc::new(AtomicBool::new(false));
        let reconnecting = Arc::new(AtomicBool::new(false));
        let video_visible = Arc::new(AtomicBool::new(true));
        let errors = Arc::new(Mutex::new(None));
        let frames = Arc::new(Mutex::new(LatestFrames::default()));
        let decoded_frames = Arc::new(AtomicU64::new(0));
        let epochs = [Arc::new(AtomicU64::new(1)), Arc::new(AtomicU64::new(1))];
        let source_states = [Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0))];
        let tracks = Arc::new(Mutex::new(HashMap::new()));
        let audio_tracks = Arc::new(Mutex::new(HashMap::new()));
        let audio_sink = Arc::new(Mutex::new(None));
        let application_audio_active = Arc::new(AtomicBool::new(false));
        let diagnostics = Arc::new(Mutex::new(HashMap::new()));
        let wake = Arc::new(tokio::sync::Notify::new());
        let (commands, receiver) = tokio::sync::mpsc::channel(8);
        let state = WorkerState {
            stop: stop.clone(),
            ready: ready.clone(),
            reconnecting: reconnecting.clone(),
            video_visible: video_visible.clone(),
            errors: errors.clone(),
            frames: frames.clone(),
            video_conversion: Arc::new(Mutex::new(())),
            decoded_frames: decoded_frames.clone(),
            epochs: epochs.clone(),
            source_states: source_states.clone(),
            tracks: tracks.clone(),
            audio_tracks: audio_tracks.clone(),
            audio_sink: audio_sink.clone(),
            application_audio_active: application_audio_active.clone(),
            diagnostics: diagnostics.clone(),
            wake: wake.clone(),
            ctx,
            connection_id: NEXT_CONNECTION.fetch_add(1, Ordering::Relaxed),
            call_keys,
        };
        let owner = thread::Builder::new()
            .name("relay-native-rtc".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build();
                let result = match runtime {
                    Ok(runtime) => runtime.block_on(worker(ticket, receiver, &state)),
                    Err(e) => Err(format!("Create RTC runtime: {e}")),
                };
                state.ready.store(false, Ordering::Release);
                state.reconnecting.store(false, Ordering::Release);
                for source in &state.source_states {
                    source.store(0, Ordering::Release);
                }
                if let Some(error) = rtc_end_error(result, state.stop.load(Ordering::Acquire)) {
                    if let Ok(mut slot) = state.errors.lock() {
                        *slot = Some(error);
                    }
                }
                if let Ok(mut frames) = state.frames.lock() {
                    frames.frames.clear();
                }
                state.ctx.request_repaint();
            })
            .map_err(|e| format!("Start RTC worker: {e}"))?;
        Ok(Self {
            stop,
            ready,
            reconnecting,
            video_visible,
            errors,
            frames,
            decoded_frames,
            epochs,
            source_states,
            tracks,
            audio_tracks,
            audio_sink,
            application_audio_active,
            diagnostics,
            commands,
            wake,
            owner: Some(owner),
        })
    }
    pub fn is_starting(&self) -> bool {
        !self.stop.load(Ordering::Acquire)
            && !self.ready.load(Ordering::Acquire)
            && self.owner.as_ref().is_some_and(|h| !h.is_finished())
    }
    pub fn is_active(&self) -> bool {
        !self.stop.load(Ordering::Acquire) && self.ready.load(Ordering::Acquire)
    }
    pub fn is_reconnecting(&self) -> bool {
        !self.stop.load(Ordering::Acquire) && self.reconnecting.load(Ordering::Acquire)
    }
    /// Release terminal sessions so an explicit Connect Video can obtain a new
    /// ticket. Neither a finished worker nor a stopped protected session can
    /// restart its previous sources. In-flight/active sessions are kept.
    pub fn discard_ended(slot: &mut Option<Self>) -> bool {
        if slot.as_ref().is_some_and(|session| {
            session.stop.load(Ordering::Acquire)
                || session
                    .owner
                    .as_ref()
                    .is_none_or(|owner| owner.is_finished())
        }) {
            if let Some(session) = slot.take() {
                session.cancel();
            }
            true
        } else {
            false
        }
    }
    pub fn take_error(&self) -> Option<String> {
        self.errors.lock().ok()?.take()
    }
    pub fn frames(&self) -> Vec<RtcFrame> {
        if !self.is_active() || !self.video_visible.load(Ordering::Acquire) {
            return Vec::new();
        }
        self.frames
            .lock()
            .map(|mut f| {
                f.frames
                    .retain(|_, v| v.received.elapsed() < Duration::from_secs(3));
                f.frames.values().map(|v| v.frame.clone()).collect()
            })
            .unwrap_or_default()
    }
    /// Audio and explicitly shared local sources continue while remote video is hidden.
    pub fn set_video_visible(&self, visible: bool) {
        if self.video_visible.swap(visible, Ordering::AcqRel) != visible {
            if !visible {
                if let Ok(mut frames) = self.frames.lock() {
                    frames.frames.clear();
                }
            }
            self.wake.notify_one();
        }
    }
    pub fn decoded_frames(&self) -> u64 {
        self.decoded_frames.load(Ordering::Relaxed)
    }
    pub fn is_sharing(&self, camera: bool) -> bool {
        (self.is_active() || self.is_reconnecting())
            && source_phase(
                &self.source_states[usize::from(camera)],
                self.epochs[usize::from(camera)].load(Ordering::Acquire),
            ) == 3
    }
    pub fn is_source_starting(&self, camera: bool) -> bool {
        (self.is_active() || self.is_reconnecting())
            && matches!(
                source_phase(
                    &self.source_states[usize::from(camera)],
                    self.epochs[usize::from(camera)].load(Ordering::Acquire)
                ),
                1 | 2
            )
    }
    pub fn source_diagnostics(&self, camera: bool) -> Option<CaptureDiagnostics> {
        self.diagnostics.lock().ok()?.get(&camera).cloned()
    }
    pub fn set_application_audio_sink(&self, sink: ApplicationAudioSink) {
        if let Ok(mut slot) = self.audio_sink.lock() {
            *slot = Some(sink);
        }
    }
    pub fn is_application_audio_active(&self) -> bool {
        self.is_active() && self.application_audio_active.load(Ordering::Acquire)
    }
    pub fn start_screen(
        &self,
        source: CaptureSource,
        options: CaptureOptions,
    ) -> Result<(), String> {
        self.start(false, source, options)
    }
    pub fn start_camera(
        &self,
        source: CaptureSource,
        options: CaptureOptions,
    ) -> Result<(), String> {
        self.start(true, source, options)
    }
    fn start(
        &self,
        camera: bool,
        source: CaptureSource,
        options: CaptureOptions,
    ) -> Result<(), String> {
        if !self.is_active() {
            return Err("RTC connection is not ready".into());
        }
        let generation = self.epochs[usize::from(camera)]
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |old| {
                (old < MAX_SOURCE_GENERATION).then_some(old + 1)
            })
            .map_err(|_| "Video source limit reached; reconnect video".to_string())?
            + 1;
        self.source_states[usize::from(camera)].store((generation << 2) | 1, Ordering::Release);
        if self
            .commands
            .try_send(Command::Start {
                camera,
                generation,
                source,
                options: options.normalized(),
            })
            .is_err()
        {
            set_source_phase(&self.source_states[usize::from(camera)], generation, 0);
            return Err("RTC command queue is busy".into());
        }
        Ok(())
    }
    pub fn stop_screen(&self) {
        self.stop_source(false);
    }
    pub fn stop_camera(&self) {
        self.stop_source(true);
    }
    fn stop_source(&self, camera: bool) {
        self.epochs[usize::from(camera)].fetch_add(1, Ordering::AcqRel);
        self.source_states[usize::from(camera)].store(0, Ordering::Release);
        if let Ok(mut diagnostics) = self.diagnostics.lock() {
            diagnostics.remove(&camera);
        }
        if let Ok(tracks) = self.tracks.lock() {
            if let Some(track) = tracks.get(&camera) {
                track.rtc_track().set_enabled(false);
            }
        }
        if let Ok(tracks) = self.audio_tracks.lock() {
            if let Some(track) = tracks.get(&camera) {
                track.rtc_track().set_enabled(false);
            }
        }
        if !camera {
            self.application_audio_active
                .store(false, Ordering::Release);
        }
        let _ = self.commands.try_send(Command::Stop(camera));
        self.wake.notify_one();
    }
    pub fn cancel(&self) {
        self.stop.store(true, Ordering::Release);
        self.ready.store(false, Ordering::Release);
        self.reconnecting.store(false, Ordering::Release);
        self.stop_screen();
        self.stop_camera();
        if let Ok(mut frames) = self.frames.lock() {
            frames.frames.clear();
        }
        self.wake.notify_one();
    }
}
impl Drop for RtcSession {
    fn drop(&mut self) {
        self.cancel();
        self.owner.take();
    }
}
#[derive(Clone)]
struct WorkerState {
    stop: Arc<AtomicBool>,
    ready: Arc<AtomicBool>,
    reconnecting: Arc<AtomicBool>,
    video_visible: Arc<AtomicBool>,
    errors: Arc<Mutex<Option<String>>>,
    frames: Arc<Mutex<LatestFrames>>,
    // Drop busy frames rather than queueing simultaneous full-size RGBA and
    // ColorImage conversions for every subscribed peer.
    video_conversion: Arc<Mutex<()>>,
    decoded_frames: Arc<AtomicU64>,
    epochs: [Arc<AtomicU64>; 2],
    source_states: [Arc<AtomicU64>; 2],
    tracks: Arc<Mutex<HashMap<bool, LocalVideoTrack>>>,
    audio_tracks: Arc<Mutex<HashMap<bool, LocalAudioTrack>>>,
    audio_sink: Arc<Mutex<Option<ApplicationAudioSink>>>,
    application_audio_active: Arc<AtomicBool>,
    diagnostics: Arc<Mutex<HashMap<bool, CaptureDiagnostics>>>,
    wake: Arc<tokio::sync::Notify>,
    ctx: Context,
    connection_id: u64,
    call_keys: Arc<CallKeys>,
}
struct Publication {
    capture: CaptureSession,
    receiver: Receiver<RawCaptureFrame>,
    source: NativeVideoSource,
    sid: livekit::id::TrackSid,
    native_id: String,
    generation: u64,
    options: CaptureOptions,
    pipeline: VideoPipeline,
    adaptation: AdaptiveVideo,
    stats_task: Option<tokio::task::JoinHandle<Option<VideoSample>>>,
    next_stats: Instant,
    previous_sample: Option<VideoSample>,
    next_frame: Option<Instant>,
    rate_bitrate: Option<u64>,
    adaptive_samples: u64,
    #[cfg(test)]
    recovery_pause: Option<RecoveryPause>,
    #[cfg(target_os = "windows")]
    audio: Option<ApplicationPublication>,
}
#[cfg(test)]
struct RecoveryPause {
    reached: tokio::sync::oneshot::Sender<livekit::id::TrackSid>,
    resume: tokio::sync::oneshot::Receiver<()>,
}
#[cfg(test)]
struct RecoveryGate {
    reached: tokio::sync::oneshot::Sender<()>,
    resume: tokio::sync::oneshot::Receiver<()>,
}
#[cfg(target_os = "windows")]
struct ApplicationPublication {
    capture: ProcessAudioSession,
    receiver: Receiver<ProcessAudioFrame>,
    source: NativeAudioSource,
    sid: livekit::id::TrackSid,
    native_id: String,
}
enum VideoPipeline {
    Software,
    #[cfg(target_os = "windows")]
    Hardware(HardwareEncoder),
}

#[derive(Debug, PartialEq, Eq)]
enum VideoPumpError {
    Fatal(String),
    SoftwareFallback {
        error: String,
        dimensions: (u32, u32),
    },
}
impl From<String> for VideoPumpError {
    fn from(error: String) -> Self {
        Self::Fatal(error)
    }
}
impl From<&str> for VideoPumpError {
    fn from(error: &str) -> Self {
        Self::Fatal(error.into())
    }
}
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn hardware_reconfiguration<T>(
    preference: VideoCodecPreference,
    result: Result<T, String>,
    dimensions: (u32, u32),
) -> Result<T, VideoPumpError> {
    result.map_err(|error| match preference {
        VideoCodecPreference::Auto => VideoPumpError::SoftwareFallback { error, dimensions },
        VideoCodecPreference::Software | VideoCodecPreference::Hardware => {
            VideoPumpError::Fatal(error)
        }
    })
}

struct RemoteReceiver {
    sid: livekit::id::TrackSid,
    task: tokio::task::JoinHandle<()>,
}
async fn remove_receiver(
    receivers: &mut HashMap<String, RemoteReceiver>,
    frames: &Arc<Mutex<LatestFrames>>,
    key: &str,
    sid: &livekit::id::TrackSid,
) -> bool {
    if receivers
        .get(key)
        .is_none_or(|receiver| &receiver.sid != sid)
    {
        return false;
    }
    let receiver = receivers.remove(key).unwrap();
    receiver.task.abort();
    let _ = receiver.task.await;
    if let Ok(mut frames) = frames.lock() {
        frames.frames.remove(key);
    }
    true
}
impl VideoPipeline {
    fn hardware(&self) -> bool {
        match self {
            Self::Software => false,
            #[cfg(target_os = "windows")]
            Self::Hardware(_) => true,
        }
    }
    fn name(&self) -> String {
        match self {
            Self::Software => "Software VP8".into(),
            #[cfg(target_os = "windows")]
            Self::Hardware(encoder) => format!("Hardware H.264 · {} (CPU staging)", encoder.name),
        }
    }
}

fn protected_room_options(keys: &CallKeys) -> Result<RoomOptions, String> {
    crate::media::validate_call_keys(keys)?;
    // Fresh MLS-derived epoch keys are installed explicitly. Do not silently
    // ratchet a removed member's old key into a future call or accept plaintext.
    let provider = KeyProvider::new(KeyProviderOptions {
        ratchet_window_size: 0,
        // Native frame crypto treats negative tolerance as unlimited failures.
        // Report the first bad frame so the protected session fails closed.
        failure_tolerance: 0,
        key_ring_size: 1,
        ..Default::default()
    });
    for (user, sender) in &keys.sender_keys {
        if !provider.set_key(&user.clone().into(), 0, sender.rtc.to_vec()) {
            return Err("Install protected RTC participant key failed".into());
        }
    }
    let mut options = RoomOptions::default();
    options.auto_subscribe = false;
    options.encryption = Some(E2eeOptions {
        encryption_type: EncryptionType::Gcm,
        key_provider: provider,
    });
    Ok(options)
}
fn verified_publication(keys: &CallKeys, user: &str, publication: &RemoteTrackPublication) -> bool {
    publication.encryption_type() == EncryptionType::Gcm
        && user != keys.local_user_id
        && keys.sender_keys.contains_key(user)
        && matches!(
            (publication.kind(), publication.source()),
            (
                TrackKind::Video,
                TrackSource::Camera | TrackSource::Screenshare
            ) | (TrackKind::Audio, TrackSource::ScreenshareAudio)
        )
}
fn controller_pump_interval(period: Duration) -> tokio::time::Interval {
    let mut tick = tokio::time::interval(period);
    // Returning from an idle room must not replay accumulated frame-pump ticks.
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    tick
}

async fn wait_for_controller_maintenance(
    tick: &mut tokio::time::Interval,
    local_work: bool,
    recovery_since: Option<Instant>,
) {
    if local_work {
        tick.tick().await;
    } else if let Some(since) = recovery_since {
        // This is the original reconnect deadline, not a new periodic poll or
        // a deadline restarted by each command, retry or visibility change.
        tokio::time::sleep_until(tokio::time::Instant::from_std(since + RTC_RECOVERY_TIMEOUT))
            .await;
    } else {
        std::future::pending::<()>().await;
    }
}

async fn refresh_remote_video_visibility(
    state: &WorkerState,
    last_visible: &mut bool,
    remote_video: &HashMap<String, RemoteTrackPublication>,
    receivers: &mut HashMap<String, RemoteReceiver>,
) {
    let visible = state.video_visible.load(Ordering::Acquire);
    if visible == *last_visible {
        return;
    }
    for publication in remote_video.values() {
        publication.set_subscribed(visible);
    }
    if !visible {
        for (key, publication) in remote_video {
            remove_receiver(receivers, &state.frames, key, &publication.sid()).await;
        }
        if let Ok(mut frames) = state.frames.lock() {
            frames.frames.clear();
        }
    }
    *last_visible = visible;
}

async fn worker(
    ticket: RtcTicket,
    mut commands: tokio::sync::mpsc::Receiver<Command>,
    state: &WorkerState,
) -> Result<(), String> {
    if state.stop.load(Ordering::Acquire) {
        return Ok(());
    }
    crate::tls_crypto::initialize();
    let mut options = protected_room_options(&state.call_keys)?;
    let private_route = crate::embedded_network::routed(&ticket.url);
    let https_media = if let Some(bridge) = ticket.media_bridge.clone() {
        let signaling = ticket.url.clone();
        Some(
            tokio::task::spawn_blocking(move || {
                crate::embedded_network::HttpsMediaLease::open(&bridge, &signaling)
            })
            .await
            .map_err(|_| "Secure media setup could not finish")??,
        )
    } else {
        None
    };
    if state.stop.load(Ordering::Acquire) {
        return Ok(());
    }
    let turn = https_media
        .as_ref()
        .map(|m| m.turn.clone())
        .or_else(|| crate::embedded_network::rtc_turn(&ticket.url));
    if let Some(turn) = turn {
        use livekit::webrtc::peer_connection_factory::{IceServer, IceTransportsType};
        options.rtc_config.ice_servers = vec![IceServer {
            urls: vec![turn.url],
            username: turn.username,
            password: turn.password,
        }];
        options.rtc_config.ice_transport_type = IceTransportsType::Relay;
        options.rtc_config.enable_loopback_turn = true;
    } else if crate::embedded_network::rtc_requires_turn(&ticket.url) {
        return Err(
            "The private video connection stopped. Rejoin the room to reconnect it.".into(),
        );
    }
    let connection_url = crate::embedded_network::endpoint(&ticket.url);
    let connect = tokio::time::timeout(
        Duration::from_secs(15),
        Room::connect(&connection_url, &ticket.token, options),
    );
    tokio::pin!(connect);
    let (room, mut events) = loop {
        tokio::select! {
            connected=&mut connect=>break connected.map_err(|_|"RTC connection timed out".to_string())?.map_err(|e|format!("Connect native RTC: {e}"))?,
            _=state.wake.notified()=>{if state.stop.load(Ordering::Acquire){return Ok(());}}
        }
    };
    if state.stop.load(Ordering::Acquire) {
        let _ = room.close().await;
        return Ok(());
    }
    if room.local_participant().identity().to_string() != state.call_keys.local_user_id {
        let _ = tokio::time::timeout(Duration::from_secs(3), room.close()).await;
        return Err("RTC ticket identity does not match the verified device".into());
    }
    let room = Arc::new(room);
    state.ready.store(true, Ordering::Release);
    state.ctx.request_repaint();
    let mut publications: HashMap<bool, Publication> = HashMap::new();
    let mut jobs = SourceJobs::default();
    let mut receivers: HashMap<String, RemoteReceiver> = HashMap::new();
    let mut remote_video: HashMap<String, RemoteTrackPublication> = HashMap::new();
    let mut remote_audio: HashMap<String, RemoteTrackPublication> = HashMap::new();
    // Use one initial visibility snapshot; a concurrent change retains its Notify
    // permit and is reconciled by the event-driven owner below.
    let mut last_video_visible = state.video_visible.load(Ordering::Acquire);
    for participant in room.remote_participants().values() {
        for publication in participant.track_publications().values() {
            let allowed = verified_publication(
                &state.call_keys,
                &participant.identity().to_string(),
                publication,
            );
            publication.set_subscribed(
                allowed && (publication.kind() != TrackKind::Video || last_video_visible),
            );
            if allowed && publication.kind() == TrackKind::Video {
                let kind = if publication.source() == TrackSource::Camera {
                    "camera"
                } else {
                    "screen"
                };
                remote_video.insert(
                    format!("{}:{kind}", participant.identity()),
                    publication.clone(),
                );
            } else if allowed && publication.kind() == TrackKind::Audio {
                remote_audio.insert(
                    format!("{}:application-audio", participant.identity()),
                    publication.clone(),
                );
            }
        }
    }
    let mut recovery_since = None;
    let mut interrupted_video = None;
    let mut terminal_error = None;
    let mut tick = controller_pump_interval(Duration::from_millis(5));
    let mut private_connection_tick = controller_pump_interval(Duration::from_secs(1));
    while !state.stop.load(Ordering::Acquire) {
        // Remote streams consume their own native events. Only local capture,
        // queued starts and retained source futures require the 5 ms pump.
        let local_work =
            !publications.is_empty() || !jobs.pending.is_empty() || !jobs.queued.is_empty();
        tokio::select! {
            _=async {if private_route||https_media.is_some() {private_connection_tick.tick().await;} else {std::future::pending::<()>().await;}}=>{
                if https_media.as_ref().is_some_and(|m|m.alive()==Some(false)) {
                    terminal_error=Some("The video connection stopped. Join the call again to reconnect.".into());break;
                }
                if private_route&&crate::embedded_network::endpoint(&ticket.url)!=connection_url {
                    terminal_error=Some("The private video connection stopped. Reconnect the private network and join the call again.".into());
                    break;
                }
            },
            _=state.wake.notified()=>{
                if !state.stop.load(Ordering::Acquire) {
                    refresh_remote_video_visibility(state,&mut last_video_visible,&remote_video,&mut receivers).await;
                }
            },
            command=commands.recv()=>match command {
                Some(Command::Stop(camera))=>stop_source_job(&room,&mut publications,&mut jobs,state,camera,true),
                #[cfg(test)]
                Some(Command::InspectRoom(reply))=>{let _=reply.send(room.clone());},
                #[cfg(test)]
                Some(Command::TestRecovery{camera,before_retire,after_publish,reply})=>{
                    let result=if let Some(current)=publications.get_mut(&camera){
                        let (width,height)=current.adaptation.profile().quality.dimensions();
                        begin_video_recovery(&room,current,&mut jobs,state,camera,(width&!1,height&!1),(before_retire,after_publish))
                    }else{Err("Owned source is not attached".into())};
                    let _=reply.send(result);
                },
                #[cfg(all(test,target_os="windows"))]
                Some(Command::TestApplicationAudio{receiver,reply})=>{
                    let result=if let Some(current)=publications.get_mut(&false){
                        attach_synthetic_application_audio(&room,current,state,receiver).await
                    }else{Err("Owned screen source is not attached".into())};
                    let _=reply.send(result);
                },
                #[cfg(test)]
                Some(Command::TestReplayLocalPublication{publication,track,republished,reply})=>{
                    let previous=republished.then(||publication.sid());
                    let result=handle_local_publication(&room,&mut publications,&mut jobs,state,&publication,&track,previous.as_ref());
                    if let Err(error)=&result{stop_protected_media(state,error);}
                    let _=reply.send(result);
                },
                Some(Command::Start{camera,generation,source,options})=>{
                    if state.stop.load(Ordering::Acquire)||state.epochs[usize::from(camera)].load(Ordering::Acquire)!=generation{continue;}
                    queue_source_start(&room,&mut publications,&mut jobs,state,camera,SourceSelection{generation,source,options});
                },
                None=>break,
            },
            event=events.recv()=>match event{
                Some(RoomEvent::Reconnecting)=>{
                    if private_route&&crate::embedded_network::endpoint(&ticket.url)!=connection_url {
                        terminal_error=Some("The private video connection stopped. Reconnect the private network and join the call again.".into());
                        break;
                    }
                    if recovery_since.is_none(){
                        interrupted_video=Some(publications.iter().map(|(camera,current)|(*camera,
                            InterruptedVideo{generation:current.generation,sid:current.sid.clone(),native_id:current.native_id.clone()})).collect::<HashMap<_,_>>());
                    }
                    recovery_since.get_or_insert_with(Instant::now);
                    state.reconnecting.store(true,Ordering::Release);
                    state.ready.store(false,Ordering::Release);
                    state.ctx.request_repaint();
                },
                Some(RoomEvent::Reconnected)=>{
                    if room.local_participant().identity().to_string()!=state.call_keys.local_user_id {
                        terminal_error=Some("Reconnected video belongs to another account. Disconnect and sign in again.".into());
                        break;
                    }
                    if let Some(interrupted)=interrupted_video.take(){
                        if let Err(error)=rebind_resumed_video(&room,&mut publications,&mut jobs,state,interrupted){
                            stop_protected_media(state,&error);terminal_error=Some(error);break;
                        }
                    }
                    recovery_since=None;
                    if !state.stop.load(Ordering::Acquire){state.reconnecting.store(false,Ordering::Release);state.ready.store(true,Ordering::Release);}
                    state.ctx.request_repaint();
                },
                Some(RoomEvent::LocalTrackPublished{publication,track,participant})=>{
                    if participant.identity().to_string()!=state.call_keys.local_user_id || publication.encryption_type()!=EncryptionType::Gcm {
                        track.rtc_track().set_enabled(false);
                        terminal_error=Some("Video publication could not confirm protected account ownership. Disconnect and sign in again.".into());
                        break;
                    }
                    if let Err(error)=handle_local_publication(&room,&mut publications,&mut jobs,state,&publication,&track,None) {
                        stop_protected_media(state,&error);terminal_error=Some(error);break;
                    }
                },
                Some(RoomEvent::LocalTrackRepublished{previous_sid,publication,track,participant})=>{
                    if participant.identity().to_string()!=state.call_keys.local_user_id || publication.encryption_type()!=EncryptionType::Gcm {
                        track.rtc_track().set_enabled(false);
                        terminal_error=Some("Reconnected video could not confirm protected account ownership. Disconnect and sign in again.".into());
                        break;
                    }
                    if let Err(error)=handle_local_publication(&room,&mut publications,&mut jobs,state,&publication,&track,Some(&previous_sid)) {
                        stop_protected_media(state,&error);terminal_error=Some(error);break;
                    }
                },
                Some(RoomEvent::TrackSubscribed{track:RemoteTrack::Video(track),publication,participant})=>{
                    if !participant.track_publications().contains_key(&publication.sid()) || !verified_publication(&state.call_keys,&participant.identity().to_string(),&publication){publication.set_subscribed(false);continue;}
                    let kind=if publication.source()==TrackSource::Camera{"camera"}else{"screen"};
                    let user_id=participant.identity().to_string();let key=format!("{user_id}:{kind}");
                    if remote_video.get(&key).is_some_and(|current|current.sid()!=publication.sid()){publication.set_subscribed(false);continue;}
                    remote_video.insert(key.clone(),publication.clone());
                    if !state.video_visible.load(Ordering::Acquire){publication.set_subscribed(false);continue;}
                    if let Some(old)=receivers.remove(&key){old.task.abort();let _=old.task.await;}
                    if receivers.len()>=MAX_STREAMS{publication.set_subscribed(false);continue;}
                    let mut stream=NativeVideoStream::new(track.rtc_track());
                    let frames=state.frames.clone();let conversion=state.video_conversion.clone();let decoded_frames=state.decoded_frames.clone();let visible=state.video_visible.clone();let stop=state.stop.clone();let ctx=state.ctx.clone();let task_key=key.clone();let kind=kind.to_string();
                    #[cfg(test)] let source_sid=publication.sid();
                    receivers.insert(key,RemoteReceiver{sid:publication.sid(),task:tokio::spawn(async move{
                        while let Some(frame)=stream.next().await{
                            if stop.load(Ordering::Acquire){break;}
                            if !visible.load(Ordering::Acquire){continue;}
                            decoded_frames.fetch_add(1,Ordering::Relaxed);
                            let width=frame.buffer.width();let height=frame.buffer.height();
                            let Some(bytes)=rgba_bytes(width,height) else{continue;};
                            if frame.timestamp_us<0{continue;}
                            let Ok(_conversion)=conversion.try_lock() else{continue;};
                            let mut rgba=vec![0; bytes];
                            frame.buffer.to_argb(VideoFormatType::ABGR,&mut rgba,width*4,width as i32,height as i32);
                            let image=Arc::new(ColorImage::from_rgba_unmultiplied([width as usize,height as usize],&rgba));
                            if stop.load(Ordering::Acquire){break;}
                            if let Ok(mut cache)=frames.lock(){if visible.load(Ordering::Acquire)&&!stop.load(Ordering::Acquire){
                                #[cfg(test)] let accepted_image=image.clone();
                                cache.insert(task_key.clone(),RtcFrame{user_id:user_id.clone(),kind:kind.clone(),image,timestamp_us:frame.timestamp_us});
                                // Provenance is test-only and stamps only this exact
                                // accepted NativeVideoStream image, never a rejected
                                // insert that left an older cached image in place.
                                #[cfg(test)] if let Some(cached)=cache.frames.get_mut(&task_key){if Arc::ptr_eq(&cached.frame.image,&accepted_image){cached.source_sid=Some(source_sid.clone());}}
                            }}
                            ctx.request_repaint();
                        }
                    })});
                },
                Some(RoomEvent::TrackPublished{publication,participant})=>{
                    let allowed=participant.track_publications().contains_key(&publication.sid())&&verified_publication(&state.call_keys,&participant.identity().to_string(),&publication);
                    publication.set_subscribed(allowed&&(publication.kind()!=TrackKind::Video||state.video_visible.load(Ordering::Acquire)));
                    if allowed&&publication.kind()==TrackKind::Video {let kind=if publication.source()==TrackSource::Camera{"camera"}else{"screen"};let key=format!("{}:{kind}",participant.identity());remote_video.insert(key,publication.clone());}
                    if allowed&&publication.kind()==TrackKind::Audio {remote_audio.insert(format!("{}:application-audio",participant.identity()),publication.clone());}
                },
                Some(RoomEvent::TrackUnpublished{publication,participant})=>{
                    let kind=if publication.source()==TrackSource::Camera{"camera"}else if publication.source()==TrackSource::ScreenshareAudio{"application-audio"}else{"screen"};let key=format!("{}:{kind}",participant.identity());
                    let current=remote_video.get(&key).is_some_and(|current|current.sid()==publication.sid());
                    if current{remote_video.remove(&key);}
                    if remote_audio.get(&key).is_some_and(|current|current.sid()==publication.sid()){remote_audio.remove(&key);}
                    let removed=remove_receiver(&mut receivers,&state.frames,&key,&publication.sid()).await;
                    if current&&!removed{if let Ok(mut frames)=state.frames.lock(){frames.frames.remove(&key);}}
                    state.ctx.request_repaint();
                },
                Some(RoomEvent::TrackSubscribed{track:RemoteTrack::Audio(track),publication,participant})=>{
                    if !participant.track_publications().contains_key(&publication.sid()) || !verified_publication(&state.call_keys,&participant.identity().to_string(),&publication){publication.set_subscribed(false);continue;}
                    let key=format!("{}:application-audio",participant.identity());
                    if publication.source()!=TrackSource::ScreenshareAudio || remote_audio.get(&key).is_some_and(|current|current.sid()!=publication.sid()) || (receivers.len()>=MAX_STREAMS&&!receivers.contains_key(&key)){publication.set_subscribed(false);continue;}
                    remote_audio.insert(key.clone(),publication.clone());
                    if let Some(old)=receivers.remove(&key){old.task.abort();let _=old.task.await;}
                    let mut stream=NativeAudioStream::with_options(track.rtc_track(),48_000,2,NativeAudioStreamOptions{queue_size_frames:Some(2)});
                    let sink=state.audio_sink.clone();let stop=state.stop.clone();let audio_key=format!("{}:{}",state.connection_id,key);let user_id=participant.identity().to_string();
                    receivers.insert(key,RemoteReceiver{sid:publication.sid(),task:tokio::spawn(async move{
                        let _guard=ApplicationAudioGuard{sink:sink.clone(),key:audio_key.clone()};
                        while let Some(frame)=stream.next().await{
                            if stop.load(Ordering::Acquire){break;}
                            if frame.sample_rate!=48_000||frame.num_channels!=2{continue;}
                            let sink=sink.lock().ok().and_then(|slot|slot.clone());
                            if let Some(sink)=sink{sink.push_peer_guarded(&audio_key,&user_id,&frame.data,&stop);}
                        }
                    })});
                },
                Some(RoomEvent::TrackUnsubscribed{publication,participant,..})=>{
                    let kind=if publication.source()==TrackSource::Camera{"camera"}else if publication.source()==TrackSource::ScreenshareAudio{"application-audio"}else{"screen"};let key=format!("{}:{kind}",participant.identity());
                    remove_receiver(&mut receivers,&state.frames,&key,&publication.sid()).await;state.ctx.request_repaint();
                },
                Some(RoomEvent::E2eeStateChanged{state:encryption_state,..})=>{
                    use livekit::webrtc::native::frame_cryptor::EncryptionState;
                    if matches!(encryption_state,EncryptionState::EncryptionFailed|EncryptionState::DecryptionFailed|EncryptionState::MissingKey|EncryptionState::InternalError){
                        if let Ok(mut slot)=state.errors.lock(){*slot=Some("Protected media authentication failed; reconnect with verified call keys".into());}
                        state.stop.store(true,Ordering::Release);state.ctx.request_repaint();
                    }
                },
                Some(RoomEvent::Disconnected{..})|None=>break,
                _=>{},
            },
            _=wait_for_controller_maintenance(&mut tick,local_work,recovery_since)=>{
                if recovery_expired(recovery_since,Instant::now()) {
                    terminal_error=Some("Video could not reconnect within 30 seconds. Choose Connect Video to try again, then select your camera or screen.".into());
                    break;
                }
                refresh_remote_video_visibility(state,&mut last_video_visible,&remote_video,&mut receivers).await;
                let obsolete:Vec<_>=publications.iter().filter(|(camera,p)|state.epochs[usize::from(**camera)].load(Ordering::Acquire)!=p.generation).map(|(camera,_)|*camera).collect();
                for camera in obsolete{stop_source_job(&room,&mut publications,&mut jobs,state,camera,false);}
                for (camera,job) in jobs.pending.iter_mut(){if !current_source(state,*camera,job.registry.generation){job.discard=true;job.registry.cancel();}}
                if let Err(error)=poll_source_jobs(&room,&mut publications,&mut jobs,state){stop_protected_media(state,&error);terminal_error=Some(error);break;}
                let mut failed=Vec::new();
                for (camera,publication) in publications.iter_mut(){
                    let recovering=jobs.pending.get(camera).is_some_and(|job|job.recovering);
                    if !recovering{update_video_adaptation(publication,state,*camera).await;}
                    if let Ok(mut diagnostics)=state.diagnostics.lock(){let mut current=publication.capture.diagnostics();current.video_codec=publication.pipeline.name();current.adaptive_output=publication.options.adaptive_video.then(||publication.adaptation.description());current.adaptive_samples=publication.adaptive_samples;diagnostics.insert(*camera,current);}
                    if let Some(error)=publication.capture.take_error(){if let Ok(mut slot)=state.errors.lock(){*slot=Some(error);}failed.push(*camera);state.ctx.request_repaint();continue;}
                    if publication.capture.is_active(){set_source_phase(&state.source_states[usize::from(*camera)],publication.generation,3);}
                    let mut latest=None;for _ in 0..2{match publication.receiver.try_recv(){Ok(frame)=>latest=Some(frame),Err(_)=>break}}
                    let pumped=if recovering{Ok(())}else{match pump_video(publication,latest,state,*camera,&room.local_participant().identity().to_string()) {
                        Ok(())=>Ok(()),
                        Err(VideoPumpError::Fatal(error))=>Err(error),
                        Err(VideoPumpError::SoftwareFallback{error,dimensions})=>begin_video_recovery(&room,publication,&mut jobs,state,*camera,dimensions,#[cfg(test)](None,None)).map_err(|recovery|format!("Automatic video recovery failed: {recovery} (hardware: {error})")),
                    }};
                    if let Err(error)=pumped{if let Ok(mut slot)=state.errors.lock(){*slot=Some(error);}failed.push(*camera);state.ctx.request_repaint();}
                    #[cfg(target_os="windows")]
                    if let Err(error)=pump_application_audio(publication,state,*camera).await {
                        if let Ok(mut slot)=state.errors.lock(){*slot=Some(error);}
                        if let Err(error)=stop_failed_application_audio(&room,publication,&mut jobs,state,*camera){stop_protected_media(state,&error);terminal_error=Some(error);}
                        state.ctx.request_repaint();
                    }
                }
                for camera in failed{if let Some(publication)=publications.get(&camera){set_source_phase(&state.source_states[usize::from(camera)],publication.generation,0);}stop_source_job(&room,&mut publications,&mut jobs,state,camera,true);}
            },
        }
    }
    jobs.queued.clear();
    for camera in [false, true] {
        stop_source_job(&room, &mut publications, &mut jobs, state, camera, true);
    }
    for (_, receiver) in receivers {
        receiver.task.abort();
        let _ = receiver.task.await;
    }
    // Keep every late AddTrack future owned until its exact SID is retired.
    // On a partial SDK failure, retain an owned Room close attempt before
    // dropping remaining empty sources/futures. No capture can submit media
    // after cancellation; an incomplete transport close remains unverified.
    let until = Instant::now() + Duration::from_secs(25);
    while !jobs.pending.is_empty() && terminal_error.is_none() && Instant::now() < until {
        if let Err(error) = poll_source_jobs(&room, &mut publications, &mut jobs, state) {
            terminal_error = Some(error);
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    for job in jobs.pending.values() {
        job.registry.disable();
    }
    if !jobs.pending.is_empty() && terminal_error.is_none() {
        terminal_error = Some("Protected media cleanup deadline".into());
    }
    // SDK close takes its internal handle before awaiting transport teardown.
    // A timeout must retain this same future; retrying can say AlreadyClosed
    // even when the cancelled original has not closed its transport.
    let close = room.close();
    tokio::pin!(close);
    let mut close_finished = false;
    let mut close_verified = false;
    let mut close_error = None;
    let mut close_overdue = false;
    match tokio::time::timeout(Duration::from_secs(5), close.as_mut()).await {
        Ok(Ok(())) => {
            close_finished = true;
            close_verified = true;
        }
        Ok(Err(error)) => {
            close_finished = true;
            close_error = Some(error.to_string());
        }
        Err(_) => close_overdue = true,
    }
    // Continue retaining and driving bounded source jobs during the same close
    // attempt. An unverified teardown remains an error, never a successful close.
    let drain_until = Instant::now() + Duration::from_secs(30);
    while (!jobs.pending.is_empty() || !close_finished) && Instant::now() < drain_until {
        if !close_finished {
            if let Some(result) = close.as_mut().now_or_never() {
                close_finished = true;
                match result {
                    Ok(()) => close_verified = true,
                    Err(error) => close_error = Some(error.to_string()),
                }
            }
        }
        if let Err(error) = poll_source_jobs(&room, &mut publications, &mut jobs, state) {
            terminal_error.get_or_insert(error);
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    if !close_verified {
        let error = close_error.map_or_else(
            || "Protected RTC teardown is unverified after its cleanup deadline".to_string(),
            |error| format!("Protected RTC teardown is unverified: {error}"),
        );
        terminal_error = Some(
            terminal_error.map_or_else(|| error.clone(), |previous| format!("{previous}; {error}")),
        );
    } else if close_overdue {
        let error = "Protected RTC room closed after its initial cleanup deadline";
        terminal_error = Some(terminal_error.map_or_else(
            || error.to_string(),
            |previous| format!("{previous}; {error}"),
        ));
    }
    // Capture cancellation is independent of confirmed SDK transport teardown.
    // A failed/unknown close cannot report successful overall media cleanup.
    for publication in room.local_participant().track_publications().values() {
        if let Some(track) = publication.track() {
            track.rtc_track().set_enabled(false);
        }
    }
    for (_, mut job) in jobs.pending.drain() {
        job.registry.cancel();
        if let Some(publication) = job.retiring.take() {
            publication.capture.cancel();
            jobs.failed_releases.push(publication);
        }
        // Source operations have individual deadlines. If an opaque future
        // survives this combined bound, teardown acceptance remains unverified.
        // All known tracks stay disabled and its capture has been cancelled.
        terminal_error.get_or_insert_with(|| {
            "Closed media operation did not finish within its deadline".into()
        });
    }
    for publication in jobs.failed_releases.drain(..) {
        if let Err(CaptureReleaseError { error, capture }) =
            release_cancelled_publication(publication).await
        {
            terminal_error.get_or_insert(error);
            if let Some(capture) = capture {
                jobs.failed_capture_releases.push(*capture);
            }
        }
    }
    for capture in jobs.failed_capture_releases.drain(..) {
        if let Err(CaptureReleaseError { error, capture }) =
            release_cancelled_capture(Box::new(capture)).await
        {
            terminal_error.get_or_insert(error);
            // If the OS refuses both release threads, the final cancelled
            // payload is dropped after the retained teardown attempt. Media
            // stays disabled; an unverified close already makes this an error.
            drop(capture);
        }
    }
    drop(jobs);
    terminal_error.map_or(Ok(()), Err)
}

fn stop_protected_media(state: &WorkerState, error: &str) {
    state.stop.store(true, Ordering::Release);
    state.ready.store(false, Ordering::Release);
    state.reconnecting.store(false, Ordering::Release);
    state
        .application_audio_active
        .store(false, Ordering::Release);
    for source in &state.source_states {
        source.store(0, Ordering::Release);
    }
    if let Ok(mut slot) = state.errors.lock() {
        *slot = Some(error.into());
    }
    if let Ok(tracks) = state.tracks.lock() {
        for track in tracks.values() {
            track.rtc_track().set_enabled(false);
        }
    }
    if let Ok(tracks) = state.audio_tracks.lock() {
        for track in tracks.values() {
            track.rtc_track().set_enabled(false);
        }
    }
    state.wake.notify_one();
    state.ctx.request_repaint();
}

// Each source has one owned network operation and one newest selection. The
// operation owns no raw frames; capture queues remain bounded and are drained
// by the normal worker even while retirement or AddTrack is waiting.
#[derive(Clone)]
struct PendingLocalTracks {
    camera: bool,
    generation: u64,
    connection_id: u64,
    call_keys: Arc<CallKeys>,
    cancelled: Arc<AtomicBool>,
    tracks: Arc<Mutex<Vec<(LocalTrack, TrackSource)>>>,
}
impl PendingLocalTracks {
    fn new(state: &WorkerState, camera: bool, generation: u64) -> Self {
        Self {
            camera,
            generation,
            connection_id: state.connection_id,
            call_keys: state.call_keys.clone(),
            cancelled: Arc::new(AtomicBool::new(false)),
            tracks: Arc::new(Mutex::new(Vec::with_capacity(2))),
        }
    }
    fn same_context(&self, state: &WorkerState) -> bool {
        self.connection_id == state.connection_id
            && Arc::ptr_eq(&self.call_keys, &state.call_keys)
            && crate::media::validate_call_keys(&self.call_keys).is_ok()
    }
    fn current(&self, state: &WorkerState, room: &Room) -> Result<bool, String> {
        if !self.same_context(state) {
            return Err("Pending media belongs to a different verified call".into());
        }
        if self.cancelled.load(Ordering::Acquire) {
            return Ok(false);
        }
        recovery_context_current(
            state,
            self.camera,
            self.generation,
            &room.local_participant().identity().to_string(),
        )
    }
    fn register(&self, track: LocalTrack, source: TrackSource) -> Result<(), String> {
        let mut tracks = self
            .tracks
            .lock()
            .map_err(|_| "Pending track ownership unavailable")?;
        if tracks
            .iter()
            .any(|(owned, _)| owned.rtc_track().id() == track.rtc_track().id())
        {
            return Ok(());
        }
        if tracks.len() >= 2 {
            return Err("Pending media track ownership exceeded its bound".into());
        }
        tracks.push((track, source));
        Ok(())
    }
    fn owns(
        &self,
        state: &WorkerState,
        publication: &LocalTrackPublication,
        track: &LocalTrack,
    ) -> bool {
        self.same_context(state)
            && publication.encryption_type() == EncryptionType::Gcm
            && publication.kind() == track.kind()
            && self.tracks.lock().is_ok_and(|tracks| {
                tracks.iter().any(|(owned, source)| {
                    owned.rtc_track().id() == track.rtc_track().id()
                        && owned.kind() == publication.kind()
                        && *source == publication.source()
                })
            })
    }
    fn disable(&self) {
        if let Ok(tracks) = self.tracks.lock() {
            for (track, _) in tracks.iter() {
                track.rtc_track().set_enabled(false);
            }
        }
    }
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.disable();
    }
}
struct SourceSelection {
    generation: u64,
    source: CaptureSource,
    options: CaptureOptions,
}
struct RecoveryAttachment {
    previous_sid: livekit::id::TrackSid,
    previous_native_id: String,
    sid: livekit::id::TrackSid,
    track: LocalVideoTrack,
    source: NativeVideoSource,
}
enum SourceJobOutput {
    Started(Option<Box<Publication>>),
    Recovered(Option<RecoveryAttachment>),
    Retired,
    RetirementFailed {
        error: String,
        captures: Vec<Publication>,
    },
    CaptureReleaseFailed {
        error: String,
        capture: Box<CancelledCapture>,
        captures: Vec<Publication>,
    },
}
struct CancelledCapture {
    capture: CaptureSession,
    receiver: Receiver<RawCaptureFrame>,
    #[cfg(target_os = "windows")]
    audio: Option<(ProcessAudioSession, Receiver<ProcessAudioFrame>)>,
}
struct CaptureReleaseError {
    error: String,
    capture: Option<Box<CancelledCapture>>,
}
struct RetiringTrack {
    sid: livekit::id::TrackSid,
    native_id: String,
}
type SourceJobFuture = Pin<Box<dyn Future<Output = Result<SourceJobOutput, String>>>>;
struct SourceJob {
    registry: PendingLocalTracks,
    future: SourceJobFuture,
    recovering: bool,
    discard: bool,
    retiring: Option<Publication>,
    orphans: Vec<RetiringTrack>,
}
#[derive(Default)]
struct SourceJobs {
    pending: HashMap<bool, SourceJob>,
    queued: HashMap<bool, SourceSelection>,
    failed_releases: Vec<Publication>,
    failed_capture_releases: Vec<CancelledCapture>,
}

fn detach_source(
    publications: &mut HashMap<bool, Publication>,
    state: &WorkerState,
    camera: bool,
) -> Option<Publication> {
    let mut publication = publications.remove(&camera)?;
    publication.capture.cancel();
    publication.pipeline = VideoPipeline::Software;
    if let Some(task) = publication.stats_task.take() {
        task.abort();
    }
    if let Ok(mut tracks) = state.tracks.lock() {
        if let Some(track) = tracks.remove(&camera) {
            track.rtc_track().set_enabled(false);
        }
    }
    #[cfg(target_os = "windows")]
    if let Some(audio) = publication.audio.as_ref() {
        audio.capture.cancel();
        audio.source.clear_buffer();
        if let Ok(mut tracks) = state.audio_tracks.lock() {
            if let Some(track) = tracks.remove(&camera) {
                track.rtc_track().set_enabled(false);
            }
        }
        state
            .application_audio_active
            .store(false, Ordering::Release);
    }
    if let Ok(mut diagnostics) = state.diagnostics.lock() {
        diagnostics.remove(&camera);
    }
    if let Ok(mut frames) = state.frames.lock() {
        let kind = if camera { "camera" } else { "screen" };
        frames
            .frames
            .remove(&format!("{}:{kind}", state.call_keys.local_user_id));
    }
    Some(publication)
}
async fn retire_detached_publication(
    room: &Room,
    mut publication: Publication,
) -> Result<SourceJobOutput, String> {
    publication.capture.cancel();
    publication.pipeline = VideoPipeline::Software;
    let retirement = async {
        #[cfg(target_os = "windows")]
        if let Some(audio) = publication.audio.as_ref() {
            audio.capture.cancel();
            audio.source.clear_buffer();
            retire_exact_track(
                room,
                RetiringTrack {
                    sid: audio.sid.clone(),
                    native_id: audio.native_id.clone(),
                },
            )
            .await?;
        }
        retire_exact_track(
            room,
            RetiringTrack {
                sid: publication.sid.clone(),
                native_id: publication.native_id.clone(),
            },
        )
        .await
    }
    .await;
    if let Err(error) = retirement {
        // Keep the cancelled capture owned until the worker closes the room.
        // Returning this payload does not run CaptureSession::Drop on the pump.
        return Ok(SourceJobOutput::RetirementFailed {
            error,
            captures: vec![publication],
        });
    }
    match release_cancelled_publication(publication).await {
        Ok(()) => Ok(SourceJobOutput::Retired),
        Err(CaptureReleaseError {
            error,
            capture: Some(capture),
        }) => Ok(SourceJobOutput::CaptureReleaseFailed {
            error,
            capture,
            captures: Vec::new(),
        }),
        Err(CaptureReleaseError {
            error,
            capture: None,
        }) => Err(error),
    }
}
async fn release_cancelled_publication(
    mut publication: Publication,
) -> Result<(), CaptureReleaseError> {
    publication.capture.cancel();
    // HardwareEncoder owns a COM/MF apartment on this RTC owner. Dispose it
    // here before moving only capture-session ownership to its release thread.
    publication.pipeline = VideoPipeline::Software;
    #[cfg(target_os = "windows")]
    let audio = publication.audio.take().map(|audio| {
        audio.capture.cancel();
        audio.source.clear_buffer();
        (audio.capture, audio.receiver)
    });
    let capture = Box::new(CancelledCapture {
        capture: publication.capture,
        receiver: publication.receiver,
        #[cfg(target_os = "windows")]
        audio,
    });
    // SDK sources, encoder, statistics and metadata are disposed on this owner.
    release_cancelled_capture(capture).await
}
async fn release_cancelled_capture(
    capture: Box<CancelledCapture>,
) -> Result<(), CaptureReleaseError> {
    // A failed thread creation drops its closure on the calling thread. Retain
    // the payload outside that closure, until the worker has closed its Room.
    let pending = Arc::new(Mutex::new(Some(capture)));
    let release = pending.clone();
    let owner = match thread::Builder::new()
        .name("relay-capture-release".into())
        .spawn(move || {
            let capture = release.lock().unwrap().take();
            if let Some(capture) = capture {
                let CancelledCapture {
                    capture,
                    receiver,
                    #[cfg(target_os = "windows")]
                    audio,
                } = *capture;
                drop(receiver);
                drop(capture);
                #[cfg(target_os = "windows")]
                drop(audio);
            }
        }) {
        Ok(owner) => owner,
        Err(error) => {
            return Err(CaptureReleaseError {
                error: format!("Release cancelled capture: {error}"),
                capture: pending.lock().unwrap().take(),
            })
        }
    };
    let until = Instant::now() + Duration::from_secs(5);
    while !owner.is_finished() {
        if Instant::now() >= until {
            // A physical driver that hangs remains owned by the release thread.
            // No SDK operation or live encoder is detached by this deadline.
            return Err(CaptureReleaseError {
                error: "Cancelled capture release deadline".into(),
                capture: None,
            });
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    owner.join().map_err(|_| CaptureReleaseError {
        error: "Cancelled capture release failed".into(),
        capture: None,
    })?;
    Ok(())
}
fn retain_failed_capture(jobs: &mut SourceJobs, mut publication: Publication) {
    publication.capture.cancel();
    publication.pipeline = VideoPipeline::Software;
    #[cfg(target_os = "windows")]
    if let Some(audio) = publication.audio.as_ref() {
        audio.capture.cancel();
        audio.source.clear_buffer();
    }
    jobs.failed_releases.push(publication);
}
fn failed_output(error: String, output: SourceJobOutput) -> SourceJobOutput {
    match output {
        SourceJobOutput::CaptureReleaseFailed {
            capture, captures, ..
        } => SourceJobOutput::CaptureReleaseFailed {
            error,
            capture,
            captures,
        },
        SourceJobOutput::Started(Some(publication)) => SourceJobOutput::RetirementFailed {
            error,
            captures: vec![*publication],
        },
        SourceJobOutput::RetirementFailed { captures, .. } => {
            SourceJobOutput::RetirementFailed { error, captures }
        }
        _ => SourceJobOutput::RetirementFailed {
            error,
            captures: Vec::new(),
        },
    }
}
fn retain_failed_output(jobs: &mut SourceJobs, output: SourceJobOutput) {
    match output {
        SourceJobOutput::RetirementFailed { captures, .. } => {
            for publication in captures {
                retain_failed_capture(jobs, publication);
            }
        }
        SourceJobOutput::CaptureReleaseFailed {
            capture, captures, ..
        } => {
            jobs.failed_capture_releases.push(*capture);
            for publication in captures {
                retain_failed_capture(jobs, publication);
            }
        }
        _ => {}
    }
}
fn stop_source_job(
    room: &Arc<Room>,
    publications: &mut HashMap<bool, Publication>,
    jobs: &mut SourceJobs,
    state: &WorkerState,
    camera: bool,
    clear_selection: bool,
) {
    if clear_selection {
        jobs.queued.remove(&camera);
    }
    let retiring = detach_source(publications, state, camera);
    if let Some(job) = jobs.pending.get_mut(&camera) {
        job.discard = true;
        job.registry.cancel();
        if retiring.is_some() {
            job.retiring = retiring;
        }
    } else if let Some(publication) = retiring {
        let registry = PendingLocalTracks::new(state, camera, publication.generation);
        let owned_room = room.clone();
        jobs.pending.insert(
            camera,
            SourceJob {
                registry,
                future: Box::pin(async move {
                    retire_detached_publication(&owned_room, publication).await
                }),
                recovering: false,
                discard: false,
                retiring: None,
                orphans: Vec::with_capacity(2),
            },
        );
    }
}
fn queue_source_start(
    room: &Arc<Room>,
    publications: &mut HashMap<bool, Publication>,
    jobs: &mut SourceJobs,
    state: &WorkerState,
    camera: bool,
    selection: SourceSelection,
) {
    // Replace metadata only; no extra capture/thread/job is created for an older selection.
    jobs.queued.insert(camera, selection);
    stop_source_job(room, publications, jobs, state, camera, false);
}

struct InterruptedVideo {
    generation: u64,
    sid: livekit::id::TrackSid,
    native_id: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum VideoRebind {
    SoftwareFallback((u32, u32)),
    SignalResume((u32, u32)),
}

fn rebind_resumed_video(
    room: &Arc<Room>,
    publications: &mut HashMap<bool, Publication>,
    jobs: &mut SourceJobs,
    state: &WorkerState,
    interrupted: HashMap<bool, InterruptedVideo>,
) -> Result<(), String> {
    for (camera, before) in interrupted {
        let Some(current) = publications.get_mut(&camera) else {
            continue;
        };
        // Full SDK reconnect already replaces SIDs. Resume keeps the old sender
        // and can leave its remote decoder frozen despite Connected/RTP traffic.
        // A stopped, queued, replaced or SDK-republished source is never revived.
        if jobs.pending.contains_key(&camera)
            || current.generation != before.generation
            || current.sid != before.sid
            || current.native_id != before.native_id
            || !current_source(state, camera, before.generation)
            || !room
                .local_participant()
                .track_publications()
                .contains_key(&before.sid)
        {
            continue;
        }
        let resolution = current.source.video_resolution();
        begin_video_rebind(
            room,
            current,
            jobs,
            state,
            camera,
            VideoRebind::SignalResume((resolution.width, resolution.height)),
            #[cfg(test)]
            (None, None),
        )?;
    }
    Ok(())
}

fn begin_video_recovery(
    room: &Arc<Room>,
    current: &mut Publication,
    jobs: &mut SourceJobs,
    state: &WorkerState,
    camera: bool,
    dimensions: (u32, u32),
    #[cfg(test)] probes: (Option<RecoveryGate>, Option<RecoveryPause>),
) -> Result<(), String> {
    begin_video_rebind(
        room,
        current,
        jobs,
        state,
        camera,
        VideoRebind::SoftwareFallback(dimensions),
        #[cfg(test)]
        probes,
    )
}

fn begin_video_rebind(
    room: &Arc<Room>,
    current: &mut Publication,
    jobs: &mut SourceJobs,
    state: &WorkerState,
    camera: bool,
    reason: VideoRebind,
    #[cfg(test)] probes: (Option<RecoveryGate>, Option<RecoveryPause>),
) -> Result<(), String> {
    #[cfg(test)]
    let (before_retire, after_publish) = probes;
    let (dimensions, software_fallback) = match reason {
        VideoRebind::SoftwareFallback(dimensions) => (dimensions, true),
        VideoRebind::SignalResume(dimensions) => (dimensions, false),
    };
    if jobs.pending.contains_key(&camera) {
        return Err("A media operation is already pending for this source".into());
    }
    if (software_fallback && current.options.codec != VideoCodecPreference::Auto)
        || rtc_rgba_bytes(dimensions.0, dimensions.1).is_none()
    {
        return Err("Invalid automatic software video recovery".into());
    }
    let registry = PendingLocalTracks::new(state, camera, current.generation);
    if !registry.current(state, room)? {
        return Ok(());
    }
    let old = room
        .local_participant()
        .track_publications()
        .get(&current.sid)
        .cloned()
        .ok_or("The previous protected video publication is no longer available")?;
    let expected_source = if camera {
        TrackSource::Camera
    } else {
        TrackSource::Screenshare
    };
    let previous_track = state
        .tracks
        .lock()
        .map_err(|_| "Video track ownership unavailable")?
        .get(&camera)
        .cloned()
        .ok_or("The previous video track is no longer available")?;
    let published_track = old
        .track()
        .ok_or("The previous published track is no longer available")?;
    if old.encryption_type() != EncryptionType::Gcm
        || old.kind() != TrackKind::Video
        || old.source() != expected_source
        || published_track.rtc_track().id() != previous_track.rtc_track().id()
        || current.native_id != previous_track.rtc_track().id()
    {
        return Err("The previous video track could not confirm protection and ownership".into());
    }
    previous_track.rtc_track().set_enabled(false);
    registry.register(LocalTrack::Video(previous_track.clone()), expected_source)?;
    let resolution = VideoResolution {
        width: dimensions.0,
        height: dimensions.1,
    };
    let hardware = !software_fallback && current.pipeline.hardware();
    let source = if hardware {
        NativeVideoSource::new_encoded(resolution)
    } else {
        NativeVideoSource::new(resolution, !camera)
    };
    let track = LocalVideoTrack::create_video_track(
        if camera { "camera" } else { "screen" },
        RtcVideoSource::Native(source.clone()),
    );
    track.rtc_track().set_enabled(false);
    registry.register(LocalTrack::Video(track.clone()), expected_source)?;
    let previous_sid = current.sid.clone();
    let previous_native_id = previous_track.rtc_track().id();
    let options = current.options;
    if software_fallback {
        current.pipeline = VideoPipeline::Software;
    }
    if let Some(task) = current.stats_task.take() {
        task.abort();
    }
    current.previous_sample = None;
    current.rate_bitrate = None;
    current.next_frame = None;
    current
        .adaptation
        .observe(VideoPressure::Unknown, Instant::now());
    let owned_room = room.clone();
    let owned_state = state.clone();
    let operation_registry = registry.clone();
    jobs.pending.insert(
        camera,
        SourceJob {
            registry,
            future: Box::pin(async move {
                #[cfg(test)]
                if let Some(gate) = before_retire {
                    let _ = gate.reached.send(());
                    tokio::time::timeout(Duration::from_secs(15), gate.resume)
                        .await
                        .map_err(|_| "Owned pre-retirement probe deadline")?
                        .map_err(|_| "Owned pre-retirement probe ended")?;
                }
                retire_exact_track(
                    &owned_room,
                    RetiringTrack {
                        sid: previous_sid.clone(),
                        native_id: previous_native_id.clone(),
                    },
                )
                .await?;
                if !operation_registry.current(&owned_state, &owned_room)? {
                    return Ok(SourceJobOutput::Recovered(None));
                }
                let publication = tokio::time::timeout(
                    Duration::from_secs(8),
                    owned_room.local_participant().publish_track(
                        LocalTrack::Video(track.clone()),
                        TrackPublishOptions {
                            source: expected_source,
                            video_codec: if hardware {
                                VideoCodec::H264
                            } else {
                                VideoCodec::VP8
                            },
                            video_encoder: if hardware {
                                VideoEncoderBackend::PreEncoded
                            } else {
                                VideoEncoderBackend::Software
                            },
                            video_encoding: Some(VideoEncoding {
                                max_bitrate: u64::from(options.target_bitrate()),
                                max_framerate: f64::from(options.fps),
                            }),
                            simulcast: false,
                            ..Default::default()
                        },
                    ),
                )
                .await
                .map_err(|_| "Protected software recovery publication deadline")?
                .map_err(|error| format!("Publish protected software recovery: {error}"))?;
                track.rtc_track().set_enabled(false);
                if !operation_registry.owns(
                    &owned_state,
                    &publication,
                    &LocalTrack::Video(track.clone()),
                ) || publication
                    .track()
                    .is_none_or(|published| published.rtc_track().id() != track.rtc_track().id())
                {
                    retire_local_publication(&owned_room, &publication.sid()).await?;
                    return Err(
                        "RTC host did not confirm protected software recovery ownership".into(),
                    );
                }
                #[cfg(test)]
                if let Some(pause) = after_publish {
                    let _ = pause.reached.send(publication.sid());
                    tokio::time::timeout(Duration::from_secs(15), pause.resume)
                        .await
                        .map_err(|_| "Owned post-publication probe deadline")?
                        .map_err(|_| "Owned post-publication probe ended")?;
                }
                Ok(SourceJobOutput::Recovered(Some(RecoveryAttachment {
                    previous_sid,
                    previous_native_id,
                    sid: publication.sid(),
                    track,
                    source,
                })))
            }),
            recovering: true,
            discard: false,
            retiring: None,
            orphans: Vec::with_capacity(2),
        },
    );
    Ok(())
}

async fn retire_exact_track(room: &Room, retiring: RetiringTrack) -> Result<(), String> {
    if let Some(publication) = room
        .local_participant()
        .track_publications()
        .get(&retiring.sid)
    {
        if publication
            .track()
            .is_none_or(|track| track.rtc_track().id() != retiring.native_id)
        {
            return Err("Retiring media SID no longer matches its native track".into());
        }
    }
    retire_local_publication(room, &retiring.sid).await
}

fn poll_source_jobs(
    room: &Arc<Room>,
    publications: &mut HashMap<bool, Publication>,
    jobs: &mut SourceJobs,
    state: &WorkerState,
) -> Result<(), String> {
    for camera in [false, true] {
        // Poll one borrowed pinned future at a time. A first error must not drop
        // another ready-but-unprocessed capture or repoll its finished future.
        let result = jobs
            .pending
            .get_mut(&camera)
            .and_then(|job| job.future.as_mut().now_or_never());
        let Some(result) = result else {
            continue;
        };
        let mut job = jobs.pending.remove(&camera).unwrap();
        let output = match result {
            Ok(output) => output,
            Err(error) => {
                job.registry.disable();
                if let Some(publication) = job.retiring.take() {
                    retain_failed_capture(jobs, publication);
                }
                stop_protected_media(state, &error);
                return Err(error);
            }
        };
        if !job.orphans.is_empty() {
            let orphans = std::mem::take(&mut job.orphans);
            let owned_room = room.clone();
            job.future = Box::pin(async move {
                for orphan in orphans {
                    if let Err(error) = retire_exact_track(&owned_room, orphan).await {
                        return Ok(failed_output(error, output));
                    }
                }
                Ok(output)
            });
            jobs.pending.insert(camera, job);
            continue;
        }
        let current = match job.registry.current(state, room) {
            Ok(current) => !job.discard && current,
            Err(error) => {
                retain_failed_output(jobs, failed_output(error.clone(), output));
                if let Some(publication) = job.retiring.take() {
                    retain_failed_capture(jobs, publication);
                }
                job.registry.cancel();
                stop_protected_media(state, &error);
                return Err(error);
            }
        };
        let mut retire = Vec::with_capacity(2);
        match output {
            SourceJobOutput::CaptureReleaseFailed {
                error,
                capture,
                mut captures,
            } => {
                job.registry.cancel();
                if let Some(publication) = job.retiring.take() {
                    captures.push(publication);
                }
                retain_failed_output(
                    jobs,
                    SourceJobOutput::CaptureReleaseFailed {
                        error: error.clone(),
                        capture,
                        captures,
                    },
                );
                stop_protected_media(state, &error);
                return Err(error);
            }
            SourceJobOutput::RetirementFailed {
                error,
                mut captures,
            } => {
                job.registry.cancel();
                if let Some(publication) = job.retiring.take() {
                    captures.push(publication);
                }
                for publication in captures {
                    retain_failed_capture(jobs, publication);
                }
                stop_protected_media(state, &error);
                return Err(error);
            }
            SourceJobOutput::Recovered(Some(replacement)) => {
                let attached = if current {
                    if let Some(publication) = publications.get_mut(&camera) {
                        let mut tracks = state.tracks.lock().map_err(|_| {
                            "Video ownership unavailable during recovery attachment"
                        })?;
                        if publication.generation == job.registry.generation
                            && publication.sid == replacement.previous_sid
                            && tracks.get(&camera).is_some_and(|old| {
                                old.rtc_track().id() == replacement.previous_native_id
                            })
                            && job.registry.current(state, room)?
                            && room
                                .local_participant()
                                .track_publications()
                                .get(&replacement.sid)
                                .is_some_and(|published| {
                                    job.registry.owns(
                                        state,
                                        published,
                                        &LocalTrack::Video(replacement.track.clone()),
                                    ) && published.track().is_some_and(|track| {
                                        track.rtc_track().id() == replacement.track.rtc_track().id()
                                    })
                                })
                        {
                            publication.source = replacement.source;
                            publication.sid = replacement.sid.clone();
                            publication.native_id = replacement.track.rtc_track().id();
                            publication.next_stats = Instant::now() + Duration::from_secs(2);
                            #[cfg(target_os = "windows")]
                            if let VideoPipeline::Hardware(encoder) = &mut publication.pipeline {
                                encoder.request_keyframe();
                            }
                            tracks.insert(camera, replacement.track.clone());
                            replacement.track.rtc_track().set_enabled(true);
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                } else {
                    false
                };
                if !attached {
                    replacement.track.rtc_track().set_enabled(false);
                    job.orphans.push(RetiringTrack {
                        sid: replacement.sid,
                        native_id: replacement.track.rtc_track().id(),
                    });
                    if job.retiring.is_none()
                        && publications.get(&camera).is_some_and(|publication| {
                            publication.generation == job.registry.generation
                                && publication.native_id == replacement.previous_native_id
                        })
                    {
                        job.retiring = detach_source(publications, state, camera);
                    }
                    set_source_phase(
                        &state.source_states[usize::from(camera)],
                        job.registry.generation,
                        0,
                    );
                }
            }
            SourceJobOutput::Started(Some(publication)) => {
                // Keep the capture payload outside the fallible attachment
                // closure. Any SDK/lock/context error retains it until Room close.
                let mut started = Some(publication);
                let attached = (|| -> Result<bool, String> {
                    let publication = started.as_ref().unwrap();
                    if !current {
                        return Ok(false);
                    }
                    let published = room
                        .local_participant()
                        .track_publications()
                        .get(&publication.sid)
                        .cloned();
                    let Some(published) = published else {
                        return Ok(false);
                    };
                    let Some(LocalTrack::Video(track)) = published.track() else {
                        return Ok(false);
                    };
                    if !job
                        .registry
                        .owns(state, &published, &LocalTrack::Video(track.clone()))
                    {
                        return Err("Started video could not confirm protection".into());
                    }
                    #[cfg(target_os = "windows")]
                    let audio_track = if let Some(audio) = publication.audio.as_ref() {
                        let audio_publication = room
                            .local_participant()
                            .track_publications()
                            .get(&audio.sid)
                            .cloned()
                            .ok_or("Started application audio publication disappeared")?;
                        let audio_track = audio_publication
                            .track()
                            .ok_or("Started application audio track disappeared")?;
                        if !job.registry.owns(state, &audio_publication, &audio_track) {
                            return Err(
                                "Started application audio could not confirm protection".into()
                            );
                        }
                        let LocalTrack::Audio(audio_track) = audio_track else {
                            return Err("Started application audio has the wrong kind".into());
                        };
                        Some(audio_track)
                    } else {
                        None
                    };
                    let mut tracks = state
                        .tracks
                        .lock()
                        .map_err(|_| "Started video ownership unavailable")?;
                    #[cfg(target_os = "windows")]
                    let mut audio_tracks = state
                        .audio_tracks
                        .lock()
                        .map_err(|_| "Started audio ownership unavailable")?;
                    if !job.registry.current(state, room)? {
                        return Ok(false);
                    }
                    if publications.contains_key(&camera) {
                        return Err("Started source would replace an attached capture".into());
                    }
                    tracks.insert(camera, track.clone());
                    #[cfg(target_os = "windows")]
                    if let Some(audio_track) = audio_track {
                        audio_tracks.insert(camera, audio_track.clone());
                        audio_track.rtc_track().set_enabled(true);
                    }
                    publications.insert(camera, *started.take().unwrap());
                    set_source_phase(
                        &state.source_states[usize::from(camera)],
                        job.registry.generation,
                        2,
                    );
                    track.rtc_track().set_enabled(true);
                    Ok(true)
                })();
                match attached {
                    Ok(true) => {}
                    Ok(false) => retire.push(*started.take().unwrap()),
                    Err(error) => {
                        job.registry.cancel();
                        if let Some(publication) = started.take() {
                            retain_failed_capture(jobs, *publication);
                        }
                        if let Some(publication) = job.retiring.take() {
                            retain_failed_capture(jobs, publication);
                        }
                        stop_protected_media(state, &error);
                        return Err(error);
                    }
                }
            }
            SourceJobOutput::Recovered(None) => {
                // A cancelled recovery may have retired its old SID without a
                // replacement. Never resume its old encoded source after the
                // pending slot disappears, including legitimate SDK republish.
                if publications.get(&camera).is_some_and(|publication| {
                    publication.generation == job.registry.generation
                        && job.registry.tracks.lock().is_ok_and(|tracks| {
                            tracks
                                .iter()
                                .any(|(owned, _)| owned.rtc_track().id() == publication.native_id)
                        })
                }) {
                    if let Some(publication) = detach_source(publications, state, camera) {
                        retire.push(publication);
                    }
                    set_source_phase(
                        &state.source_states[usize::from(camera)],
                        job.registry.generation,
                        0,
                    );
                }
            }
            SourceJobOutput::Started(None) | SourceJobOutput::Retired => {}
        }
        if let Some(publication) = job.retiring.take() {
            retire.push(publication);
        }
        if !job.orphans.is_empty() || !retire.is_empty() {
            let orphans = std::mem::take(&mut job.orphans);
            let owned_room = room.clone();
            job.registry.disable();
            job.recovering = false;
            job.discard = true;
            job.registry.cancel();
            job.future = Box::pin(async move {
                for orphan in orphans {
                    if let Err(error) = retire_exact_track(&owned_room, orphan).await {
                        return Ok(SourceJobOutput::RetirementFailed {
                            error,
                            captures: retire,
                        });
                    }
                }
                while let Some(publication) = retire.pop() {
                    let outcome = match retire_detached_publication(&owned_room, publication).await
                    {
                        Ok(outcome) => outcome,
                        Err(error) => {
                            return Ok(SourceJobOutput::RetirementFailed {
                                error,
                                captures: retire,
                            })
                        }
                    };
                    match outcome {
                        SourceJobOutput::RetirementFailed {
                            error,
                            mut captures,
                        } => {
                            captures.extend(retire);
                            return Ok(SourceJobOutput::RetirementFailed { error, captures });
                        }
                        SourceJobOutput::CaptureReleaseFailed {
                            error,
                            capture,
                            mut captures,
                        } => {
                            captures.extend(retire);
                            return Ok(SourceJobOutput::CaptureReleaseFailed {
                                error,
                                capture,
                                captures,
                            });
                        }
                        SourceJobOutput::Retired => {}
                        _ => {
                            return Ok(SourceJobOutput::RetirementFailed {
                                error: "Unexpected capture retirement outcome".into(),
                                captures: retire,
                            })
                        }
                    }
                }
                Ok(SourceJobOutput::Retired)
            });
            jobs.pending.insert(camera, job);
        }
    }
    for camera in [false, true] {
        if jobs.pending.contains_key(&camera) {
            continue;
        }
        let Some(selection) = jobs.queued.remove(&camera) else {
            continue;
        };
        if !current_source(state, camera, selection.generation) {
            continue;
        }
        let registry = PendingLocalTracks::new(state, camera, selection.generation);
        let operation_registry = registry.clone();
        let owned_room = room.clone();
        let owned_state = state.clone();
        jobs.pending.insert(
            camera,
            SourceJob {
                registry,
                future: Box::pin(async move {
                    start_publication(
                        &owned_room,
                        &owned_state,
                        camera,
                        selection.generation,
                        selection.source,
                        selection.options,
                        &operation_registry,
                    )
                    .await
                }),
                recovering: false,
                discard: false,
                retiring: None,
                orphans: Vec::with_capacity(2),
            },
        );
    }
    Ok(())
}

fn handle_local_publication(
    room: &Arc<Room>,
    publications: &mut HashMap<bool, Publication>,
    jobs: &mut SourceJobs,
    state: &WorkerState,
    publication: &LocalTrackPublication,
    track: &LocalTrack,
    previous_sid: Option<&livekit::id::TrackSid>,
) -> Result<(), String> {
    if publication.encryption_type() != EncryptionType::Gcm
        || room.local_participant().identity().to_string() != state.call_keys.local_user_id
        || crate::media::validate_call_keys(&state.call_keys).is_err()
    {
        track.rtc_track().set_enabled(false);
        return Err("Local publication could not confirm protected call ownership".into());
    }
    let camera = match (publication.kind(), publication.source()) {
        (TrackKind::Video, TrackSource::Camera) => true,
        (TrackKind::Video, TrackSource::Screenshare)
        | (TrackKind::Audio, TrackSource::ScreenshareAudio) => false,
        _ => {
            track.rtc_track().set_enabled(false);
            return Err("Unexpected protected local media source".into());
        }
    };
    if let Some(job) = jobs.pending.get_mut(&camera) {
        if job.registry.owns(state, publication, track) {
            track.rtc_track().set_enabled(false);
            if previous_sid.is_none() {
                return Ok(());
            }
            // A republished retiring/pending track must never enable or reconcile
            // its old encoder/source. Keep exact cleanup in this bounded slot.
            job.discard = true;
            job.registry.cancel();
        }
    }
    let pending_video = jobs
        .pending
        .get(&camera)
        .is_some_and(|job| job.recovering || job.discard);
    if let Some(current) = publications.get_mut(&camera) {
        if current_source(state, camera, current.generation) {
            let local = room.local_participant().track_publications();
            let same_native = current.native_id == track.rtc_track().id()
                && state.tracks.lock().is_ok_and(|tracks| {
                    tracks
                        .get(&camera)
                        .is_some_and(|owned| owned.rtc_track().id() == track.rtc_track().id())
                });
            let live_native = |sid: &livekit::id::TrackSid| {
                local.get(sid).is_some_and(|actual| {
                    actual.encryption_type() == EncryptionType::Gcm
                        && actual.kind() == publication.kind()
                        && actual.source() == publication.source()
                        && actual
                            .track()
                            .is_some_and(|owned| owned.rtc_track().id() == track.rtc_track().id())
                })
            };
            // Full SDK recovery emits Published(new SID) before Republished(old,
            // new). Reconcile only its exact still-selected native track, after
            // the old SID is gone and the new protected SDK publication exists.
            let sdk_republished = previous_sid.is_none()
                && current.sid != publication.sid()
                && !local.contains_key(&current.sid)
                && live_native(&publication.sid());
            // A delayed event for that same Arc's retired SID cannot disable the
            // now-current publication. Stopped/pending/new-source cases stay out.
            if !pending_video
                && same_native
                && current.sid != publication.sid()
                && !local.contains_key(&publication.sid())
                && live_native(&current.sid)
            {
                return Ok(());
            }
            if !pending_video
                && publication.kind() == TrackKind::Video
                && (current.sid == publication.sid()
                    || previous_sid == Some(&current.sid)
                    || sdk_republished)
                && same_native
                && live_native(&publication.sid())
            {
                current.sid = publication.sid();
                current.native_id = track.rtc_track().id();
                return Ok(());
            }
            #[cfg(target_os = "windows")]
            if let Some(audio) = current.audio.as_mut() {
                let same_audio = audio.native_id == track.rtc_track().id()
                    && state.audio_tracks.lock().is_ok_and(|tracks| {
                        tracks
                            .get(&camera)
                            .is_some_and(|owned| owned.rtc_track().id() == track.rtc_track().id())
                    });
                let sdk_republished_audio = previous_sid.is_none()
                    && audio.sid != publication.sid()
                    && !local.contains_key(&audio.sid)
                    && live_native(&publication.sid());
                if same_audio
                    && audio.sid != publication.sid()
                    && !local.contains_key(&publication.sid())
                    && live_native(&audio.sid)
                {
                    return Ok(());
                }
                if publication.kind() == TrackKind::Audio
                    && (audio.sid == publication.sid()
                        || previous_sid == Some(&audio.sid)
                        || sdk_republished_audio)
                    && same_audio
                    && live_native(&publication.sid())
                {
                    audio.sid = publication.sid();
                    return Ok(());
                }
            }
        }
    }
    track.rtc_track().set_enabled(false);
    if let Some(actual) = room
        .local_participant()
        .track_publications()
        .get(&publication.sid())
    {
        if actual
            .track()
            .is_none_or(|actual| actual.rtc_track().id() != track.rtc_track().id())
        {
            return Err("Late publication SID no longer matches its native track".into());
        }
    } else {
        // Exact pending native IDs were handled above, including early SDK
        // events. An already-retired SID needs no bounded cleanup slot; its
        // stale event must not affect a newer attached source.
        return Ok(());
    }
    let job = jobs.pending.entry(camera).or_insert_with(|| SourceJob {
        registry: PendingLocalTracks::new(
            state,
            camera,
            state.epochs[usize::from(camera)].load(Ordering::Acquire),
        ),
        future: Box::pin(async { Ok(SourceJobOutput::Retired) }),
        recovering: false,
        discard: false,
        retiring: None,
        orphans: Vec::with_capacity(2),
    });
    if !job
        .orphans
        .iter()
        .any(|owned| owned.sid == publication.sid())
    {
        if job.orphans.len() >= 2 {
            return Err("Late publication cleanup exceeded its bound".into());
        }
        job.orphans.push(RetiringTrack {
            sid: publication.sid(),
            native_id: track.rtc_track().id(),
        });
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn stop_failed_application_audio(
    room: &Arc<Room>,
    publication: &mut Publication,
    jobs: &mut SourceJobs,
    state: &WorkerState,
    camera: bool,
) -> Result<(), String> {
    let Some(audio) = publication.audio.take() else {
        return Ok(());
    };
    audio.capture.cancel();
    audio.source.clear_buffer();
    state
        .application_audio_active
        .store(false, Ordering::Release);
    let track = state
        .audio_tracks
        .lock()
        .map_err(|_| "Failed audio ownership unavailable")?
        .remove(&camera);
    let native_id = if let Some(track) = track {
        track.rtc_track().set_enabled(false);
        track.rtc_track().id()
    } else {
        room.local_participant()
            .track_publications()
            .get(&audio.sid)
            .and_then(LocalTrackPublication::track)
            .map(|track| {
                track.rtc_track().set_enabled(false);
                track.rtc_track().id()
            })
            .ok_or("Failed application audio track is no longer available")?
    };
    let job = jobs.pending.entry(camera).or_insert_with(|| SourceJob {
        registry: PendingLocalTracks::new(state, camera, publication.generation),
        future: Box::pin(async { Ok(SourceJobOutput::Retired) }),
        recovering: false,
        discard: false,
        retiring: None,
        orphans: Vec::with_capacity(2),
    });
    if !job.orphans.iter().any(|track| track.sid == audio.sid) {
        if job.orphans.len() >= 2 {
            return Err("Application audio cleanup exceeded its bound".into());
        }
        job.orphans.push(RetiringTrack {
            sid: audio.sid,
            native_id,
        });
    }
    Ok(())
}

fn retired_closed_sender_during_reconnect(
    error: &livekit::RoomError,
    was_reconnecting: bool,
    still_registered: bool,
) -> bool {
    was_reconnecting
        && !still_registered
        && matches!(error,
        livekit::RoomError::Engine(livekit::rtc_engine::EngineError::Rtc(native))
            if matches!(native.error_type, livekit::webrtc::RtcErrorType::InvalidState)
                && native.message == "PeerConnection is closed.")
}

async fn retire_local_publication(room: &Room, sid: &livekit::id::TrackSid) -> Result<(), String> {
    if !room
        .local_participant()
        .track_publications()
        .contains_key(sid)
    {
        return Ok(()); // A queued event can refer to an already retired SID.
    }
    if let Some(track) = room
        .local_participant()
        .track_publications()
        .get(sid)
        .and_then(LocalTrackPublication::track)
    {
        track.rtc_track().set_enabled(false);
    }
    let was_reconnecting = room.connection_state() == ConnectionState::Reconnecting;
    match tokio::time::timeout(
        Duration::from_secs(3),
        room.local_participant().unpublish_track(sid),
    )
    .await
    {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error))
            if retired_closed_sender_during_reconnect(
                &error,
                was_reconnecting,
                room.local_participant()
                    .track_publications()
                    .contains_key(sid),
            ) =>
        {
            // SDK removes the SID before removing its native sender. During a
            // full reconnect that sender's previous PeerConnection is closed.
            // Its already-disabled track/SID is retired; late SDK republish is
            // still handled by the exact current-generation/event guards.
            Ok(())
        }
        Ok(Err(error)) => {
            #[cfg(test)]
            eprintln!("Owned RTC retirement error detail: {error:?}");
            Err(format!("Retire protected video publication: {error}"))
        }
        Err(_) => Err("Retiring the protected video publication timed out".into()),
    }
}

fn recovery_context_current(
    state: &WorkerState,
    camera: bool,
    generation: u64,
    identity: &str,
) -> Result<bool, String> {
    if !current_source(state, camera, generation) {
        return Ok(false);
    }
    crate::media::validate_call_keys(&state.call_keys)?;
    if identity != state.call_keys.local_user_id {
        return Err(
            "Software video recovery belongs to another account. Disconnect and sign in again."
                .into(),
        );
    }
    Ok(true)
}

#[cfg(test)]
async fn recover_software_publication(
    room: &Room,
    current: &mut Publication,
    state: &WorkerState,
    camera: bool,
    dimensions: (u32, u32),
) -> Result<bool, String> {
    let result = recover_software_publication_inner(room, current, state, camera, dimensions).await;
    if let Err(error) = &result {
        // The SDK can acknowledge AddTrack before inserting its local SID map.
        // Closing this owned Room also removes partial publications after a
        // cancelled/timed-out publish; an empty source never receives pixels.
        stop_protected_media(state, error);
    }
    result
}

#[cfg(test)]
async fn recover_software_publication_inner(
    room: &Room,
    current: &mut Publication,
    state: &WorkerState,
    camera: bool,
    (width, height): (u32, u32),
) -> Result<bool, String> {
    let generation = current.generation;
    let identity = room.local_participant().identity().to_string();
    if !recovery_context_current(state, camera, generation, &identity)? {
        return Ok(false);
    }
    if current.options.codec != VideoCodecPreference::Auto
        || rtc_rgba_bytes(width, height).is_none()
    {
        return Err("Invalid automatic software video recovery".into());
    }
    let old = room
        .local_participant()
        .track_publications()
        .get(&current.sid)
        .cloned()
        .ok_or("The previous protected video publication is no longer available")?;
    if old.encryption_type() != EncryptionType::Gcm
        || old.kind() != TrackKind::Video
        || old.source()
            != if camera {
                TrackSource::Camera
            } else {
                TrackSource::Screenshare
            }
    {
        return Err("The previous video publication could not confirm protection".into());
    }
    {
        let tracks = state
            .tracks
            .lock()
            .map_err(|_| "Video track ownership could not be checked")?;
        let old_track = tracks
            .get(&camera)
            .ok_or("The previous video track is no longer available")?;
        let published_track = old
            .track()
            .ok_or("The previous published video track is no longer available")?;
        if published_track.rtc_track().id() != old_track.rtc_track().id() {
            return Err("The previous video track no longer matches its publication".into());
        }
        old_track.rtc_track().set_enabled(false);
    }
    current.pipeline = VideoPipeline::Software; // Release the failed transform before awaiting RTC.
    if let Some(task) = current.stats_task.take() {
        task.abort();
    }
    current.previous_sample = None;
    current.rate_bitrate = None;
    current.next_frame = None;
    current
        .adaptation
        .observe(VideoPressure::Unknown, Instant::now());
    retire_local_publication(room, &current.sid).await?;
    if !recovery_context_current(
        state,
        camera,
        generation,
        &room.local_participant().identity().to_string(),
    )? {
        return Ok(false);
    }
    let source = NativeVideoSource::new(VideoResolution { width, height }, !camera);
    let track = LocalVideoTrack::create_video_track(
        if camera { "camera" } else { "screen" },
        RtcVideoSource::Native(source.clone()),
    );
    track.rtc_track().set_enabled(false);
    let participant = room.local_participant();
    let publishing = participant.publish_track(
        LocalTrack::Video(track.clone()),
        TrackPublishOptions {
            source: if camera {
                TrackSource::Camera
            } else {
                TrackSource::Screenshare
            },
            video_codec: VideoCodec::VP8,
            video_encoder: VideoEncoderBackend::Software,
            video_encoding: Some(VideoEncoding {
                max_bitrate: u64::from(current.options.target_bitrate()),
                max_framerate: f64::from(current.options.fps),
            }),
            simulcast: false,
            ..Default::default()
        },
    );
    tokio::pin!(publishing);
    let deadline = tokio::time::sleep(Duration::from_secs(8));
    tokio::pin!(deadline);
    let mut audio_tick = tokio::time::interval(Duration::from_millis(10));
    audio_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let published = loop {
        tokio::select! {
            result = &mut publishing => break result.map_err(|error| format!("Publish protected software video: {error}")),
            _ = &mut deadline => break Err("Protected software video recovery timed out".into()),
            _ = audio_tick.tick() => {
                #[cfg(target_os = "windows")]
                if current_source(state,camera,generation) {
                    if let Err(error) = pump_application_audio(current,state,camera).await {
                        if let Ok(mut slot)=state.errors.lock(){*slot=Some(error);}
                        stop_application_audio(room,current,state,camera).await;
                        state.ctx.request_repaint();
                    }
                }
            },
        }
    };
    let publication = match published {
        Ok(publication) => publication,
        Err(error) => {
            track.rtc_track().set_enabled(false);
            return Err(error);
        }
    };
    // SDK publish_track enables a track itself. This new source is still empty
    // and remains unpumped until protection and ownership have been checked.
    track.rtc_track().set_enabled(false);
    if publication.encryption_type() != EncryptionType::Gcm
        || publication.kind() != TrackKind::Video
        || publication.source()
            != if camera {
                TrackSource::Camera
            } else {
                TrackSource::Screenshare
            }
    {
        retire_local_publication(room, &publication.sid()).await?;
        return Err("RTC host did not accept protected software video recovery".into());
    }
    #[cfg(test)]
    if let Some(pause) = current.recovery_pause.take() {
        let _ = pause.reached.send(publication.sid());
        tokio::time::timeout(Duration::from_secs(2), pause.resume)
            .await
            .map_err(|_| "Owned recovery timing probe timed out")?
            .map_err(|_| "Owned recovery timing probe ended")?;
    }
    if !recovery_context_current(
        state,
        camera,
        generation,
        &room.local_participant().identity().to_string(),
    )? {
        retire_local_publication(room, &publication.sid()).await?;
        return Ok(false);
    }
    let attached = {
        let mut tracks = state
            .tracks
            .lock()
            .map_err(|_| "Software video track ownership could not be checked")?;
        if current_source(state, camera, generation) {
            current.source = source;
            current.sid = publication.sid();
            current.native_id = track.rtc_track().id();
            current.next_stats = Instant::now() + Duration::from_secs(2);
            tracks.insert(camera, track.clone());
            track.rtc_track().set_enabled(true);
            true
        } else {
            false
        }
    };
    if !attached {
        retire_local_publication(room, &publication.sid()).await?;
    }
    Ok(attached)
}
struct ApplicationAudioGuard {
    sink: Arc<Mutex<Option<ApplicationAudioSink>>>,
    key: String,
}
impl Drop for ApplicationAudioGuard {
    fn drop(&mut self) {
        if let Ok(slot) = self.sink.lock() {
            if let Some(sink) = slot.as_ref() {
                sink.clear_stream(&self.key);
            }
        }
    }
}
async fn start_publication(
    room: &Room,
    state: &WorkerState,
    camera: bool,
    generation: u64,
    capture_source: CaptureSource,
    options: CaptureOptions,
    registry: &PendingLocalTracks,
) -> Result<SourceJobOutput, String> {
    if !registry.current(state, room)? {
        return Ok(SourceJobOutput::Started(None));
    }
    let (width, height) = publication_dimensions(options, &capture_source);
    let bitrate = options.target_bitrate();
    #[cfg(test)]
    let force_software = capture_source.id == "relay-owned-concurrent-fixture";
    #[cfg(not(test))]
    let force_software = false;
    let pipeline = if force_software {
        VideoPipeline::Software
    } else {
        match options.codec {
            VideoCodecPreference::Software => VideoPipeline::Software,
            VideoCodecPreference::Auto | VideoCodecPreference::Hardware => {
                #[cfg(target_os = "windows")]
                {
                    match HardwareEncoder::new(
                        width,
                        height,
                        options.fps,
                        options.target_bitrate_for(width, height, options.fps),
                    ) {
                        Ok(encoder) => VideoPipeline::Hardware(encoder),
                        Err(error) if options.codec == VideoCodecPreference::Hardware => {
                            report_source_error(state, camera, generation, error);
                            return Ok(SourceJobOutput::Started(None));
                        }
                        Err(_) => VideoPipeline::Software,
                    }
                }
                #[cfg(not(target_os = "windows"))]
                {
                    if options.codec == VideoCodecPreference::Hardware {
                        report_source_error(
                            state,
                            camera,
                            generation,
                            "Native hardware H.264 is supported on Windows only".into(),
                        );
                        return Ok(SourceJobOutput::Started(None));
                    }
                    VideoPipeline::Software
                }
            }
        }
    };
    let hardware = pipeline.hardware();
    let source = if hardware {
        NativeVideoSource::new_encoded(VideoResolution { width, height })
    } else {
        NativeVideoSource::new(VideoResolution { width, height }, !camera)
    };
    let track = LocalVideoTrack::create_video_track(
        if camera { "camera" } else { "screen" },
        RtcVideoSource::Native(source.clone()),
    );
    track.rtc_track().set_enabled(false);
    registry.register(
        LocalTrack::Video(track.clone()),
        if camera {
            TrackSource::Camera
        } else {
            TrackSource::Screenshare
        },
    )?;
    let publication = tokio::time::timeout(
        Duration::from_secs(8),
        room.local_participant().publish_track(
            LocalTrack::Video(track.clone()),
            TrackPublishOptions {
                source: if camera {
                    TrackSource::Camera
                } else {
                    TrackSource::Screenshare
                },
                video_codec: if hardware {
                    VideoCodec::H264
                } else {
                    VideoCodec::VP8
                },
                video_encoder: if hardware {
                    VideoEncoderBackend::PreEncoded
                } else {
                    VideoEncoderBackend::Software
                },
                video_encoding: Some(VideoEncoding {
                    max_bitrate: bitrate as u64,
                    max_framerate: options.fps as f64,
                }),
                simulcast: false,
                ..Default::default()
            },
        ),
    )
    .await
    .map_err(|_| "RTC publication timed out".to_string())?
    .map_err(|e| format!("Publish native video: {e}"))?;
    track.rtc_track().set_enabled(false);
    if !registry.owns(state, &publication, &LocalTrack::Video(track.clone()))
        || publication
            .track()
            .is_none_or(|published| published.rtc_track().id() != track.rtc_track().id())
    {
        retire_local_publication(room, &publication.sid()).await?;
        return Err("RTC host did not accept protected video publication".into());
    }
    if !registry.current(state, room)? {
        retire_local_publication(room, &publication.sid()).await?;
        return Ok(SourceJobOutput::Started(None));
    }
    let (sender, receiver) = mpsc::sync_channel(1);
    #[cfg(test)]
    let synthetic = matches!(
        capture_source.id.as_str(),
        "relay-owned-synthetic-fixture" | "relay-owned-concurrent-fixture"
    );
    #[cfg(not(test))]
    let synthetic = false;
    let audio_capture_source = capture_source.clone();
    let capture = if synthetic {
        #[cfg(test)]
        {
            CaptureSession::synthetic_raw(sender, generation, options)
        }
        #[cfg(not(test))]
        {
            unreachable!()
        }
    } else if camera {
        CaptureSession::start_camera_raw(sender, generation, capture_source, options)
    } else {
        CaptureSession::start_screen_raw(sender, generation, capture_source, options)
    };
    let capture = match capture {
        Ok(capture) => capture,
        Err(error) => {
            track.rtc_track().set_enabled(false);
            retire_local_publication(room, &publication.sid()).await?;
            report_source_error(state, camera, generation, error);
            return Ok(SourceJobOutput::Started(None));
        }
    };
    let mut current = Publication {
        capture,
        receiver,
        source,
        sid: publication.sid(),
        native_id: track.rtc_track().id(),
        generation,
        options,
        pipeline,
        adaptation: AdaptiveVideo::new(options),
        stats_task: None,
        next_stats: Instant::now() + Duration::from_secs(2),
        previous_sample: None,
        next_frame: None,
        rate_bitrate: None,
        adaptive_samples: 0,
        #[cfg(test)]
        recovery_pause: None,
        #[cfg(target_os = "windows")]
        audio: None,
    };
    match registry.current(state, room) {
        Ok(false) => return retire_detached_publication(room, current).await,
        Err(error) => {
            return Ok(SourceJobOutput::RetirementFailed {
                error,
                captures: vec![current],
            })
        }
        Ok(true) => {}
    }
    #[cfg(target_os = "windows")]
    if options.application_audio && !camera && current_source(state, camera, generation) {
        match start_application_audio(room, state, generation, &audio_capture_source, registry)
            .await
        {
            Ok(audio) => current.audio = audio,
            Err(error) => {
                current.capture.cancel();
                current.pipeline = VideoPipeline::Software;
                return Ok(SourceJobOutput::RetirementFailed {
                    error,
                    captures: vec![current],
                });
            }
        }
    }
    match registry.current(state, room) {
        Ok(false) => retire_detached_publication(room, current).await,
        Err(error) => Ok(SourceJobOutput::RetirementFailed {
            error,
            captures: vec![current],
        }),
        Ok(true) => Ok(SourceJobOutput::Started(Some(Box::new(current)))),
    }
}
fn current_source(state: &WorkerState, camera: bool, generation: u64) -> bool {
    !state.stop.load(Ordering::Acquire)
        && state.epochs[usize::from(camera)].load(Ordering::Acquire) == generation
}
fn report_source_error(state: &WorkerState, camera: bool, generation: u64, error: String) {
    if current_source(state, camera, generation)
        && set_source_phase(&state.source_states[usize::from(camera)], generation, 0)
    {
        if let Ok(mut slot) = state.errors.lock() {
            *slot = Some(error);
        }
        state.ctx.request_repaint();
    }
}
#[cfg(target_os = "windows")]
async fn start_application_audio(
    room: &Room,
    state: &WorkerState,
    generation: u64,
    capture_source: &CaptureSource,
    registry: &PendingLocalTracks,
) -> Result<Option<ApplicationPublication>, String> {
    if capture_source.kind != crate::capture::SourceKind::Window {
        if let Ok(mut slot) = state.errors.lock() {
            *slot = Some("Application audio requires a selected window".into());
        }
        state.ctx.request_repaint();
        return Ok(None);
    }
    let source = NativeAudioSource::new(
        AudioSourceOptions {
            echo_cancellation: false,
            noise_suppression: false,
            auto_gain_control: false,
        },
        48_000,
        2,
        0,
    );
    let track = LocalAudioTrack::create_audio_track(
        "selected-app-audio",
        RtcAudioSource::Native(source.clone()),
    );
    track.rtc_track().set_enabled(false);
    registry.register(
        LocalTrack::Audio(track.clone()),
        TrackSource::ScreenshareAudio,
    )?;
    let publication = tokio::time::timeout(
        Duration::from_secs(8),
        room.local_participant().publish_track(
            LocalTrack::Audio(track.clone()),
            TrackPublishOptions {
                source: TrackSource::ScreenshareAudio,
                dtx: false,
                stereo: true,
                audio_encoding: Some(livekit::options::AudioEncoding {
                    max_bitrate: 128_000,
                }),
                ..Default::default()
            },
        ),
    )
    .await
    .map_err(|_| "Application audio publication timed out")?
    .map_err(|e| format!("Publish selected app audio: {e}"))?;
    track.rtc_track().set_enabled(false);
    if !registry.owns(state, &publication, &LocalTrack::Audio(track.clone()))
        || publication
            .track()
            .is_none_or(|published| published.rtc_track().id() != track.rtc_track().id())
    {
        retire_local_publication(room, &publication.sid()).await?;
        return Err("RTC host did not accept protected application audio".into());
    }
    if !current_source(state, false, generation) {
        track.rtc_track().set_enabled(false);
        retire_local_publication(room, &publication.sid()).await?;
        return Ok(None);
    }
    let (sender, receiver) = mpsc::sync_channel(2);
    let capture = match ProcessAudioSession::start(capture_source, sender) {
        Ok(capture) => capture,
        Err(error) => {
            track.rtc_track().set_enabled(false);
            retire_local_publication(room, &publication.sid()).await?;
            if let Ok(mut slot) = state.errors.lock() {
                *slot = Some(error);
            }
            state.ctx.request_repaint();
            return Ok(None);
        }
    };
    if !current_source(state, false, generation) {
        // Give the cancelled capture back to the containing Publication. The
        // outer registry check owns exact retirement and off-pump release.
        capture.cancel();
        track.rtc_track().set_enabled(false);
    }
    Ok(Some(ApplicationPublication {
        capture,
        receiver,
        source,
        sid: publication.sid(),
        native_id: track.rtc_track().id(),
    }))
}
#[cfg(all(test, target_os = "windows"))]
async fn attach_synthetic_application_audio(
    room: &Room,
    publication: &mut Publication,
    state: &WorkerState,
    receiver: Receiver<ProcessAudioFrame>,
) -> Result<livekit::id::TrackSid, String> {
    if publication.audio.is_some() || !current_source(state, false, publication.generation) {
        return Err("Owned synthetic screen cannot attach audio".into());
    }
    let source = NativeAudioSource::new(AudioSourceOptions::default(), 48_000, 2, 0);
    let track = LocalAudioTrack::create_audio_track(
        "owned-generated-application-audio",
        RtcAudioSource::Native(source.clone()),
    );
    track.rtc_track().set_enabled(false);
    let accepted = tokio::time::timeout(
        Duration::from_secs(8),
        room.local_participant().publish_track(
            LocalTrack::Audio(track.clone()),
            TrackPublishOptions {
                source: TrackSource::ScreenshareAudio,
                dtx: false,
                stereo: true,
                audio_encoding: Some(livekit::options::AudioEncoding {
                    max_bitrate: 128_000,
                }),
                ..Default::default()
            },
        ),
    )
    .await
    .map_err(|_| "Owned generated audio publication deadline")?
    .map_err(|error| format!("Owned generated audio publication: {error}"))?;
    track.rtc_track().set_enabled(false);
    if accepted.encryption_type() != EncryptionType::Gcm
        || accepted.source() != TrackSource::ScreenshareAudio
        || accepted
            .track()
            .is_none_or(|published| published.rtc_track().id() != track.rtc_track().id())
    {
        retire_local_publication(room, &accepted.sid()).await?;
        return Err("Owned generated audio did not confirm protection".into());
    }
    if !recovery_context_current(
        state,
        false,
        publication.generation,
        &room.local_participant().identity().to_string(),
    )? {
        retire_local_publication(room, &accepted.sid()).await?;
        return Err("Owned generated audio source was replaced".into());
    }
    let sid = accepted.sid();
    publication.audio = Some(ApplicationPublication {
        capture: ProcessAudioSession::synthetic(),
        receiver,
        source,
        sid: sid.clone(),
        native_id: track.rtc_track().id(),
    });
    state
        .audio_tracks
        .lock()
        .map_err(|_| "Owned generated audio track slot unavailable")?
        .insert(false, track.clone());
    track.rtc_track().set_enabled(true);
    Ok(sid)
}
#[cfg(target_os = "windows")]
async fn pump_application_audio(
    publication: &mut Publication,
    state: &WorkerState,
    camera: bool,
) -> Result<(), String> {
    if !current_source(state, camera, publication.generation) {
        return Ok(());
    }
    let Some(audio) = publication.audio.as_mut() else {
        return Ok(());
    };
    if let Some(error) = audio.capture.take_error() {
        return Err(error);
    }
    state
        .application_audio_active
        .store(audio.capture.is_active(), Ordering::Release);
    for _ in 0..4 {
        let Ok(frame) = audio.receiver.try_recv() else {
            break;
        };
        if !current_source(state, camera, publication.generation) {
            break;
        }
        if frame.samples.len() != 1920 || frame.timestamp_us < 0 {
            return Err("Invalid selected app audio frame".into());
        }
        // The capture worker already supplies bounded, paced20ms frames.
        // The native source's extra20ms queue can time out during reconnect and
        // retire healthy application audio. Its direct path accepts exact10ms
        // stereo chunks; preserve every sample and the same40ms whole-frame limit.
        let deadline = tokio::time::Instant::now() + Duration::from_millis(40);
        for samples in frame.samples.chunks_exact(480 * 2) {
            if !current_source(state, camera, publication.generation) {
                audio.source.clear_buffer();
                return Ok(());
            }
            let chunk = AudioFrame {
                data: samples.into(),
                sample_rate: 48_000,
                num_channels: 2,
                samples_per_channel: 480,
            };
            tokio::time::timeout_at(deadline, audio.source.capture_frame(&chunk))
                .await
                .map_err(|_| "Selected app audio queue stalled")?
                .map_err(|e| format!("Send selected app audio: {e}"))?;
            // The direct native path completes synchronously. Also enforce the
            // original deadline after a native poll that did not yield.
            if tokio::time::Instant::now() > deadline {
                return Err("Selected app audio queue stalled".into());
            }
            if !current_source(state, camera, publication.generation) {
                audio.source.clear_buffer();
                return Ok(());
            }
        }
    }
    Ok(())
}
#[cfg(all(test, target_os = "windows"))]
async fn stop_application_audio(
    room: &Room,
    publication: &mut Publication,
    state: &WorkerState,
    camera: bool,
) {
    if let Some(audio) = publication.audio.take() {
        audio.capture.cancel();
        audio.source.clear_buffer();
        if let Ok(mut tracks) = state.audio_tracks.lock() {
            if let Some(track) = tracks.remove(&camera) {
                track.rtc_track().set_enabled(false);
            }
        }
        state
            .application_audio_active
            .store(false, Ordering::Release);
        let _ = tokio::time::timeout(
            Duration::from_secs(3),
            room.local_participant().unpublish_track(&audio.sid),
        )
        .await;
    }
}
#[derive(Clone)]
struct VideoSample {
    id: String,
    timestamp: i64,
    encoded: u32,
    bytes_sent: u64,
    pressure: VideoPressure,
}

fn video_sample(stats: &[livekit::webrtc::stats::RtcStats]) -> Option<VideoSample> {
    use livekit::webrtc::stats::{QualityLimitationReason, RtcStats};
    let mut samples = stats.iter().filter_map(|stats| {
        let RtcStats::OutboundRtp(outbound) = stats else {
            return None;
        };
        let data = &outbound.outbound;
        if outbound.stream.kind != "video"
            || outbound.rtc.id.is_empty()
            || outbound.rtc.timestamp <= 0
            || !data.active
            || data.frames_encoded == 0
            || outbound.sent.bytes_sent == 0
            || rgba_bytes(data.frame_width, data.frame_height).is_none()
        {
            return None;
        }
        let pressure = match data.quality_limitation_reason {
            QualityLimitationReason::Bandwidth => VideoPressure::Bandwidth,
            QualityLimitationReason::Cpu => VideoPressure::Cpu,
            QualityLimitationReason::None
                if data
                    .quality_limitation_durations
                    .get("none")
                    .is_some_and(|duration| duration.is_finite() && *duration >= 0.0) =>
            {
                VideoPressure::Healthy
            }
            _ => VideoPressure::Unknown,
        };
        Some(VideoSample {
            id: outbound.rtc.id.clone(),
            timestamp: outbound.rtc.timestamp,
            encoded: data.frames_encoded,
            bytes_sent: outbound.sent.bytes_sent,
            pressure,
        })
    });
    let sample = samples.next()?;
    if samples.next().is_some() {
        return None;
    } // No guess across multiple senders/layers.
    Some(sample)
}

async fn update_video_adaptation(publication: &mut Publication, state: &WorkerState, camera: bool) {
    if !publication.options.adaptive_video || !current_source(state, camera, publication.generation)
    {
        return;
    }
    if publication
        .stats_task
        .as_ref()
        .is_some_and(tokio::task::JoinHandle::is_finished)
    {
        let sample = publication.stats_task.take().unwrap().await.ok().flatten();
        if !current_source(state, camera, publication.generation) {
            return;
        }
        let pressure = match (&publication.previous_sample, &sample) {
            (Some(previous), Some(current))
                if previous.id == current.id
                    && current.timestamp > previous.timestamp
                    && current.encoded >= previous.encoded
                    && current.bytes_sent >= previous.bytes_sent
                    && (current.encoded > previous.encoded
                        || matches!(
                            current.pressure,
                            VideoPressure::Bandwidth | VideoPressure::Cpu
                        )) =>
            {
                current.pressure
            }
            _ => VideoPressure::Unknown,
        };
        publication.previous_sample = sample;
        if pressure != VideoPressure::Unknown {
            publication.adaptive_samples = publication.adaptive_samples.saturating_add(1);
        }
        publication.adaptation.observe(pressure, Instant::now());
    }
    if publication.stats_task.is_none() && Instant::now() >= publication.next_stats {
        publication.next_stats = Instant::now() + Duration::from_secs(2);
        let track = state
            .tracks
            .lock()
            .ok()
            .and_then(|tracks| tracks.get(&camera).cloned());
        if let Some(track) = track {
            // One bounded task per publication. The 5 ms frame pump never waits
            // for RTC telemetry, and stale publications drop/abort their task.
            publication.stats_task = Some(tokio::spawn(async move {
                tokio::time::timeout(Duration::from_millis(500), track.get_stats())
                    .await
                    .ok()?
                    .ok()
                    .and_then(|stats| video_sample(&stats))
            }));
        } else {
            publication
                .adaptation
                .observe(VideoPressure::Unknown, Instant::now());
        }
    }
}

fn resize_for_profile(
    frame: RawCaptureFrame,
    profile: VideoProfile,
) -> Result<RawCaptureFrame, String> {
    if frame.width < 2
        || frame.height < 2
        || !valid_video_dimensions(frame.width, frame.height)
        || frame.rgba.len() != frame.width as usize * frame.height as usize * 4
        || frame.timestamp_us < 0
    {
        return Err("Invalid raw RTC frame".into());
    }
    let (width, height) = profile.quality.dimensions();
    if frame.width <= width
        && frame.height <= height
        && frame.width.is_multiple_of(2)
        && frame.height.is_multiple_of(2)
    {
        return Ok(frame);
    }
    let image = image::RgbaImage::from_raw(frame.width, frame.height, frame.rgba)
        .ok_or("Invalid raw RTC frame")?;
    let image = if image.width() > width || image.height() > height {
        image::DynamicImage::ImageRgba8(image)
            .resize(width, height, image::imageops::FilterType::Triangle)
            .into_rgba8()
    } else {
        image
    };
    let even_width = image.width() & !1;
    let even_height = image.height() & !1;
    if rtc_rgba_bytes(even_width, even_height).is_none() {
        return Err("Invalid resized RTC frame".into());
    }
    // Retain aspect ratio and remove at most one edge pixel for 4:2:0 encoders.
    let image = if image.width() != even_width || image.height() != even_height {
        image::imageops::crop_imm(&image, 0, 0, even_width, even_height).to_image()
    } else {
        image
    };
    Ok(RawCaptureFrame {
        width: image.width(),
        height: image.height(),
        rgba: image.into_raw(),
        timestamp_us: frame.timestamp_us,
    })
}

fn pump_video(
    publication: &mut Publication,
    frame: Option<RawCaptureFrame>,
    state: &WorkerState,
    camera: bool,
    identity: &str,
) -> Result<(), VideoPumpError> {
    if !current_source(state, camera, publication.generation) {
        return Ok(());
    }
    let profile = publication.adaptation.profile();
    let now = Instant::now();
    let frame = frame.filter(|_| {
        if profile.fps >= publication.options.fps {
            publication.next_frame = None;
            return true;
        }
        if publication
            .next_frame
            .is_some_and(|deadline| now + Duration::from_millis(3) < deadline)
        {
            return false;
        }
        publication.next_frame = Some(now + Duration::from_secs_f64(1.0 / f64::from(profile.fps)));
        true
    });
    let conversion = state.video_conversion.try_lock().ok();
    let frame = frame.filter(|_| conversion.is_some());
    if let Some(frame) = frame {
        let frame = resize_for_profile(frame, profile)?;
        match &mut publication.pipeline {
            VideoPipeline::Software => {
                let buffer = rgba_to_i420(&frame)?;
                if current_source(state, camera, publication.generation) {
                    publication.source.capture_frame(&VideoFrame {
                        rotation: VideoRotation::VideoRotation0,
                        timestamp_us: frame.timestamp_us,
                        frame_metadata: None,
                        buffer,
                    });
                }
            }
            #[cfg(target_os = "windows")]
            VideoPipeline::Hardware(encoder) => {
                let width = frame.width;
                let height = frame.height;
                if rtc_rgba_bytes(width, height) != Some(frame.rgba.len()) {
                    return Err("Invalid hardware capture frame".into());
                }
                let bitrate = hardware_video_bitrate(
                    publication.options,
                    width,
                    height,
                    profile.fps,
                    publication.rate_bitrate,
                );
                if encoder.width != width || encoder.height != height || encoder.fps != profile.fps
                {
                    *encoder = hardware_reconfiguration(
                        publication.options.codec,
                        HardwareEncoder::new(width, height, profile.fps, bitrate),
                        (width, height),
                    )?;
                    encoder.request_keyframe();
                }
                encoder.set_bitrate(u64::from(bitrate));
                let mut nv12 = vec![0; width as usize * height as usize * 3 / 2];
                let (y, uv) = nv12.split_at_mut(width as usize * height as usize);
                yuv_helper::abgr_to_nv12(
                    &frame.rgba,
                    frame.width * 4,
                    y,
                    width,
                    uv,
                    width,
                    width as i32,
                    height as i32,
                );
                if current_source(state, camera, publication.generation) {
                    encoder.submit_nv12(nv12, frame.timestamp_us)?;
                }
            }
        }
        if state.video_visible.load(Ordering::Acquire)
            && current_source(state, camera, publication.generation)
        {
            let image = Arc::new(ColorImage::from_rgba_unmultiplied(
                [frame.width as usize, frame.height as usize],
                &frame.rgba,
            ));
            if let Ok(mut cache) = state.frames.lock() {
                if current_source(state, camera, publication.generation) {
                    let kind = if camera { "camera" } else { "screen" };
                    cache.insert(
                        format!("{identity}:{kind}"),
                        RtcFrame {
                            user_id: identity.into(),
                            kind: kind.into(),
                            image,
                            timestamp_us: frame.timestamp_us,
                        },
                    );
                }
            }
            state.ctx.request_repaint();
        }
    }
    #[cfg(target_os = "windows")]
    if let VideoPipeline::Hardware(encoder) = &mut publication.pipeline {
        if publication.source.take_keyframe_request() {
            encoder.request_keyframe();
        }
        if let Some(rate) = publication.source.take_rate_control_request() {
            publication.rate_bitrate = Some(rate.target_bitrate_bps);
            encoder.set_bitrate(u64::from(hardware_video_bitrate(
                publication.options,
                encoder.width,
                encoder.height,
                profile.fps,
                publication.rate_bitrate,
            )));
        }
        for frame in encoder.poll()? {
            if !current_source(state, camera, publication.generation) {
                break;
            }
            publication
                .source
                .capture_encoded_frame(&EncodedVideoFrame {
                    codec: EncodedVideoCodec::H264,
                    payload: &frame.bytes,
                    timestamp_us: frame.timestamp_us,
                    frame_type: if frame.key {
                        EncodedFrameType::Key
                    } else {
                        EncodedFrameType::Delta
                    },
                    resolution: VideoResolution {
                        width: encoder.width,
                        height: encoder.height,
                    },
                    frame_metadata: None,
                });
        }
    }
    Ok(())
}
fn rgba_to_i420(frame: &RawCaptureFrame) -> Result<I420Buffer, String> {
    if rtc_rgba_bytes(frame.width, frame.height) != Some(frame.rgba.len()) || frame.timestamp_us < 0
    {
        return Err("Invalid raw RTC frame".into());
    }
    let mut buffer = I420Buffer::new(frame.width, frame.height);
    let (sy, su, sv) = buffer.strides();
    let (y, u, v) = buffer.data_mut();
    yuv_helper::abgr_to_i420(
        &frame.rgba,
        frame.width * 4,
        y,
        sy,
        u,
        su,
        v,
        sv,
        frame.width as i32,
        frame.height as i32,
    );
    Ok(buffer)
}

#[cfg(test)]
#[path = "rtc_tests.rs"]
mod integration_tests;

#[cfg(test)]
#[path = "rtc_people_tests.rs"]
mod people_tests;
#[cfg(test)]
pub(crate) use people_tests::{probe_live_people_media, PeopleMediaGrant};

#[cfg(test)]
mod tests {
    use super::*;
    use livekit::webrtc::prelude::VideoBuffer;
    #[test]
    fn cache_and_raw_frames_have_hard_bounds() {
        let mut cache = LatestFrames::default();
        for id in 0..30 {
            cache.insert(
                id.to_string(),
                RtcFrame {
                    user_id: id.to_string(),
                    kind: "screen".into(),
                    image: Arc::new(ColorImage::new([640, 360], eframe::egui::Color32::RED)),
                    timestamp_us: id,
                },
            );
        }
        assert_eq!(cache.frames.len(), MAX_STREAMS);
        assert!(
            cache
                .frames
                .values()
                .map(|f| f.frame.image.pixels.len() * 4)
                .sum::<usize>()
                <= MAX_VIDEO_BYTES
        );
        assert!(rgba_to_i420(&RawCaptureFrame {
            width: 1921,
            height: 1,
            rgba: vec![],
            timestamp_us: 0
        })
        .is_err());
        let frame = RawCaptureFrame {
            width: 16,
            height: 16,
            rgba: [200, 30, 60, 255].repeat(256),
            timestamp_us: 0,
        };
        let buffer = rgba_to_i420(&frame).unwrap();
        assert_eq!((buffer.width(), buffer.height()), (16, 16));
        assert!(buffer.data().0.iter().any(|v| *v > 20));
    }
}
