//! Native device capture and encrypted, bounded 2.5/5/10/20 ms Opus voice packets.
//! Streams open only for an explicit voice join, local microphone preview, or speaker cue.

#[cfg(feature = "native-rtc")]
pub use crate::application_audio::ApplicationAudioSink;
#[cfg(feature = "native-rtc")]
use crate::audio_echo::{EchoCanceller, EchoReference};
pub use crate::audio_settings::AudioCue;
use crate::audio_settings::{AudioDevices, AudioSettings, TransmissionMode};
use crate::e2ee::CallKeys;
use crate::voice_jitter::{Speaker, SHORT_WORKER_STALL};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use chacha20poly1305::{
    aead::{Aead, Payload},
    ChaCha20Poly1305, KeyInit, Nonce, XChaCha20Poly1305, XNonce,
};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
#[cfg(test)]
use nnnoiseless::DenoiseState;
#[cfg(test)]
use opus::{Application, Channels, Decoder};
use opus::{Bitrate, Encoder};
use relay_protocol::{
    VoiceTicket, AUDIO_FRAME_SAMPLES, AUDIO_PACKET_V2, AUDIO_PACKET_V3, AUDIO_SAMPLE_RATE,
    AUDIO_V2_HEADER_LEN, AUDIO_WHISPER, UDP_HEADER_LEN, UDP_MAX_PACKET, UDP_PING,
};
use std::{
    collections::{BTreeSet, HashMap, VecDeque},
    net::{SocketAddr, ToSocketAddrs, UdpSocket},
    sync::{
        atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[path = "microphone_preview.rs"]
mod microphone_preview;

const FRAME_TIME: Duration = Duration::from_millis(20);
// Keep at most two codec frames locally. The receive jitter target belongs to
// the packet reorder buffer; adding it again to device playout doubles buffering.
const CAPTURE_SAMPLES: usize = AUDIO_FRAME_SAMPLES * 2;
// Reserve once for larger fallback callbacks. Normal callbacks retain only 40 ms;
// allocated capacity does not impose any waiting period.
const MAX_CAPTURE_SAMPLES: usize = AUDIO_SAMPLE_RATE as usize / 5;
const MAX_PLAYBACK_SAMPLES: usize = AUDIO_FRAME_SAMPLES * 2;
// Normal 5 ms callbacks keep the 40 ms bound. Larger driver/default callbacks
// may retain one device block (up to 200 ms), the existing 5 ms clock reserve,
// and one 2.5 ms mixer quantum of headroom for fractional clock corrections.
const PLAYBACK_QUANTUM_SAMPLES: usize = 120;
const MAX_DEVICE_PLAYBACK_SAMPLES: usize = AUDIO_SAMPLE_RATE as usize / 5
    + PLAYBACK_QUANTUM_SAMPLES
    + crate::voice_output::DEFAULT_RESERVE_SAMPLES;
const MAX_TONE_SAMPLES: usize = crate::sound_customization::MAX_SAMPLES;
const MAX_SPEAKERS: usize = 32;
type SampleQueue = Arc<crate::device_queue::Queue>;
type ToneQueue = Arc<Mutex<VecDeque<f32>>>;
fn sample_queue(samples: VecDeque<f32>) -> SampleQueue {
    Arc::new(crate::device_queue::Queue::from_samples(
        samples,
        MAX_DEVICE_PLAYBACK_SAMPLES,
    ))
}
type ErrorSlot = Arc<Mutex<Option<String>>>;

/// Receive evidence includes faint decoded audio; it is not a speaking detector
/// or a confirmation that the output device played the signal.
fn received_audio_activity(power: f32) -> bool {
    power.is_finite() && power > 0.000001
}

pub(crate) struct AudioControls {
    /// Only the isolated local preview clears rendered history on PTT changes.
    local_preview: bool,
    call_tones: ToneQueue,
    call_tone_epoch: AtomicU64,
    gain: AtomicU32,
    playback_volume: AtomicU32,
    threshold: AtomicU32,
    automatic_sensitivity: AtomicBool,
    automatic_microphone_volume: AtomicBool,
    preserve_speech_start: AtomicBool,
    hold_ms: AtomicU32,
    bitrate_kbps: AtomicU32,
    recover_packet_loss: AtomicBool,
    automatic_loss_recovery: AtomicBool,
    packet_samples: AtomicU32,
    jitter_ms: AtomicU32,
    adaptive_jitter: AtomicBool,
    transmission: AtomicU32,
    suppression: AtomicBool,
    noise_model: AtomicU32,
    noise_strength: AtomicU32,
    low_latency_device: AtomicBool,
    input_timing: Mutex<Option<DeviceTiming>>,
    output_timing: Mutex<Option<DeviceTiming>>,
    output_reserve: AtomicUsize,
    wake: crate::audio_wake::Wake,
    #[cfg(feature = "native-rtc")]
    echo_enabled: AtomicBool,
    #[cfg(feature = "native-rtc")]
    echo_delay_ms: AtomicU32,
    #[cfg(feature = "native-rtc")]
    echo_reference: EchoReference,
    ptt_pressed: AtomicBool,
    ptt_key: AtomicI32,
    attack_ms: AtomicU32,
    release_ms: AtomicU32,
    /// What the voice-activation gate measured, for the Settings bar.
    gate_meter: crate::voice_gate::GateMeter,
    /// Capture RMS before gain and processing, for "no sound from the microphone".
    capture_rms: AtomicU32,
    /// The app follows Windows device changes; an unplugged device then
    /// reopens instead of ending the call.
    follow_devices: AtomicBool,
    /// A stream reported that its device disappeared.
    device_lost: AtomicBool,
    transmitting: AtomicBool,
    peer_volumes: Mutex<HashMap<String, f32>>,
    priority_users: Mutex<BTreeSet<String>>,
    priority_enabled: AtomicBool,
    priority_volume: AtomicU32,
    privacy_generation: AtomicU64,
    device_revision: AtomicU64,
    // Only rare error callbacks and the device owner take this lock. A retired
    // stream may report its own error, but cannot stop the replacement's worker.
    device_errors: Mutex<Option<(ErrorSlot, ErrorSlot)>>,
    whisper_receive_generation: AtomicU64,
    whisper_enabled: AtomicBool,
    whisper_pressed: AtomicBool,
    whisper_target: Mutex<Vec<String>>,
    whisper_ciphers: Mutex<HashMap<String, CallCipher>>,
    whisper_requested: Mutex<BTreeSet<String>>,
    direct_enabled: AtomicBool,
    gateway_enabled: Arc<crate::gateway_control::Toggle>,
    private_network: AtomicBool,
    direct: Mutex<Option<crate::direct_voice::DirectVoice>>,
    tcp_enabled: AtomicBool,
    voice_qos: AtomicBool,
    // Recover the account-free route without requiring the optional TCP setting.
    // The fallback keeps using the same authenticated, encrypted route.
    automatic_tcp: AtomicBool,
    tunnel_config: Mutex<Option<crate::voice_tunnel::Config>>,
    tunnel: Mutex<Option<crate::voice_tunnel::Tunnel>>,
    speaking: Mutex<HashMap<String, Instant>>,
    /// Last non-silent decoded audio per person; not physical playback.
    heard: Mutex<HashMap<String, Instant>>,
    #[cfg(feature = "native-rtc")]
    application_audio: ApplicationAudioSink,
}
impl AudioControls {
    fn set_voice_connection(&self, address: &str, private: bool) {
        self.private_network.store(private, Ordering::Release);
        self.automatic_tcp
            .store(crate::iroh_route::is_iroh(address), Ordering::Release);
    }

    fn tcp_allowed(&self) -> bool {
        self.tcp_enabled.load(Ordering::Acquire) || self.automatic_tcp.load(Ordering::Acquire)
    }

    pub(crate) fn new(settings: &AudioSettings) -> Self {
        Self {
            local_preview: false,
            call_tones: Arc::new(Mutex::new(VecDeque::new())),
            call_tone_epoch: AtomicU64::new(0),
            gain: AtomicU32::new(settings.microphone_gain.to_bits()),
            playback_volume: AtomicU32::new(settings.playback_volume.to_bits()),
            threshold: AtomicU32::new(settings.activation_threshold.to_bits()),
            automatic_sensitivity: AtomicBool::new(settings.automatic_sensitivity()),
            automatic_microphone_volume: AtomicBool::new(settings.automatic_microphone_volume),
            preserve_speech_start: AtomicBool::new(settings.preserve_speech_start),
            hold_ms: AtomicU32::new(settings.activation_hold_ms),
            bitrate_kbps: AtomicU32::new(settings.bitrate_kbps),
            recover_packet_loss: AtomicBool::new(settings.recover_packet_loss),
            automatic_loss_recovery: AtomicBool::new(settings.automatic_loss_recovery),
            packet_samples: AtomicU32::new(settings.packet_samples() as u32),
            jitter_ms: AtomicU32::new(settings.jitter_ms),
            adaptive_jitter: AtomicBool::new(settings.adaptive_jitter),
            transmission: AtomicU32::new(settings.transmission as u32),
            suppression: AtomicBool::new(settings.noise_suppression),
            noise_model: AtomicU32::new(settings.noise_model as u32),
            noise_strength: AtomicU32::new(settings.noise_strength as u32),
            low_latency_device: AtomicBool::new(settings.low_latency_device),
            input_timing: Mutex::new(None),
            output_timing: Mutex::new(None),
            output_reserve: AtomicUsize::new(crate::voice_output::DEFAULT_RESERVE_SAMPLES),
            wake: crate::audio_wake::Wake::new(),
            #[cfg(feature = "native-rtc")]
            echo_enabled: AtomicBool::new(settings.echo_cancellation),
            #[cfg(feature = "native-rtc")]
            echo_delay_ms: AtomicU32::new(settings.echo_delay_ms),
            #[cfg(feature = "native-rtc")]
            echo_reference: EchoReference::default(),
            ptt_pressed: AtomicBool::new(false),
            ptt_key: AtomicI32::new(0),
            attack_ms: AtomicU32::new(settings.activation_attack_ms),
            release_ms: AtomicU32::new(settings.activation_release_ms),
            gate_meter: Default::default(),
            capture_rms: AtomicU32::new(0),
            follow_devices: AtomicBool::new(false),
            device_lost: AtomicBool::new(false),
            transmitting: AtomicBool::new(false),
            peer_volumes: Mutex::new(HashMap::new()),
            priority_users: Mutex::default(),
            priority_enabled: AtomicBool::new(settings.priority_enabled),
            priority_volume: AtomicU32::new(settings.priority_volume.to_bits()),
            privacy_generation: AtomicU64::new(1),
            device_revision: AtomicU64::new(1),
            device_errors: Mutex::new(None),
            whisper_receive_generation: AtomicU64::new(1),
            whisper_enabled: AtomicBool::new(settings.whisper_enabled),
            whisper_pressed: AtomicBool::new(false),
            whisper_target: Mutex::default(),
            whisper_ciphers: Mutex::default(),
            whisper_requested: Mutex::default(),
            direct_enabled: AtomicBool::new(settings.direct_voice_enabled),
            gateway_enabled: Arc::new(crate::gateway_control::Toggle::new(settings.local_voice_gateways)),
            private_network: AtomicBool::new(false),
            direct: Mutex::new(None),
            tcp_enabled: AtomicBool::new(settings.tcp_fallback_enabled),
            voice_qos: AtomicBool::new(settings.voice_qos),
            automatic_tcp: AtomicBool::new(false),
            tunnel_config: Mutex::new(None),
            tunnel: Mutex::new(None),
            speaking: Mutex::new(HashMap::new()),
            heard: Mutex::new(HashMap::new()),
            #[cfg(feature = "native-rtc")]
            application_audio: ApplicationAudioSink::default(),
        }
    }
    fn apply(&self, settings: &AudioSettings) {
        self.apply_with(settings, false);
    }
    /// A running local preview: only a transmission-mode change is a privacy
    /// boundary there, so dragging gain or threshold never restarts RNNoise.
    fn apply_preview(&self, settings: &AudioSettings) {
        self.apply_with(settings, true);
    }
    fn apply_with(&self, settings: &AudioSettings, preview: bool) {
        let mode_changed = self.transmission.load(Ordering::Relaxed) != settings.transmission as u32;
        // Changing playback/receive settings must not reset VAD, RNNoise, gain
        // tracking or Opus history halfway through the local person's sentence.
        let capture_changed = self.gain.load(Ordering::Relaxed)
            != settings.microphone_gain.to_bits()
            || self.threshold.load(Ordering::Relaxed) != settings.activation_threshold.to_bits()
            || self.automatic_sensitivity.load(Ordering::Relaxed)
                != settings.automatic_sensitivity()
            || self.automatic_microphone_volume.load(Ordering::Relaxed)
                != settings.automatic_microphone_volume
            || self.hold_ms.load(Ordering::Relaxed) != settings.activation_hold_ms
            || self.preserve_speech_start.load(Ordering::Relaxed) != settings.preserve_speech_start
            || self.transmission.load(Ordering::Relaxed) != settings.transmission as u32
            || self.suppression.load(Ordering::Relaxed) != settings.noise_suppression
            || self.noise_model.load(Ordering::Relaxed) != settings.noise_model as u32
            || self.noise_strength.load(Ordering::Relaxed) != settings.noise_strength as u32
            || self.whisper_enabled.load(Ordering::Relaxed) != settings.whisper_enabled
            || self.tcp_enabled.load(Ordering::Relaxed) != settings.tcp_fallback_enabled
            || self.direct_enabled.load(Ordering::Relaxed)
                != (settings.direct_voice_enabled && !self.private_network.load(Ordering::Acquire));
        #[cfg(feature = "native-rtc")]
        let capture_changed = capture_changed
            || self.echo_enabled.load(Ordering::Relaxed) != settings.echo_cancellation
            || self.echo_delay_ms.load(Ordering::Relaxed) != settings.echo_delay_ms;
        self.low_latency_device
            .store(settings.low_latency_device, Ordering::Relaxed);
        self.gain
            .store(settings.microphone_gain.to_bits(), Ordering::Relaxed);
        self.playback_volume
            .store(settings.playback_volume.to_bits(), Ordering::Relaxed);
        self.threshold
            .store(settings.activation_threshold.to_bits(), Ordering::Relaxed);
        self.automatic_sensitivity
            .store(settings.automatic_sensitivity(), Ordering::Relaxed);
        self.automatic_microphone_volume
            .store(settings.automatic_microphone_volume, Ordering::Relaxed);
        self.hold_ms
            .store(settings.activation_hold_ms, Ordering::Relaxed);
        // Gate ramps change no privacy boundary; they apply on the next block.
        self.attack_ms
            .store(settings.activation_attack_ms, Ordering::Relaxed);
        self.release_ms
            .store(settings.activation_release_ms, Ordering::Relaxed);
        self.preserve_speech_start
            .store(settings.preserve_speech_start, Ordering::Relaxed);
        self.bitrate_kbps
            .store(settings.bitrate_kbps, Ordering::Relaxed);
        self.recover_packet_loss
            .store(settings.recover_packet_loss, Ordering::Relaxed);
        self.automatic_loss_recovery
            .store(settings.automatic_loss_recovery, Ordering::Relaxed);
        self.packet_samples
            .store(settings.packet_samples() as u32, Ordering::Relaxed);
        if self
            .whisper_enabled
            .swap(settings.whisper_enabled, Ordering::AcqRel)
            != settings.whisper_enabled
        {
            self.whisper_receive_generation
                .fetch_add(1, Ordering::AcqRel);
        }
        self.direct_enabled.store(
            settings.direct_voice_enabled && !self.private_network.load(Ordering::Acquire),
            Ordering::Release,
        );
        if !settings.direct_voice_enabled || self.private_network.load(Ordering::Acquire) {
            if let Ok(mut direct) = self.direct.lock() {
                if let Some(direct) = direct.as_mut() {
                    direct.disable(Instant::now());
                }
            }
        }
        self.voice_qos.store(settings.voice_qos, Ordering::Release);
        self.gateway_enabled.store(settings.local_voice_gateways, Ordering::Release);
        self.tcp_enabled
            .store(settings.tcp_fallback_enabled, Ordering::Relaxed);
        self.priority_enabled
            .store(settings.priority_enabled, Ordering::Relaxed);
        self.priority_volume
            .store(settings.priority_volume.to_bits(), Ordering::Relaxed);
        self.jitter_ms.store(settings.jitter_ms, Ordering::Relaxed);
        self.adaptive_jitter
            .store(settings.adaptive_jitter, Ordering::Relaxed);
        self.transmission
            .store(settings.transmission as u32, Ordering::Relaxed);
        self.suppression
            .store(settings.noise_suppression, Ordering::Relaxed);
        self.noise_model
            .store(settings.noise_model as u32, Ordering::Relaxed);
        self.noise_strength
            .store(settings.noise_strength as u32, Ordering::Relaxed);
        #[cfg(feature = "native-rtc")]
        {
            self.echo_enabled
                .store(settings.echo_cancellation, Ordering::Release);
            self.echo_delay_ms
                .store(settings.echo_delay_ms, Ordering::Relaxed);
        }
        let capture_changed = if preview { mode_changed } else { capture_changed };
        // Publish after the changed capture policy, so an acquired generation
        // includes those stores before accepting queued microphone/tunnel audio.
        if capture_changed {
            self.privacy_generation.fetch_add(1, Ordering::AcqRel);
        }
        self.wake.notify();
    }
}

/// Device discovery never opens a capture or playback stream.
pub fn enumerate_audio_devices() -> Result<AudioDevices, String> {
    let host = cpal::default_host();
    let inputs: BTreeSet<_> = host
        .input_devices()
        .map_err(|e| format!("List microphones: {e}"))?
        .filter_map(|device| device.name().ok())
        .collect();
    let outputs: BTreeSet<_> = host
        .output_devices()
        .map_err(|e| format!("List playback devices: {e}"))?
        .filter_map(|device| device.name().ok())
        .collect();
    let bluetooth = inputs
        .iter()
        .filter(|name| is_bluetooth(name, true))
        .chain(outputs.iter().filter(|name| is_bluetooth(name, false)))
        .cloned()
        .collect();
    Ok(AudioDevices {
        bluetooth,
        default_input: host.default_input_device().and_then(|d| d.name().ok()),
        default_output: host.default_output_device().and_then(|d| d.name().ok()),
        inputs: inputs.into_iter().collect(),
        outputs: outputs.into_iter().collect(),
    })
}

fn is_bluetooth(name: &str, capture: bool) -> bool {
    #[cfg(windows)]
    {
        crate::windows_audio::is_bluetooth(name, capture)
    }
    #[cfg(not(windows))]
    {
        let _ = (name, capture);
        false
    }
}

fn audio_device(
    host: &cpal::Host,
    selected: &Option<String>,
    input: bool,
) -> Result<cpal::Device, String> {
    if let Some(name) = selected {
        let mut devices = if input {
            host.input_devices()
        } else {
            host.output_devices()
        }
        .map_err(|e| format!("List audio devices: {e}"))?;
        return devices
            .find(|device| device.name().ok().as_ref() == Some(name))
            .ok_or_else(|| {
                format!(
                    "Selected {} is unavailable: {name}. Reconnect it or choose another in Voice & Video.",
                    if input {
                        "microphone"
                    } else {
                        "playback device"
                    }
                )
            });
    }
    if input {
        host.default_input_device()
    } else {
        host.default_output_device()
    }
    .ok_or_else(|| {
        if input {
            "No microphone found. Check microphone access and your default input device.".into()
        } else {
            "No playback device found. Select an audio output device.".into()
        }
    })
}

#[derive(Clone, Copy, Debug)]
pub struct DeviceTiming {
    pub period_ms: f32,
    pub default_period_ms: f32,
    pub buffer_ms: f32,
}
#[cfg(windows)]
fn device_timing(info: crate::windows_audio::PeriodInfo) -> DeviceTiming {
    DeviceTiming {
        period_ms: info.period_ms(),
        default_period_ms: info.default_frames as f32 * 1000.0 / info.sample_rate as f32,
        buffer_ms: info.buffer_frames as f32 * 1000.0 / info.sample_rate as f32,
    }
}
#[derive(Clone, Copy, Debug, Default)]
pub struct NetworkStats {
    pub sent_packets: u64,
    pub received_packets: u64,
    pub rejected_packets: u64,
    pub dropped_capture_frames: u64,
    pub dropped_playback_frames: u64,
    pub late_voice_frames: u64,
    pub reordered_voice_frames: u64,
    pub concealed_voice_frames: u64,
    pub adaptive_held_frames: u64,
    pub adaptive_quiet_skips: u64,
    /// Highest nominal receive target of current speakers, not end-to-end delay.
    pub receive_target_ms: Option<u32>,
    /// Authenticated relay round-trip time; excludes capture, encode and playout.
    pub udp_round_trip_ms: Option<f32>,
    pub udp_round_trip_age: Option<Duration>,
    pub input_timing: Option<DeviceTiming>,
    pub output_timing: Option<DeviceTiming>,
    pub direct_active: bool,
    /// This exact live call has an admitted control-plane session. Opt-in and
    /// route selection remain separate; availability does not mean it is used.
    pub gateway_available: bool,
    pub gateway_active: bool,
    pub gateway_round_trip_ms: Option<f32>,
    pub gateway_round_trip_age: Option<Duration>,
    pub direct_round_trip_ms: Option<f32>,
    pub direct_round_trip_age: Option<Duration>,
    /// Authenticated voice probe over the WebSocket fallback, not an HTTP check.
    pub tcp_round_trip_ms: Option<f32>,
    pub tcp_round_trip_age: Option<Duration>,
    pub tcp_active: bool,
    pub udp_reachable: bool,
    pub udp_probe_timed_out: bool,
    /// Remaining bounded recovery time after five seconds without a valid probe.
    pub udp_recovery_remaining_ms: Option<u32>,
    /// Incoming voice over the last 5–10 s, worst speaker; None until enough packets.
    pub voice_quality: Option<crate::voice_quality::Quality>,
}

struct StatsClock(Instant);
impl Default for StatsClock {
    fn default() -> Self {
        Self(Instant::now())
    }
}
impl StatsClock {
    // Zero means no sample. Relative monotonic ticks need no audio-thread lock.
    fn tick(&self, at: Instant) -> u64 {
        at.saturating_duration_since(self.0)
            .as_micros()
            .min((u64::MAX - 1) as u128) as u64
            + 1
    }
    fn age(&self, tick: u64, now: Instant) -> Option<Duration> {
        (tick != 0).then(|| Duration::from_micros(self.tick(now).saturating_sub(tick)))
    }
}

#[derive(Default)]
struct Counters {
    clock: StatsClock,
    sent: AtomicU64,
    #[cfg(test)]
    sent_microphone_audio: AtomicU64,
    received: AtomicU64,
    rejected: AtomicU64,
    capture_drop: AtomicU64,
    playback_drop: AtomicU64,
    late: AtomicU64,
    reordered: AtomicU64,
    concealed: AtomicU64,
    held: AtomicU64,
    quiet_skips: AtomicU64,
    receive_target_ms: AtomicU32,
    udp_rtt_us: AtomicU64,
    udp_rtt_at: AtomicU64,
    direct_active: AtomicBool,
    gateway_available: AtomicBool,
    gateway_active: AtomicBool,
    gateway_rtt_us: AtomicU64,
    gateway_rtt_at: AtomicU64,
    direct_rtt_us: AtomicU64,
    direct_rtt_at: AtomicU64,
    tcp_rtt_us: AtomicU64,
    tcp_rtt_at: AtomicU64,
    tcp_active: AtomicBool,
    /// Worst current speaker, valid only while `voice_quality_known`.
    voice_quality_known: AtomicBool,
    voice_loss_permille: AtomicU32,
    voice_late_permille: AtomicU32,
    voice_jitter_us: AtomicU32,
    // Publish reachability, timeout and its countdown in one atomic value.
    // Bits 0/1 are connected/timed-out; the upper bits hold remaining ms.
    udp_link: AtomicU64,
}
impl Counters {
    fn publish_udp_link(&self, link: crate::voice_recovery::LinkState) {
        use crate::voice_recovery::LinkState;
        let value = match link {
            LinkState::Checking => 0,
            LinkState::Connected => 1,
            LinkState::Expired => 2,
            LinkState::Reconnecting { remaining } => {
                2 | ((remaining.as_millis().clamp(1, u32::MAX as u128) as u64) << 2)
            }
        };
        self.udp_link.store(value, Ordering::Release);
    }
    fn udp_reachable(&self) -> bool {
        self.udp_link.load(Ordering::Acquire) & 1 != 0
    }
    fn stop_udp_link(&self) {
        // Retain the final timeout diagnosis, but retire live reachability and
        // the countdown when this worker stops.
        self.udp_link.fetch_and(2, Ordering::AcqRel);
        self.voice_quality_known.store(false, Ordering::Release);
        crate::voice_priority::publish(false);
    }
    fn snapshot(&self) -> NetworkStats {
        let link = self.udp_link.load(Ordering::Acquire);
        let now = Instant::now();
        let udp_at = self.udp_rtt_at.load(Ordering::Acquire);
        let direct_at = self.direct_rtt_at.load(Ordering::Acquire);
        let tcp_at = self.tcp_rtt_at.load(Ordering::Acquire);
        NetworkStats {
            sent_packets: self.sent.load(Ordering::Relaxed),
            received_packets: self.received.load(Ordering::Relaxed),
            rejected_packets: self.rejected.load(Ordering::Relaxed),
            dropped_capture_frames: self.capture_drop.load(Ordering::Relaxed),
            dropped_playback_frames: self.playback_drop.load(Ordering::Relaxed),
            late_voice_frames: self.late.load(Ordering::Relaxed),
            reordered_voice_frames: self.reordered.load(Ordering::Relaxed),
            concealed_voice_frames: self.concealed.load(Ordering::Relaxed),
            adaptive_held_frames: self.held.load(Ordering::Relaxed),
            adaptive_quiet_skips: self.quiet_skips.load(Ordering::Relaxed),
            receive_target_ms: match self.receive_target_ms.load(Ordering::Relaxed) {
                0 => None,
                ms => Some(ms),
            },
            udp_round_trip_ms: match self.udp_rtt_us.load(Ordering::Relaxed) {
                0 => None,
                value => Some(value as f32 / 1000.0),
            },
            udp_round_trip_age: self.clock.age(udp_at, now),
            input_timing: None,
            output_timing: None,
            direct_active: self.direct_active.load(Ordering::Acquire),
            gateway_available: self.gateway_available.load(Ordering::Acquire),
            gateway_active: self.gateway_active.load(Ordering::Acquire),
            gateway_round_trip_ms: match self.gateway_rtt_us.load(Ordering::Relaxed) {
                0 => None,
                us => Some(us as f32 / 1000.0),
            },
            gateway_round_trip_age: self.clock.age(self.gateway_rtt_at.load(Ordering::Acquire), now),
            direct_round_trip_ms: match self.direct_rtt_us.load(Ordering::Relaxed) {
                0 => None,
                us => Some(us as f32 / 1000.0),
            },
            direct_round_trip_age: self.clock.age(direct_at, now),
            tcp_round_trip_ms: match self.tcp_rtt_us.load(Ordering::Relaxed) {
                0 => None,
                us => Some(us as f32 / 1000.0),
            },
            tcp_round_trip_age: self.clock.age(tcp_at, now),
            tcp_active: self.tcp_active.load(Ordering::Acquire),
            udp_reachable: link & 1 != 0,
            udp_probe_timed_out: link & 2 != 0,
            udp_recovery_remaining_ms: match (link >> 2) as u32 {
                0 => None,
                remaining => Some(remaining),
            },
            voice_quality: self.voice_quality(),
        }
    }
    fn publish_voice_quality(&self, quality: Option<crate::voice_quality::Quality>) {
        let permille = |percent: f32| (percent * 10.0).round().clamp(0.0, 1000.0) as u32;
        if let Some(q) = quality {
            self.voice_loss_permille.store(permille(q.loss_percent), Ordering::Relaxed);
            self.voice_late_permille.store(permille(q.late_percent), Ordering::Relaxed);
            self.voice_jitter_us.store(
                (q.jitter_ms * 1000.0).round().clamp(0.0, u32::MAX as f32) as u32,
                Ordering::Relaxed,
            );
        }
        self.voice_quality_known.store(quality.is_some(), Ordering::Release);
    }
    fn voice_quality(&self) -> Option<crate::voice_quality::Quality> {
        self.voice_quality_known.load(Ordering::Acquire).then(|| crate::voice_quality::Quality {
            loss_percent: self.voice_loss_permille.load(Ordering::Relaxed) as f32 / 10.0,
            late_percent: self.voice_late_permille.load(Ordering::Relaxed) as f32 / 10.0,
            jitter_ms: self.voice_jitter_us.load(Ordering::Relaxed) as f32 / 1000.0,
            expected: 0,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AudioPhase {
    Idle,
    StartingVoice,
    ActiveVoice,
    ChangingDevices,
    StartingMicrophone,
    ActiveMicrophone,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum AudioOperation {
    Voice,
    Microphone,
}

struct OwnerState {
    generation: u64,
    phase: AudioPhase,
}

struct VoiceStart {
    host: String,
    ticket: VoiceTicket,
    stop: Arc<AtomicBool>,
    capture: SampleQueue,
    playback: SampleQueue,
    errors: ErrorSlot,
    counters: Arc<Counters>,
    settings: AudioSettings,
    call_keys: Arc<CallKeys>,
    gateway: Option<crate::gateway_control::Config>,
}

struct MicrophoneStart {
    stop: Arc<AtomicBool>,
    settings: AudioSettings,
    errors: ErrorSlot,
    /// Owned by the engine too, so edits reach the running preview live.
    controls: Arc<AudioControls>,
    taps: Arc<crate::self_listen::MeterTaps>,
}

/// The engine's view of a running "Listen to Myself" preview.
struct PreviewLink {
    settings: AudioSettings,
    controls: Arc<AudioControls>,
    taps: Arc<crate::self_listen::MeterTaps>,
}

pub struct DeviceChangeFailure {
    generation: u64,
    revision: u64,
    pub attempted: AudioSettings,
    pub restored: AudioSettings,
    pub error: String,
    /// Started by following Windows, not by the person: saved choices stay.
    pub follow: bool,
}

struct DeviceReopenRequest {
    generation: u64,
    revision: u64,
    stop: Arc<AtomicBool>,
    settings: AudioSettings,
    controls: Arc<AudioControls>,
    errors: ErrorSlot,
    failures: Arc<Mutex<Option<DeviceChangeFailure>>>,
    follow: bool,
}

enum DeviceReopen {
    Applied,
    Superseded,
    Retained {
        settings: Box<AudioSettings>,
        error: String,
    },
}

enum AudioCommand {
    CallTone {
        cue: AudioCue,
        settings: AudioSettings,
        epoch: u64,
        controls: Arc<AudioControls>,
        deafened: Arc<AtomicBool>,
        errors: ErrorSlot,
    },
    Voice {
        generation: u64,
        request: VoiceStart,
    },
    Microphone {
        generation: u64,
        request: MicrophoneStart,
    },
    StopVoice,
    StopMicrophone,
    Tone {
        generation: u64,
        cue: AudioCue,
        settings: AudioSettings,
        preview: bool,
        errors: ErrorSlot,
    },
    StopSound,
    Shutdown,
}

#[derive(Clone)]
struct AudioShared {
    muted: Arc<AtomicBool>,
    deafened: Arc<AtomicBool>,
    level: Arc<AtomicU32>,
    controls: Arc<AudioControls>,
    tones: ToneQueue,
    sound_errors: ErrorSlot,
    sound_cancel: Arc<Mutex<Option<Arc<AtomicBool>>>>,
}

/// Device-free frontend. Native streams never cross a thread boundary.
pub struct MediaEngine {
    settings: AudioSettings,
    shared: AudioShared,
    state: Arc<Mutex<OwnerState>>,
    sender: Option<SyncSender<AudioCommand>>,
    owner: Option<JoinHandle<()>>,
    #[cfg(windows)]
    ducker: Mutex<Option<crate::audio_duck::Ducker>>,
    stop: Option<Arc<AtomicBool>>,
    operation: Option<AudioOperation>,
    voice_queues: Option<(SampleQueue, SampleQueue)>,
    errors: ErrorSlot,
    counters: Arc<Counters>,
    pending_devices: Arc<Mutex<Option<DeviceReopenRequest>>>,
    device_failures: Arc<Mutex<Option<DeviceChangeFailure>>>,
    preview: Option<PreviewLink>,
}

/// Output-only access to the existing owner; no microphone or voice lifecycle.
#[derive(Clone)]
pub(crate) struct CallAlertAudio {
    sender: SyncSender<AudioCommand>,
    controls: Arc<AudioControls>,
    deafened: Arc<AtomicBool>,
    errors: ErrorSlot,
}
impl CallAlertAudio {
    pub fn stop(&self) {
        self.controls.call_tone_epoch.fetch_add(1, Ordering::AcqRel);
        self.controls
            .call_tones
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }
    pub fn play(&self, cue: AudioCue, settings: AudioSettings) {
        if self.deafened.load(Ordering::Acquire)
            || !crate::sound_customization::audible(&settings, cue)
        {
            return;
        }
        if self
            .sender
            .try_send(AudioCommand::CallTone {
                cue,
                settings,
                epoch: self.controls.call_tone_epoch.load(Ordering::Acquire),
                controls: self.controls.clone(),
                deafened: self.deafened.clone(),
                errors: self.errors.clone(),
            })
            .is_err()
        {
            record_error(
                &self.errors,
                "Call alert audio is busy. Check your sound settings.".into(),
            );
        }
    }
}

impl MediaEngine {
    pub fn new() -> Result<Self, String> {
        let settings = AudioSettings::default();
        Ok(Self {
            shared: AudioShared {
                muted: Arc::new(AtomicBool::new(false)),
                deafened: Arc::new(AtomicBool::new(false)),
                level: Arc::new(AtomicU32::new(0)),
                controls: Arc::new(AudioControls::new(&settings)),
                tones: Arc::new(Mutex::new(VecDeque::new())),
                sound_errors: Arc::new(Mutex::new(None)),
                sound_cancel: Arc::new(Mutex::new(None)),
            },
            settings,
            state: Arc::new(Mutex::new(OwnerState {
                generation: 0,
                phase: AudioPhase::Idle,
            })),
            sender: None,
            owner: None,
            #[cfg(windows)]
            ducker: Mutex::new(None),
            stop: None,
            operation: None,
            voice_queues: None,
            errors: Arc::new(Mutex::new(None)),
            counters: Arc::new(Counters::default()),
            pending_devices: Arc::new(Mutex::new(None)),
            device_failures: Arc::new(Mutex::new(None)),
            preview: None,
        })
    }
    fn phase(&self) -> AudioPhase {
        self.state
            .lock()
            .map(|state| state.phase)
            .unwrap_or(AudioPhase::Idle)
    }
    fn begin(&mut self, phase: AudioPhase) -> (u64, Arc<AtomicBool>) {
        self.cancel();
        self.operation = Some(if phase == AudioPhase::StartingVoice {
            AudioOperation::Voice
        } else {
            AudioOperation::Microphone
        });
        let stop = Arc::new(AtomicBool::new(false));
        self.stop = Some(stop.clone());
        self.errors = Arc::new(Mutex::new(None));
        let mut state = self.state.lock().unwrap();
        state.generation = state.generation.wrapping_add(1);
        state.phase = phase;
        (state.generation, stop)
    }
    fn cancel(&mut self) {
        if let Some(stop) = self.stop.take() {
            stop.store(true, Ordering::Release);
        }
        if let Ok(mut pending) = self.pending_devices.lock() {
            pending.take();
        }
        // Late callbacks keep their old slot, never the next operation's failure channel.
        self.errors = Arc::new(Mutex::new(None));
        self.operation = None;
        self.voice_queues.take();
        if let Ok(mut speaking) = self.shared.controls.speaking.lock() {
            speaking.clear();
        }
        if let Ok(mut heard) = self.shared.controls.heard.lock() {
            heard.clear();
        }
        #[cfg(feature = "native-rtc")]
        self.shared.controls.application_audio.clear();
        self.shared.level.store(0, Ordering::Relaxed);
        self.shared
            .controls
            .transmitting
            .store(false, Ordering::Relaxed);
        if let Ok(mut state) = self.state.lock() {
            state.generation = state.generation.wrapping_add(1);
            state.phase = AudioPhase::Idle;
        }
    }
    fn ensure_owner(&mut self) -> Result<(), String> {
        if self.sender.is_none() {
            let (sender, receiver) = mpsc::sync_channel(8);
            let shared = self.shared.clone();
            let state = self.state.clone();
            let pending_devices = self.pending_devices.clone();
            let owner = thread::Builder::new()
                .name("relay-audio-owner".into())
                .spawn(move || {
                    audio_owner(receiver, state, pending_devices, || {
                        NativeAudio::with_shared(shared)
                    })
                })
                .map_err(|error| format!("Start audio worker: {error}"))?;
            self.sender = Some(sender);
            self.owner = Some(owner);
        }
        Ok(())
    }
    pub(crate) fn call_alert_audio(&mut self) -> Result<CallAlertAudio, String> {
        self.ensure_owner()?;
        Ok(CallAlertAudio {
            sender: self.sender.as_ref().unwrap().clone(),
            controls: self.shared.controls.clone(),
            deafened: self.shared.deafened.clone(),
            errors: self.shared.sound_errors.clone(),
        })
    }
    fn enqueue(&mut self, command: AudioCommand) -> Result<(), String> {
        self.ensure_owner()?;
        match self.sender.as_ref().unwrap().try_send(command) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                Err("Audio is busy finishing a previous device operation. Try again.".into())
            }
            Err(TrySendError::Disconnected(_)) => {
                Err("The audio worker stopped. Restart Relay.".into())
            }
        }
    }
    /// Queues DNS and device startup; is_active becomes true only once streams are playing.
    pub fn start_voice(
        &mut self,
        host: &str,
        ticket: VoiceTicket,
        call_keys: Arc<CallKeys>,
    ) -> Result<(), String> {
        self.start_voice_with_gateway(host, ticket, call_keys, None)
    }
    pub fn start_voice_with_gateway(
        &mut self,
        host: &str,
        ticket: VoiceTicket,
        call_keys: Arc<CallKeys>,
        gateway: Option<crate::gateway_control::Config>,
    ) -> Result<(), String> {
        validate_call_keys(&call_keys)?;
        if ticket.wire_version < 3 {
            return Err("This server cannot relay protected voice. Update the host.".into());
        }
        let (generation, stop) = self.begin(AudioPhase::StartingVoice);
        let capture = sample_queue(VecDeque::with_capacity(MAX_CAPTURE_SAMPLES));
        let playback = sample_queue(VecDeque::with_capacity(MAX_PLAYBACK_SAMPLES));
        self.voice_queues = Some((capture.clone(), playback.clone()));
        self.counters = Arc::new(Counters::default());
        let request = VoiceStart {
            gateway,
            host: host.into(),
            ticket,
            stop,
            capture,
            playback,
            errors: self.errors.clone(),
            counters: self.counters.clone(),
            settings: self.settings.clone(),
            call_keys,
        };
        if let Err(error) = self.enqueue(AudioCommand::Voice {
            generation,
            request,
        }) {
            self.cancel();
            return Err(error);
        }
        Ok(())
    }
    pub fn stop_voice(&mut self) {
        if self.operation == Some(AudioOperation::Voice) {
            self.cancel();
        }
        if let Some(sender) = &self.sender {
            let _ = sender.try_send(AudioCommand::StopVoice);
        }
    }
    pub fn is_starting(&self) -> bool {
        matches!(
            self.phase(),
            AudioPhase::StartingVoice | AudioPhase::ChangingDevices
        )
    }
    pub fn is_changing_devices(&self) -> bool {
        self.phase() == AudioPhase::ChangingDevices
    }
    pub fn is_active(&self) -> bool {
        self.phase() == AudioPhase::ActiveVoice
    }
    pub fn configure(&mut self, settings: AudioSettings) -> bool {
        let settings = settings.normalized();
        let sounds_changed = self.settings.sounds_enabled != settings.sounds_enabled
            || self.settings.sound_volume != settings.sound_volume
            || self.settings.event_sounds != settings.event_sounds;
        let changed = self.settings.input_device != settings.input_device
            || self.settings.output_device != settings.output_device
            || self.settings.low_latency_device != settings.low_latency_device;
        if changed {
            self.shared
                .controls
                .device_revision
                .fetch_add(1, Ordering::AcqRel);
        }
        self.shared.controls.apply(&settings);
        self.settings = settings;
        if changed || sounds_changed {
            self.cancel_sound();
            if sounds_changed {
                self.shared.controls.call_tone_epoch.fetch_add(1, Ordering::AcqRel);
                self.shared.controls.call_tones.lock().unwrap_or_else(|error| error.into_inner()).clear();
            }
            if let Some(sender) = &self.sender {
                let _ = sender.try_send(AudioCommand::StopSound);
            }
        }
        changed && self.phase() != AudioPhase::Idle
    }

    /// Coalesce rapid device choices; retain the voice worker, ticket, nonce
    /// sequence and receive state. No server rejoin or RTC restart is involved.
    pub fn reopen_voice_devices(&mut self) -> Result<(), String> {
        self.reopen_voice_devices_for(false)
    }

    fn reopen_voice_devices_for(&mut self, follow: bool) -> Result<(), String> {
        if self.operation != Some(AudioOperation::Voice) {
            return Ok(());
        }
        let Some(stop) = self
            .stop
            .clone()
            .filter(|stop| !stop.load(Ordering::Acquire))
        else {
            return Ok(());
        };
        // Publish phase and request under one state lock, so a fast owner cannot
        // finish before the UI marks this request as changing.
        let mut state = self.state.lock().map_err(|_| "Audio state stopped.")?;
        let generation = state.generation;
        let mut pending = self
            .pending_devices
            .lock()
            .map_err(|_| "Audio device selection stopped.")?;
        *pending = Some(DeviceReopenRequest {
            generation,
            revision: self.shared.controls.device_revision.load(Ordering::Acquire),
            stop,
            settings: self.settings.clone(),
            controls: self.shared.controls.clone(),
            errors: self.errors.clone(),
            failures: self.device_failures.clone(),
            follow,
        });
        if state.phase == AudioPhase::ActiveVoice {
            state.phase = AudioPhase::ChangingDevices;
        }
        Ok(())
    }

    /// The input and output names the engine opens (None: Windows default).
    pub(crate) fn device_selection(&self) -> (Option<String>, Option<String>) {
        (self.settings.input_device.clone(), self.settings.output_device.clone())
    }

    /// Windows moved a default or a device came or went: reopen the same names.
    /// Only the device streams are replaced; the call, mute, deafen and
    /// per-person volumes are untouched.
    pub(crate) fn follow_device_change(&mut self, effective: AudioSettings) -> Result<(), String> {
        self.configure(effective);
        self.shared.controls.device_revision.fetch_add(1, Ordering::AcqRel);
        if self.operation == Some(AudioOperation::Voice) {
            self.reopen_voice_devices_for(true)?;
        }
        Ok(())
    }

    /// Turn on reopening (instead of stopping) when a device disappears.
    pub(crate) fn set_device_following(&mut self, on: bool) {
        self.shared.controls.follow_devices.store(on, Ordering::Release);
    }

    /// A call or preview stream lost its device since the last check.
    pub(crate) fn take_device_lost(&mut self) -> bool {
        let call = self.shared.controls.device_lost.swap(false, Ordering::AcqRel);
        let preview = self
            .preview
            .as_ref()
            .is_some_and(|preview| preview.controls.device_lost.swap(false, Ordering::AcqRel));
        call || preview
    }

    /// The preview's input and output names, while it runs.
    pub(crate) fn preview_device_selection(&self) -> Option<(Option<String>, Option<String>)> {
        self.preview
            .as_ref()
            .filter(|_| self.is_microphone_test_active())
            .map(|preview| (preview.settings.input_device.clone(), preview.settings.output_device.clone()))
    }

    /// Reopen a running Listen to Myself preview on the current devices.
    pub(crate) fn reopen_microphone_preview(&mut self, settings: &AudioSettings) -> Result<(), String> {
        if self.preview.is_some() && self.is_microphone_test_active() {
            self.stop_microphone_test();
            self.preview_microphone(settings.clone())?;
        }
        Ok(())
    }

    pub fn take_device_change_failure(&mut self) -> Option<DeviceChangeFailure> {
        let failure = self.device_failures.lock().ok()?.take()?;
        if !current_generation(&self.state, failure.generation)
            || self.shared.controls.device_revision.load(Ordering::Acquire) != failure.revision
        {
            return None;
        }
        crate::audio_settings::restore_device_selection(
            &mut self.settings,
            &failure.attempted,
            &failure.restored,
        );
        self.shared
            .controls
            .low_latency_device
            .store(failure.restored.low_latency_device, Ordering::Release);
        Some(failure)
    }
    #[cfg_attr(not(test), allow(dead_code))] // Manual state remains available to platform adapters/fixtures.
    pub fn set_push_to_talk_pressed(&mut self, pressed: bool) {
        self.shared
            .controls
            .ptt_pressed
            .store(pressed, Ordering::Release);
    }
    /// A zero key preserves manually supplied state for tests and non-Windows callers.
    pub fn set_push_to_talk_key(&mut self, key: i32) {
        self.shared.controls.ptt_key.store(key, Ordering::Release);
        if let Some(preview) = &self.preview {
            preview.controls.ptt_key.store(key, Ordering::Release);
            preview.controls.ptt_pressed.store(false, Ordering::Release);
            preview.controls.privacy_generation.fetch_add(1, Ordering::AcqRel);
            preview.controls.wake.notify();
        }
    }
    pub fn is_transmitting(&self) -> bool {
        self.shared.controls.transmitting.load(Ordering::Relaxed)
    }
    #[cfg(test)]
    pub fn start_microphone_test(&mut self) -> Result<(), String> {
        self.preview_microphone(self.settings.clone())
    }
    /// Test a snapshot of the editor without applying it to subsequent calls.
    pub fn preview_microphone(&mut self, settings: AudioSettings) -> Result<(), String> {
        if self.is_active() || self.is_starting() {
            return Err("Leave voice before starting a microphone test.".into());
        }
        let (generation, stop) = self.begin(AudioPhase::StartingMicrophone);
        let settings = crate::self_listen::preview_settings(&settings);
        let ptt_key = self.shared.controls.ptt_key.load(Ordering::Acquire);
        let controls = Arc::new(microphone_preview::controls(&settings, ptt_key));
        controls.follow_devices.store(
            self.shared.controls.follow_devices.load(Ordering::Acquire),
            Ordering::Release,
        );
        let taps = Arc::new(crate::self_listen::MeterTaps::default());
        let request = MicrophoneStart {
            stop,
            settings: settings.clone(),
            errors: self.errors.clone(),
            controls: controls.clone(),
            taps: taps.clone(),
        };
        if let Err(error) = self.enqueue(AudioCommand::Microphone {
            generation,
            request,
        }) {
            self.cancel();
            return Err(error);
        }
        self.preview = Some(PreviewLink { settings, controls, taps });
        Ok(())
    }
    /// Apply an edited draft to the running preview. Device edits reopen the
    /// preview's own streams; everything else is heard on the next block.
    pub fn update_microphone_preview(
        &mut self,
        settings: &AudioSettings,
    ) -> Result<crate::self_listen::PreviewChange, String> {
        use crate::self_listen::PreviewChange;
        if !self.is_microphone_test_active() {
            self.preview = None;
        }
        let Some(preview) = self.preview.as_mut() else {
            return Ok(PreviewChange::None);
        };
        let settings = crate::self_listen::preview_settings(settings);
        match crate::self_listen::preview_change(&preview.settings, &settings) {
            PreviewChange::None => Ok(PreviewChange::None),
            PreviewChange::Live => {
                preview.controls.apply_preview(&settings);
                preview.settings = settings;
                Ok(PreviewChange::Live)
            }
            PreviewChange::Reopen => {
                self.stop_microphone_test();
                self.preview_microphone(settings).map(|()| PreviewChange::Reopen)
            }
        }
    }
    /// Level taps since the previous read, the device periods measured by the
    /// preview's own streams, and whether its push-to-talk key is held.
    pub(crate) fn microphone_preview_meter(
        &self,
    ) -> Option<(crate::self_listen::MeterSample, Option<f32>, Option<f32>, bool)> {
        let preview = self.preview.as_ref().filter(|_| self.is_microphone_test_active())?;
        let period = |timing: &Mutex<Option<DeviceTiming>>| {
            timing.lock().ok().and_then(|timing| timing.as_ref().map(|timing| timing.period_ms))
        };
        Some((
            preview.taps.take(),
            period(&preview.controls.input_timing),
            period(&preview.controls.output_timing),
            preview.controls.ptt_pressed.load(Ordering::Acquire),
        ))
    }
    /// What the voice-activation gate measured since the previous read: from
    /// the running preview, else from the call's own capture.
    pub(crate) fn voice_gate_meter(&self) -> Option<crate::voice_gate::GateReading> {
        if let Some(preview) = self.preview.as_ref().filter(|_| self.is_microphone_test_active()) {
            return Some(preview.controls.gate_meter.take());
        }
        self.is_active().then(|| self.shared.controls.gate_meter.take())
    }
    /// Gate threshold, sensitivity mode and ramps of an unsaved draft, heard in
    /// the running call on the next block. None of them is a privacy boundary,
    /// so no history is discarded; Reset or closing Settings writes them back.
    pub(crate) fn preview_gate(&self, settings: &AudioSettings) {
        let settings = settings.clone().normalized();
        let controls = &self.shared.controls;
        controls
            .threshold
            .store(settings.activation_threshold.to_bits(), Ordering::Relaxed);
        controls
            .automatic_sensitivity
            .store(settings.automatic_sensitivity(), Ordering::Relaxed);
        controls
            .hold_ms
            .store(settings.activation_hold_ms, Ordering::Relaxed);
        controls
            .attack_ms
            .store(settings.activation_attack_ms, Ordering::Relaxed);
        controls
            .release_ms
            .store(settings.activation_release_ms, Ordering::Relaxed);
    }
    pub fn stop_microphone_test(&mut self) {
        self.preview.take();
        if self.operation == Some(AudioOperation::Microphone) {
            self.cancel();
        }
        if let Some(sender) = &self.sender {
            let _ = sender.try_send(AudioCommand::StopMicrophone);
        }
    }
    pub fn is_microphone_test_active(&self) -> bool {
        matches!(
            self.phase(),
            AudioPhase::StartingMicrophone | AudioPhase::ActiveMicrophone
        )
    }
    pub fn play_tone(&mut self, cue: AudioCue) -> Result<(), String> {
        self.queue_tone(cue, self.settings.clone(), false)
    }
    /// Output-only preview; never changes the microphone or the active call.
    #[cfg(test)]
    pub fn preview_sound(&mut self, settings: AudioSettings) -> Result<(), String> {
        self.preview_cue(AudioCue::Unmute, settings)
    }
    /// Preview exactly one event's edited sound without applying audio settings.
    pub fn preview_cue(&mut self, cue: AudioCue, settings: AudioSettings) -> Result<(), String> {
        self.queue_tone(cue, settings.normalized(), true)
    }
    fn queue_tone(
        &mut self,
        cue: AudioCue,
        settings: AudioSettings,
        preview: bool,
    ) -> Result<(), String> {
        if !crate::sound_customization::audible(&settings, cue) {
            return Ok(());
        }
        let generation = self.state.lock().map(|state| state.generation).unwrap_or(0);
        self.enqueue(AudioCommand::Tone {
            generation,
            cue,
            settings,
            preview,
            errors: self.shared.sound_errors.clone(),
        })
    }
    pub fn set_muted(&mut self, value: bool) {
        self.shared
            .controls
            .privacy_generation
            .fetch_add(1, Ordering::AcqRel);
        self.shared.muted.store(value, Ordering::Release);
        self.shared.controls.wake.notify();
    }
    pub fn set_deafened(&mut self, value: bool) {
        self.shared
            .controls
            .privacy_generation
            .fetch_add(1, Ordering::AcqRel);
        self.shared.deafened.store(value, Ordering::Release);
        self.shared.controls.wake.notify();
        #[cfg(feature = "native-rtc")]
        self.shared.controls.application_audio.clear();
    }

    /// Raw microphone RMS of the call (before gain and noise suppression).
    pub(crate) fn capture_level(&self) -> f32 {
        f32::from_bits(self.shared.controls.capture_rms.load(Ordering::Relaxed))
    }

    pub fn input_level(&self) -> f32 {
        if self.shared.muted.load(Ordering::Relaxed) && !self.is_microphone_test_active() {
            0.0
        } else {
            f32::from_bits(self.shared.level.load(Ordering::Relaxed))
        }
    }
    pub fn set_user_volume(&self, user_id: &str, volume: f32) {
        let volume = if volume.is_finite() {
            volume.clamp(0.0, 2.0)
        } else {
            1.0
        };
        if let Ok(mut volumes) = self.shared.controls.peer_volumes.lock() {
            if volume == 1.0 {
                volumes.remove(user_id);
            } else if volumes.contains_key(user_id) || volumes.len() < MAX_SPEAKERS * 2 {
                volumes.insert(user_id.to_owned(), volume);
            } else {
                return;
            }
            #[cfg(feature = "native-rtc")]
            self.shared
                .controls
                .application_audio
                .set_user_volume(user_id, volume);
        }
    }
    pub fn user_volume(&self, user_id: &str) -> f32 {
        self.shared
            .controls
            .peer_volumes
            .lock()
            .ok()
            .and_then(|v| v.get(user_id).copied())
            .unwrap_or(1.0)
    }
    #[cfg(feature = "native-rtc")]
    pub fn application_audio_sink(&self) -> ApplicationAudioSink {
        self.shared.controls.application_audio.clone()
    }
    pub fn active_settings(&self) -> &AudioSettings {
        &self.settings
    }
    pub fn retain_whisper_peers(&self, peers: &BTreeSet<String>) {
        if let Ok(mut ciphers) = self.shared.controls.whisper_ciphers.lock() {
            let old = ciphers.len();
            ciphers.retain(|peer, _| peers.contains(peer));
            if old != ciphers.len() {
                self.shared
                    .controls
                    .whisper_receive_generation
                    .fetch_add(1, Ordering::AcqRel);
            }
        }
        if let Ok(mut targets) = self.shared.controls.whisper_target.lock() {
            if targets.iter().any(|peer| !peers.contains(peer)) {
                targets.clear();
                self.shared
                    .controls
                    .privacy_generation
                    .fetch_add(1, Ordering::AcqRel);
            }
        }
        if let Ok(mut requested) = self.shared.controls.whisper_requested.lock() {
            requested.retain(|peer| peers.contains(peer));
        }
    }
    pub fn prepare_whisper(&self, peer: &str) -> bool {
        self.shared
            .controls
            .whisper_requested
            .lock()
            .is_ok_and(|mut requested| {
                requested.len() < MAX_SPEAKERS && requested.insert(peer.into())
            })
    }
    pub fn select_whisper(&self, peer: &str) {
        self.shared
            .controls
            .privacy_generation
            .fetch_add(1, Ordering::AcqRel);
        if let Ok(mut target) = self.shared.controls.whisper_target.lock() {
            *target = vec![peer.into()];
        }
        self.invalidate_whisper(peer);
        if let Ok(mut requested) = self.shared.controls.whisper_requested.lock() {
            requested.remove(peer);
        }
    }
    pub fn invalidate_whisper(&self, peer: &str) {
        self.shared
            .controls
            .whisper_receive_generation
            .fetch_add(1, Ordering::AcqRel);
        self.shared
            .controls
            .privacy_generation
            .fetch_add(1, Ordering::AcqRel);
        if let Ok(mut ciphers) = self.shared.controls.whisper_ciphers.lock() {
            ciphers.remove(peer);
        }
    }
    pub fn install_whisper(&self, peer: &str, keys: Arc<CallKeys>) -> Result<(), String> {
        if keys.sender_keys.len() != 2
            || !keys.sender_keys.contains_key(peer)
            || peer == keys.local_user_id
            || {
                let mut users = [keys.local_user_id.as_str(), peer];
                users.sort_unstable();
                let parts: Vec<_> = keys.scope.split('/').collect();
                parts.len() != 4 || parts[0].is_empty() || parts[1] != "dm" || parts[2..] != users
            }
        {
            return Err("Whisper requires a verified two-person private scope.".into());
        }
        let cipher = CallCipher::new(keys)?;
        self.shared
            .controls
            .whisper_ciphers
            .lock()
            .map_err(|_| "Whisper state unavailable")?
            .insert(peer.into(), cipher);
        Ok(())
    }
    pub fn whisper_targets(&self) -> Vec<String> {
        self.shared
            .controls
            .whisper_target
            .lock()
            .map(|targets| targets.clone())
            .unwrap_or_default()
    }
    pub fn select_whisper_group(&self, peers: Vec<String>) {
        let peers: Vec<_> = peers
            .into_iter()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .take(MAX_SPEAKERS)
            .collect();
        for peer in &peers {
            self.invalidate_whisper(peer);
        }
        if let Ok(mut requested) = self.shared.controls.whisper_requested.lock() {
            for peer in &peers {
                requested.remove(peer);
            }
        }
        if let Ok(mut target) = self.shared.controls.whisper_target.lock() {
            *target = peers;
        }
        self.shared
            .controls
            .privacy_generation
            .fetch_add(1, Ordering::AcqRel);
    }
    pub fn whisper_ready(&self, peer: &str) -> bool {
        self.shared
            .controls
            .whisper_ciphers
            .lock()
            .is_ok_and(|ciphers| ciphers.contains_key(peer))
    }
    pub fn set_priority_users(&self, users: impl IntoIterator<Item = String>) {
        if let Ok(mut saved) = self.shared.controls.priority_users.lock() {
            *saved = users.into_iter().collect();
        }
    }
    pub fn update_ducking(&self, enabled: bool, active: bool, volume: f32) {
        #[cfg(windows)]
        if let Ok(mut slot) = self.ducker.lock() {
            if enabled && slot.is_none() {
                *slot = Some(crate::audio_duck::Ducker::new());
            }
            if let Some(ducker) = slot.as_mut() {
                ducker.set(enabled && active, volume);
            }
        }
        #[cfg(not(windows))]
        let _ = (enabled, active, volume);
    }

    pub fn speaking_users(&self) -> Vec<String> {
        if self.shared.deafened.load(Ordering::Acquire) || !self.is_active() {
            return Vec::new();
        }
        let now = Instant::now();
        self.shared
            .controls
            .speaking
            .lock()
            .map(|peers| {
                peers
                    .iter()
                    .filter(|(_, time)| {
                        now.saturating_duration_since(**time) < Duration::from_millis(250)
                    })
                    .map(|(id, _)| id.clone())
                    .collect()
            })
            .unwrap_or_default()
    }
    /// When non-silent audio last reached the decoder, independent of output volume.
    pub fn received_voice(&self) -> HashMap<String, Instant> {
        self.shared
            .controls
            .heard
            .lock()
            .map(|heard| heard.clone())
            .unwrap_or_default()
    }
    pub fn private_network(&self) -> bool {
        self.shared.controls.private_network.load(Ordering::Acquire)
    }
    pub fn network_stats(&self) -> NetworkStats {
        let mut stats = self.counters.snapshot();
        stats.input_timing = self
            .shared
            .controls
            .input_timing
            .lock()
            .ok()
            .and_then(|info| *info);
        stats.output_timing = self
            .shared
            .controls
            .output_timing
            .lock()
            .ok()
            .and_then(|info| *info);
        stats
    }
    pub fn take_error(&mut self) -> Option<String> {
        self.errors.lock().ok()?.take()
    }
    pub fn take_sound_error(&mut self) -> Option<String> {
        self.shared.sound_errors.lock().ok()?.take()
    }
    fn cancel_sound(&self) {
        if let Ok(stop) = self.shared.sound_cancel.lock() {
            if let Some(stop) = stop.as_ref() {
                stop.store(true, Ordering::Release);
            }
        }
        if let Ok(mut tones) = self.shared.tones.lock() {
            tones.clear();
        }
    }
}
impl Drop for MediaEngine {
    fn drop(&mut self) {
        self.cancel();
        self.cancel_sound();
        if let Some(sender) = self.sender.take() {
            let _ = sender.try_send(AudioCommand::Shutdown);
        }
        // The owner closes its own !Send streams. Slow drivers never block the UI destructor.
        self.owner.take();
    }
}

fn ensure_not_cancelled(stop: &AtomicBool) -> Result<(), String> {
    if stop.load(Ordering::Acquire) {
        Err("Audio request cancelled".into())
    } else {
        Ok(())
    }
}
fn current_generation(state: &Mutex<OwnerState>, generation: u64) -> bool {
    state
        .lock()
        .is_ok_and(|current| current.generation == generation)
}
fn publish_phase(state: &Mutex<OwnerState>, generation: u64, phase: AudioPhase) -> bool {
    let Ok(mut current) = state.lock() else {
        return false;
    };
    if current.generation != generation {
        return false;
    }
    current.phase = phase;
    true
}

// Construct the backend on the owner thread; B intentionally has no Send requirement.
trait AudioBackend {
    fn voice(&mut self, request: VoiceStart) -> Result<(), String>;
    fn reopen_devices(
        &mut self,
        settings: AudioSettings,
        revision: u64,
    ) -> Result<DeviceReopen, String>;
    fn stop_voice(&mut self);
    fn microphone(&mut self, request: MicrophoneStart) -> Result<(), String>;
    fn stop_microphone(&mut self);
    fn tone(&mut self, cue: AudioCue, settings: AudioSettings, preview: bool)
        -> Result<(), String>;
    fn stop_sound(&mut self);
    fn cleanup_cancelled(&mut self);
    fn has_devices(&self) -> bool;
}

fn audio_owner<B: AudioBackend>(
    receiver: Receiver<AudioCommand>,
    state: Arc<Mutex<OwnerState>>,
    pending_devices: Arc<Mutex<Option<DeviceReopenRequest>>>,
    create: impl FnOnce() -> B,
) {
    let mut backend = create();
    loop {
        backend.cleanup_cancelled();
        let may_reopen = state.lock().is_ok_and(|s| {
            matches!(
                s.phase,
                AudioPhase::ActiveVoice | AudioPhase::ChangingDevices
            )
        });
        let request = may_reopen
            .then(|| pending_devices.lock().ok().and_then(|mut slot| slot.take()))
            .flatten();
        if let Some(request) = request {
            let current = || {
                !request.stop.load(Ordering::Acquire)
                    && current_generation(&state, request.generation)
                    && request.controls.device_revision.load(Ordering::Acquire) == request.revision
            };
            if current() {
                publish_phase(&state, request.generation, AudioPhase::ChangingDevices);
                let result = backend.reopen_devices(request.settings.clone(), request.revision);
                if current() {
                    match result {
                        Ok(DeviceReopen::Applied) => {
                            publish_phase(&state, request.generation, AudioPhase::ActiveVoice);
                        }
                        Ok(DeviceReopen::Retained { settings, error }) => {
                            if let Ok(mut failure) = request.failures.lock() {
                                *failure = Some(DeviceChangeFailure {
                                    generation: request.generation,
                                    revision: request.revision,
                                    attempted: request.settings,
                                    restored: *settings,
                                    error,
                                    follow: request.follow,
                                });
                            }
                            publish_phase(&state, request.generation, AudioPhase::ActiveVoice);
                        }
                        Ok(DeviceReopen::Superseded) => {}
                        Err(error) => {
                            backend.stop_voice();
                            publish_phase(&state, request.generation, AudioPhase::Idle);
                            record_error(&request.errors, error);
                        }
                    }
                }
            }
        }
        let received = if backend.has_devices() {
            receiver.recv_timeout(Duration::from_millis(20))
        } else {
            receiver
                .recv()
                .map_err(|_| mpsc::RecvTimeoutError::Disconnected)
        };
        let command = match received {
            Ok(command) => command,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        match command {
            AudioCommand::Voice {
                generation,
                request,
            } => {
                if !current_generation(&state, generation) || request.stop.load(Ordering::Acquire) {
                    continue;
                }
                let stop = request.stop.clone();
                let errors = request.errors.clone();
                let result = backend.voice(request);
                if stop.load(Ordering::Acquire) || !current_generation(&state, generation) {
                    backend.stop_voice();
                    continue;
                }
                match result {
                    Ok(()) => {
                        publish_phase(&state, generation, AudioPhase::ActiveVoice);
                    }
                    Err(error) => {
                        publish_phase(&state, generation, AudioPhase::Idle);
                        record_error(&errors, error);
                    }
                }
            }
            AudioCommand::Microphone {
                generation,
                request,
            } => {
                if !current_generation(&state, generation) || request.stop.load(Ordering::Acquire) {
                    continue;
                }
                let stop = request.stop.clone();
                let errors = request.errors.clone();
                let result = backend.microphone(request);
                if stop.load(Ordering::Acquire) || !current_generation(&state, generation) {
                    backend.stop_microphone();
                    continue;
                }
                match result {
                    Ok(()) => {
                        publish_phase(&state, generation, AudioPhase::ActiveMicrophone);
                    }
                    Err(error) => {
                        publish_phase(&state, generation, AudioPhase::Idle);
                        record_error(&errors, error);
                    }
                }
            }
            AudioCommand::StopVoice => backend.stop_voice(),
            AudioCommand::StopMicrophone => backend.stop_microphone(),
            AudioCommand::CallTone {
                cue,
                settings,
                epoch,
                controls,
                deafened,
                errors,
            } => {
                // The audio owner validates the token too: a queued ring cannot
                // start after the call was answered or the account was replaced.
                if controls.call_tone_epoch.load(Ordering::Acquire) == epoch
                    && !deafened.load(Ordering::Acquire)
                {
                    if let Err(error) = backend.tone(cue, settings, false) {
                        record_error(&errors, error);
                    }
                    if controls.call_tone_epoch.load(Ordering::Acquire) != epoch
                        || deafened.load(Ordering::Acquire)
                    {
                        controls
                            .call_tones
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .clear();
                    }
                }
            }
            AudioCommand::Tone {
                generation,
                cue,
                settings,
                preview,
                errors,
            } => {
                if current_generation(&state, generation) {
                    if let Err(error) = backend.tone(cue, settings, preview) {
                        record_error(&errors, error);
                    }
                }
            }
            AudioCommand::StopSound => backend.stop_sound(),
            AudioCommand::Shutdown => break,
        }
    }
}

enum DeviceStream {
    Cpal(cpal::Stream),
    #[cfg(windows)]
    Windows(crate::windows_audio::Stream),
}
impl DeviceStream {
    fn play(&self) -> Result<(), String> {
        match self {
            Self::Cpal(stream) => stream.play().map_err(|e| e.to_string()),
            #[cfg(windows)]
            Self::Windows(stream) => stream.play(),
        }
    }
}

struct NativeAudio {
    session: Option<VoiceSession>,
    muted: Arc<AtomicBool>,
    deafened: Arc<AtomicBool>,
    level: Arc<AtomicU32>,
    counters: Arc<Counters>,
    errors: ErrorSlot,
    settings: AudioSettings,
    controls: Arc<AudioControls>,
    tones: ToneQueue,
    sound_worker: Option<JoinHandle<()>>,
    sound_preview: bool,
    sound_stop: Arc<AtomicBool>,
    sound_errors: ErrorSlot,
    sound_cancel: Arc<Mutex<Option<Arc<AtomicBool>>>>,
    microphone_test: Option<microphone_preview::Session>,
}

struct VoiceSession {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    input: Option<DeviceStream>,
    output: Option<DeviceStream>,
    capture: SampleQueue,
    playback: SampleQueue,
    playback_limit: Arc<AtomicUsize>,
}

impl NativeAudio {
    fn with_shared(shared: AudioShared) -> Self {
        let mut audio = Self::new().expect("construct device-free native audio state");
        audio.muted = shared.muted;
        audio.deafened = shared.deafened;
        audio.level = shared.level;
        audio.controls = shared.controls;
        audio.tones = shared.tones;
        audio.sound_errors = shared.sound_errors;
        audio.sound_cancel = shared.sound_cancel;
        audio
    }
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            session: None,
            muted: Arc::new(AtomicBool::new(false)),
            deafened: Arc::new(AtomicBool::new(false)),
            level: Arc::new(AtomicU32::new(0)),
            counters: Arc::new(Counters::default()),
            errors: Arc::new(Mutex::new(None)),
            settings: AudioSettings::default(),
            controls: Arc::new(AudioControls::new(&AudioSettings::default())),
            tones: Arc::new(Mutex::new(VecDeque::new())),
            sound_worker: None,
            sound_preview: false,
            sound_stop: Arc::new(AtomicBool::new(false)),
            sound_errors: Arc::new(Mutex::new(None)),
            sound_cancel: Arc::new(Mutex::new(None)),
            microphone_test: None,
        })
    }

    fn start_voice(&mut self, request: VoiceStart) -> Result<(), String> {
        let VoiceStart {
            mut host,
            mut ticket,
            stop,
            capture,
            playback,
            errors,
            counters,
            settings,
            call_keys,
            gateway,
        } = request;
        self.stop_voice();
        let private = crate::embedded_network::routed(&host);
        self.controls.set_voice_connection(&host, private);
        if private {
            let (gateway, port) = crate::embedded_network::new_voice_route(&host)?;
            host = gateway;
            ticket.udp_port = port;
            self.controls.direct_enabled.store(false, Ordering::Release);
        }
        let host = host.as_str();
        self.stop_microphone_test();
        self.stop_sound_output();
        self.errors = errors;
        self.counters = counters;
        self.settings = settings;
        let call = CallCipher::new(call_keys)?;
        ensure_not_cancelled(&stop)?;
        let key = STANDARD
            .decode(&ticket.key)
            .map_err(|_| "Voice ticket has an invalid key".to_string())?;
        if key.len() != 32 {
            return Err("Voice ticket key must contain 32 bytes".into());
        }
        let cipher =
            ChaCha20Poly1305::new_from_slice(&key).map_err(|_| "Invalid voice key".to_string())?;
        let remote = resolve_voice_host(host, ticket.udp_port)?;
        ensure_not_cancelled(&stop)?;
        let socket = UdpSocket::bind(if remote.is_ipv6() {
            "[::]:0"
        } else {
            "0.0.0.0:0"
        })
        .map_err(|e| format!("Open voice socket: {e}"))?;
        // One UDP binding serves authenticated relay traffic and opt-in direct peers.
        socket
            .set_nonblocking(true)
            .map_err(|e| format!("Configure voice socket: {e}"))?;
        let encoder = crate::audio_settings::make_voice_encoder_with_recovery(
            self.settings.bitrate_kbps,
            self.settings.loss_recovery_enabled(),
        )
        .map_err(|e| format!("Initialize Opus: {e}"))?;
        ensure_not_cancelled(&stop)?;
        if let Ok(mut error) = self.errors.lock() {
            *error = None;
        }
        let playback_limit = Arc::new(AtomicUsize::new(MAX_PLAYBACK_SAMPLES));
        self.retire_device_errors()?;
        let (input, output) = self.open_voice_devices(
            &self.settings,
            &stop,
            &capture,
            &playback,
            &playback_limit,
            None,
        )?;
        if let Ok(mut config) = self.controls.tunnel_config.lock() {
            *config = ticket.tunnel_url.as_ref().and_then(|address| {
                let mut url = url::Url::parse(address).ok()?;
                let port = url.port_or_known_default()?;
                let scheme = if url.scheme() == "https" {
                    "wss"
                } else if url.scheme() == "http" {
                    "ws"
                } else {
                    return None;
                };
                url.set_scheme(scheme).ok()?;
                let address = if private {
                    let mapped = crate::embedded_network::endpoint(url.as_str());
                    let endpoint = url::Url::parse(&mapped).ok()?;
                    if endpoint.scheme() != "ws" || endpoint.host_str() != Some("127.0.0.1") {
                        return None;
                    }
                    let port = endpoint.port().filter(|port| *port != 0)?;
                    SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), port)
                } else {
                    SocketAddr::new(remote.ip(), port)
                };
                Some(crate::voice_tunnel::Config {
                    url: url.to_string(),
                    address,
                    session: ticket.session_id,
                    key: ticket.key.clone(),
                })
            });
        }
        let gateway = gateway.and_then(|config| crate::gateway_control::Session::start(
                config, remote, self.controls.gateway_enabled.clone(), stop.clone(),
            ));
        self.counters.gateway_available.store(gateway.is_some(), Ordering::Release);
        let args = VoiceWorker {
            gateway,
            playback_limit: playback_limit.clone(),
            socket,
            remote,
            session_id: ticket.session_id,
            wire_version: ticket.wire_version,
            cipher,
            call,
            encoder,
            capture: capture.clone(),
            playback: playback.clone(),
            stop: stop.clone(),
            muted: self.muted.clone(),
            deafened: self.deafened.clone(),
            counters: self.counters.clone(),
            errors: self.errors.clone(),
            controls: self.controls.clone(),
            level: self.level.clone(),
        };
        let worker = thread::Builder::new()
            .name("relay-voice".into())
            .spawn(move || {
                let _audio_priority = crate::thread_priority::AudioThreadGuard::new("Audio");
                let _power = crate::thread_priority::CallPowerGuard::new();
                let _qos = crate::voice_qos::VoiceQos::new(
                    &args.socket,
                    args.remote,
                    args.controls.voice_qos.load(Ordering::Acquire),
                );
                voice_worker(args)
            })
            .map_err(|e| format!("Start voice worker: {e}"))?;
        self.session = Some(VoiceSession {
            stop,
            worker: Some(worker),
            input: Some(input),
            output: Some(output),
            capture,
            playback,
            playback_limit,
        });
        Ok(())
    }

    fn retire_device_errors(&self) -> Result<(), String> {
        *self
            .controls
            .device_errors
            .lock()
            .map_err(|_| "Audio error state stopped.")? =
            Some((Arc::new(Mutex::new(None)), self.errors.clone()));
        Ok(())
    }

    fn open_voice_devices(
        &self,
        settings: &AudioSettings,
        stop: &Arc<AtomicBool>,
        capture: &SampleQueue,
        playback: &SampleQueue,
        playback_limit: &Arc<AtomicUsize>,
        revision: Option<u64>,
    ) -> Result<(DeviceStream, DeviceStream), String> {
        let current = || -> Result<(), String> {
            ensure_not_cancelled(stop)?;
            if revision.is_some_and(|r| self.controls.device_revision.load(Ordering::Acquire) != r)
            {
                return Err("Audio device selection changed.".into());
            }
            Ok(())
        };
        current()?;
        let host = cpal::default_host();
        let input_device = audio_device(&host, &settings.input_device, true)?;
        current()?;
        let output_device = audio_device(&host, &settings.output_device, false)?;
        current()?;
        let input_config = input_device
            .default_input_config()
            .map_err(|e| format!("Microphone format: {e}"))?;
        current()?;
        let output_config = output_device
            .default_output_config()
            .map_err(|e| format!("Playback format: {e}"))?;
        current()?;
        self.controls
            .low_latency_device
            .store(settings.low_latency_device, Ordering::Release);
        let errors: ErrorSlot = Arc::new(Mutex::new(None));
        let input = make_input(
            &input_device,
            &input_config,
            InputState {
                queue: capture.clone(),
                level: self.level.clone(),
                muted: self.muted.clone(),
                deafened: self.deafened.clone(),
                counters: self.counters.clone(),
                errors: errors.clone(),
                controls: self.controls.clone(),
                stop: stop.clone(),
            },
        )?;
        current()?;
        let output = make_output(
            &output_device,
            &output_config,
            OutputState {
                playback_limit: playback_limit.clone(),
                counters: self.counters.clone(),
                queue: playback.clone(),
                deafened: self.deafened.clone(),
                errors: errors.clone(),
                controls: self.controls.clone(),
                tones: self.tones.clone(),
                stop: stop.clone(),
            },
        )?;
        current()?;
        output
            .play()
            .map_err(|e| format!("Start audio output: {e}"))?;
        current()?;
        input
            .play()
            .map_err(|e| format!("Start microphone (check device privacy permissions): {e}"))?;
        // Activate this pair atomically with its error check. An error from a
        // discarded/retired stream cannot stop the still-running voice worker.
        let mut active = self
            .controls
            .device_errors
            .lock()
            .map_err(|_| "Audio error state stopped.")?;
        current()?;
        if let Some(error) = errors.lock().map_err(|_| "Audio device stopped.")?.take() {
            return Err(error);
        }
        *active = Some((errors, self.errors.clone()));
        Ok((input, output))
    }

    fn change_voice_devices(
        &mut self,
        settings: AudioSettings,
        revision: u64,
    ) -> Result<DeviceReopen, String> {
        let Some(session) = self.session.as_ref() else {
            return Err("There is no active voice connection for this device change.".into());
        };
        let (stop, capture, playback, limit) = (
            session.stop.clone(),
            session.capture.clone(),
            session.playback.clone(),
            session.playback_limit.clone(),
        );
        let previous = self.settings.clone();
        let controls = self.controls.clone();
        let current = || {
            !stop.load(Ordering::Acquire)
                && controls.device_revision.load(Ordering::Acquire) == revision
        };
        if !current() {
            return Ok(DeviceReopen::Superseded);
        }
        self.retire_device_errors()?;
        // Close old callbacks before constructing new queue endpoints. Only the
        // device streams change: socket, packet sequence, keys and worker stay.
        if let Some(session) = self.session.as_mut() {
            session.input.take();
            session.output.take();
        }
        if let Ok(mut queue) = capture.lock() {
            queue.clear();
        }
        if let Ok(mut queue) = playback.lock() {
            queue.clear();
        }
        self.controls
            .privacy_generation
            .fetch_add(1, Ordering::AcqRel);
        #[cfg(feature = "native-rtc")]
        self.controls.echo_reference.clear();
        match self.open_voice_devices(
            &settings,
            &stop,
            &capture,
            &playback,
            &limit,
            Some(revision),
        ) {
            Ok((input, output)) => {
                if let Some(session) = self.session.as_mut() {
                    session.input = Some(input);
                    session.output = Some(output);
                }
                self.settings = settings;
                Ok(DeviceReopen::Applied)
            }
            Err(error) => {
                if !current() {
                    return Ok(DeviceReopen::Superseded);
                }
                // A missing/busy new device need not end a conversation. Retry
                // the last working selection once, under the same call/session.
                if let Ok(mut queue) = capture.lock() {
                    queue.clear();
                }
                if let Ok(mut queue) = playback.lock() {
                    queue.clear();
                }
                self.controls
                    .privacy_generation
                    .fetch_add(1, Ordering::AcqRel);
                match self.open_voice_devices(&previous, &stop, &capture, &playback, &limit, Some(revision)) {
                    Ok((input, output)) => {
                        if let Some(session) = self.session.as_mut() {
                            session.input = Some(input); session.output = Some(output);
                        }
                        Ok(DeviceReopen::Retained { settings: Box::new(previous), error })
                    }
                    Err(restore) if current() => Err(format!("Could not switch audio devices or reopen the previous selection. Choose an available input/output in Voice & Video and retry the call. Details: {error}; {restore}")),
                    Err(_) => Ok(DeviceReopen::Superseded),
                }
            }
        }
    }

    pub fn stop_voice(&mut self) {
        self.controls.automatic_tcp.store(false, Ordering::Release);
        if let Some(mut session) = self.session.take() {
            session.stop.store(true, Ordering::Release);
            self.controls.wake.notify();
            // Dropping both streams closes native devices before waiting on the worker.
            session.input.take();
            session.output.take();
            if let Some(worker) = session.worker.take() {
                let _ = worker.join();
            }
        }
        if let Ok(mut devices) = self.controls.device_errors.lock() {
            devices.take();
        }
        if let Ok(mut tunnel) = self.controls.tunnel.lock() {
            tunnel.take();
        }
        if let Ok(mut ciphers) = self.controls.whisper_ciphers.lock() {
            ciphers.clear();
        }
        if let Ok(mut requested) = self.controls.whisper_requested.lock() {
            requested.clear();
        }
        if let Ok(mut target) = self.controls.whisper_target.lock() {
            target.clear();
        }
        self.level.store(0, Ordering::Relaxed);
        self.controls.transmitting.store(false, Ordering::Relaxed);
        #[cfg(feature = "native-rtc")]
        self.controls.echo_reference.clear();
    }

    /// Explicit local processed preview. Its devices, queues and DSP are isolated from calls.
    fn start_microphone_test(&mut self, request: MicrophoneStart) -> Result<(), String> {
        if self.is_active() {
            return Err("Leave voice before starting a microphone test.".into());
        }
        self.stop_microphone_test();
        self.microphone_test = Some(microphone_preview::Session::start(request, self.level.clone())?);
        Ok(())
    }
    pub fn stop_microphone_test(&mut self) {
        self.microphone_test.take();
        if !self.is_active() {
            self.level.store(0, Ordering::Relaxed);
        }
    }
    /// Cue playback opens only an output stream, and only when explicitly requested.
    fn play_tone(
        &mut self,
        cue: AudioCue,
        settings: AudioSettings,
        preview: bool,
    ) -> Result<(), String> {
        if !crate::sound_customization::audible(&settings, cue) {
            return Ok(());
        }
        if preview || (self.sound_preview && !self.is_active()) {
            self.stop_sound_output();
        }
        // A preview has its own queue and output controls, even during a call.
        // Normal cues continue to use the call's existing output mixer.
        let tones = if preview {
            Arc::new(Mutex::new(VecDeque::new()))
        } else {
            self.tones.clone()
        };
        let queued = if !preview && matches!(cue, AudioCue::Ring | AudioCue::Ringback) {
            self.controls.call_tones.clone()
        } else {
            tones.clone()
        };
        let samples = crate::sound_customization::samples(cue, &settings);
        if let Ok(mut tones) = queued.lock() {
            append_tone(&mut tones, samples);
        }
        if !preview
            && (self.is_active()
                || self
                    .sound_worker
                    .as_ref()
                    .is_some_and(|worker| !worker.is_finished()))
        {
            return Ok(());
        }
        if let Some(worker) = self.sound_worker.take() {
            let _ = worker.join();
        }
        self.sound_preview = preview;
        self.sound_stop = Arc::new(AtomicBool::new(false));
        if let Ok(mut stop) = self.sound_cancel.lock() {
            *stop = Some(self.sound_stop.clone());
        }
        let stop = self.sound_stop.clone();
        let errors = self.sound_errors.clone();
        let controls = if preview {
            Arc::new(AudioControls::new(&settings))
        } else {
            self.controls.clone()
        };
        let selected = settings.output_device;
        self.sound_worker = Some(
            thread::Builder::new()
                .name("relay-cue".into())
                .spawn(move || {
                    let play = || -> Result<(), String> {
                        ensure_not_cancelled(&stop)?;
                        let host = cpal::default_host();
                        let device = audio_device(&host, &selected, false)?;
                        ensure_not_cancelled(&stop)?;
                        let config = device
                            .default_output_config()
                            .map_err(|e| format!("Cue playback format: {e}"))?;
                        ensure_not_cancelled(&stop)?;
                        let call_queue = controls.call_tones.clone();
                        let stream = make_output(
                            &device,
                            &config,
                            OutputState {
                                playback_limit: Arc::new(AtomicUsize::new(MAX_PLAYBACK_SAMPLES)),
                                counters: Arc::new(Counters::default()),
                                queue: sample_queue(VecDeque::new()),
                                deafened: Arc::new(AtomicBool::new(false)),
                                errors: errors.clone(),
                                controls,
                                tones: tones.clone(),
                                stop: stop.clone(),
                            },
                        )?;
                        ensure_not_cancelled(&stop)?;
                        stream.play().map_err(|e| format!("Play cue: {e}"))?;
                        ensure_not_cancelled(&stop)?;
                        let began = Instant::now();
                        let mut last_progress = began;
                        let mut previous_queued = usize::MAX;
                        while !stop.load(Ordering::Acquire) {
                            let queued_samples = tones.lock().map(|q| q.len()).unwrap_or(0)
                                + call_queue.lock().map(|q| q.len()).unwrap_or(0);
                            if queued_samples != previous_queued {
                                last_progress = Instant::now();
                                previous_queued = queued_samples;
                            }
                            if began.elapsed() > Duration::from_millis(200)
                                && queued_samples == 0
                            {
                                break;
                            }
                            // A new event can arrive while output is already
                            // playing. Let it finish; only a stalled device is
                            // subject to a fixed watchdog.
                            if last_progress.elapsed() > Duration::from_secs(crate::sound_customization::MAX_SECONDS as u64 + 1) {
                                break;
                            }
                            thread::sleep(Duration::from_millis(10));
                        }
                        Ok(())
                    };
                    if let Err(error) = play() {
                        if let Ok(mut queued) = queued.lock() {
                            queued.clear();
                        }
                        if !stop.load(Ordering::Acquire) {
                            record_error(&errors, error);
                        }
                    }
                })
                .map_err(|e| format!("Start cue playback: {e}"))?,
        );
        Ok(())
    }
    fn stop_sound_output(&mut self) {
        self.sound_preview = false;
        self.sound_stop.store(true, Ordering::Release);
        if let Some(worker) = self.sound_worker.take() {
            let _ = worker.join();
        }
        if let Ok(mut tones) = self.tones.lock() {
            tones.clear();
        }
    }
    pub fn is_active(&self) -> bool {
        self.session.is_some()
    }
}
impl Drop for NativeAudio {
    fn drop(&mut self) {
        self.stop_voice();
        self.stop_microphone_test();
        self.stop_sound_output();
    }
}

impl AudioBackend for NativeAudio {
    fn voice(&mut self, request: VoiceStart) -> Result<(), String> {
        self.start_voice(request)
    }
    fn reopen_devices(
        &mut self,
        settings: AudioSettings,
        revision: u64,
    ) -> Result<DeviceReopen, String> {
        self.change_voice_devices(settings, revision)
    }
    fn stop_voice(&mut self) {
        NativeAudio::stop_voice(self);
    }
    fn microphone(&mut self, request: MicrophoneStart) -> Result<(), String> {
        self.start_microphone_test(request)
    }
    fn stop_microphone(&mut self) {
        self.stop_microphone_test();
    }
    fn tone(
        &mut self,
        cue: AudioCue,
        settings: AudioSettings,
        preview: bool,
    ) -> Result<(), String> {
        self.play_tone(cue, settings, preview)
    }
    fn stop_sound(&mut self) {
        self.stop_sound_output();
    }
    fn cleanup_cancelled(&mut self) {
        if self
            .session
            .as_ref()
            .is_some_and(|session| session.stop.load(Ordering::Acquire))
        {
            self.stop_voice();
        }
        if self
            .microphone_test
            .as_ref()
            .is_some_and(microphone_preview::Session::stopped)
        {
            self.stop_microphone_test();
        }
        if self
            .sound_worker
            .as_ref()
            .is_some_and(|worker| worker.is_finished())
        {
            if let Some(worker) = self.sound_worker.take() {
                let _ = worker.join();
            }
        }
    }
    fn has_devices(&self) -> bool {
        self.session.is_some() || self.microphone_test.is_some() || self.sound_worker.is_some()
    }
}

#[cfg(test)]
fn tone_samples(cue: AudioCue, volume: f32) -> Vec<f32> {
    crate::sound_cues::samples(cue, volume)
}

fn append_tone(queue: &mut VecDeque<f32>, mut samples: Vec<f32>) {
    samples.truncate(MAX_TONE_SAMPLES);
    let excess = (queue.len() + samples.len()).saturating_sub(MAX_TONE_SAMPLES);
    queue.drain(..excess.min(queue.len()));
    queue.extend(samples);
}

fn record_error(errors: &ErrorSlot, message: String) {
    if let Ok(mut error) = errors.lock() {
        *error = Some(message);
    }
}

/// Stop the failed operation at the device callback, without waiting for a UI poll.
/// Late callbacks from a stopped operation must not overwrite its first error.
fn record_device_error(
    errors: &ErrorSlot,
    stop: &AtomicBool,
    controls: &AudioControls,
    input: bool,
    detail: impl std::fmt::Display,
) {
    if stop.load(Ordering::Acquire) {
        return;
    }
    let detail = detail.to_string();
    if controls.follow_devices.load(Ordering::Acquire) && device_gone(&detail) {
        // The app reopens on the Windows default (and back when replugged).
        controls.device_lost.store(true, Ordering::Release);
        controls.wake.notify();
        return;
    }
    let message = format!(
        "{} stopped. Reconnect it or choose an {} in Voice & Video, then try again. Details: {detail}",
        if input { "Microphone" } else { "Audio output" },
        if input { "input" } else { "output" },
    );
    let Ok(active) = controls.device_errors.lock() else {
        return;
    };
    if active
        .as_ref()
        .is_some_and(|(source, _)| !Arc::ptr_eq(source, errors))
    {
        // Keep a candidate's error for startup/rollback, without cancelling a
        // different device pair's voice worker or changing its failure channel.
        record_error(errors, message);
        return;
    }
    if stop.swap(true, Ordering::AcqRel) {
        return;
    }
    record_error(
        active
            .as_ref()
            .map_or(errors, |(_, destination)| destination),
        message,
    );
    controls.wake.notify();
}

/// cpal's DeviceNotAvailable, or WASAPI's AUDCLNT_E_DEVICE_INVALIDATED.
fn device_gone(detail: &str) -> bool {
    let detail = detail.to_ascii_lowercase();
    detail.contains("no longer available")
        || detail.contains("0x88890004")
        || detail.contains("device_invalidated")
}

fn resolve_voice_host(host: &str, port: u16) -> Result<SocketAddr, String> {
    let authority = host
        .trim()
        .split_once("://")
        .map_or(host.trim(), |(_, v)| v)
        .split('/')
        .next()
        .unwrap_or("");
    if authority.contains('@') {
        return Err("Voice server address must not include credentials".into());
    }
    let name = if authority.starts_with('[') {
        authority
            .split(']')
            .next()
            .unwrap_or("")
            .trim_start_matches('[')
    } else if authority.matches(':').count() == 1 {
        authority.split(':').next().unwrap_or("")
    } else {
        authority
    };
    (name, port)
        .to_socket_addrs()
        .map_err(|e| format!("Resolve voice server: {e}"))?
        .next()
        .ok_or("Voice server address was empty".into())
}

#[derive(Clone)]
struct InputState {
    queue: SampleQueue,
    level: Arc<AtomicU32>,
    muted: Arc<AtomicBool>,
    deafened: Arc<AtomicBool>,
    counters: Arc<Counters>,
    errors: ErrorSlot,
    controls: Arc<AudioControls>,
    stop: Arc<AtomicBool>,
}

fn preferred_device_buffer(
    sample_rate: u32,
    supported: &cpal::SupportedBufferSize,
) -> cpal::BufferSize {
    match *supported {
        cpal::SupportedBufferSize::Range { min, max } if max >= min && max > 0 => {
            // Ask for 5 ms; hardware may have a larger documented minimum.
            cpal::BufferSize::Fixed((sample_rate / 200).max(1).clamp(min.max(1), max))
        }
        _ => cpal::BufferSize::Default,
    }
}

fn capture_sample_limit(callback_frames: usize, sample_rate: u32) -> usize {
    callback_frames
        .saturating_mul(AUDIO_SAMPLE_RATE as usize)
        .checked_div(sample_rate as usize)
        .unwrap_or(CAPTURE_SAMPLES)
        .clamp(CAPTURE_SAMPLES, MAX_CAPTURE_SAMPLES)
}

fn build_device_stream<T>(
    supported: &cpal::SupportedStreamConfig,
    mut build: impl FnMut(&cpal::StreamConfig) -> Result<T, String>,
) -> Result<T, String> {
    let mut config = supported.config();
    config.buffer_size = preferred_device_buffer(config.sample_rate.0, supported.buffer_size());
    match build(&config) {
        Err(_) if config.buffer_size != cpal::BufferSize::Default => {
            // Some drivers report a range but reject a smaller shared-mode buffer.
            // Retry the system default before reporting a device failure.
            config.buffer_size = cpal::BufferSize::Default;
            build(&config)
        }
        result => result,
    }
}

trait PlaybackQueue {
    fn append_frame(&mut self, frame: &[f32], limit: usize) -> bool;
}
impl PlaybackQueue for VecDeque<f32> {
    fn append_frame(&mut self, frame: &[f32], limit: usize) -> bool {
        let excess = (self.len() + frame.len()).saturating_sub(limit);
        if excess > 0 {
            self.drain(..excess);
        }
        self.extend(frame);
        excess > 0
    }
}
impl PlaybackQueue for crate::device_queue::Guard<'_> {
    fn append_frame(&mut self, frame: &[f32], limit: usize) -> bool {
        self.append_bounded(frame, limit)
    }
}
fn append_playback_frame(output: &mut impl PlaybackQueue, frame: &[f32], limit: usize) -> bool {
    output.append_frame(
        frame,
        limit.clamp(MAX_PLAYBACK_SAMPLES, MAX_DEVICE_PLAYBACK_SAMPLES),
    )
}

fn make_input(
    device: &cpal::Device,
    config: &cpal::SupportedStreamConfig,
    state: InputState,
) -> Result<DeviceStream, String> {
    if let Ok(mut timing) = state.controls.input_timing.lock() {
        *timing = None;
    }
    #[cfg(windows)]
    if state.controls.low_latency_device.load(Ordering::Relaxed)
        && config.sample_format() == cpal::SampleFormat::F32
    {
        if let Ok(name) = device.name() {
            let errors = state.errors.clone();
            let error_stop = state.stop.clone();
            let error_controls = state.controls.clone();
            let native = crate::windows_audio::Stream::input(
                name,
                config.sample_rate().0,
                config.channels(),
                input_callback::<f32>(&config.config(), state.clone()),
                move |error| {
                    record_device_error(&errors, &error_stop, &error_controls, true, error);
                },
            );
            if let Ok(stream) = native {
                if let Ok(mut timing) = state.controls.input_timing.lock() {
                    *timing = Some(device_timing(stream.info));
                }
                eprintln!("WASAPI capture: {:.3} ms period (device minimum {:.3} ms, step {:.3} ms), default {} frames, buffer {} frames, driver latency {:.3} ms, {} Hz, {} channels", stream.info.period_ms(), stream.info.frames_ms(stream.info.min_frames), stream.info.frames_ms(stream.info.fundamental_frames), stream.info.default_frames, stream.info.buffer_frames, stream.info.stream_latency_ms(), stream.info.sample_rate, stream.info.channels);
                return Ok(DeviceStream::Windows(stream));
            }
        }
    }
    macro_rules! build {
        ($t:ty) => {
            build_device_stream(config, |stream_config| {
                input_stream::<$t>(device, stream_config, state.clone()).map(DeviceStream::Cpal)
            })
        };
    }
    match config.sample_format() {
        cpal::SampleFormat::F32 => build!(f32),
        cpal::SampleFormat::I16 => build!(i16),
        cpal::SampleFormat::U16 => build!(u16),
        cpal::SampleFormat::I32 => build!(i32),
        cpal::SampleFormat::U32 => build!(u32),
        cpal::SampleFormat::F64 => build!(f64),
        cpal::SampleFormat::I8 => build!(i8),
        cpal::SampleFormat::U8 => build!(u8),
        format => Err(format!("Unsupported microphone sample format: {format:?}")),
    }
}

fn input_callback<T>(config: &cpal::StreamConfig, state: InputState) -> impl FnMut(&[T]) + Send
where
    T: cpal::SizedSample + 'static,
    f32: cpal::FromSample<T>,
{
    let InputState {
        queue,
        level,
        muted,
        deafened,
        counters,
        errors: _,
        controls,
        stop,
    } = state;
    let mut producer = queue.producer();
    let channels = config.channels as usize;
    let mut resampler = InputResampler::new(config.sample_rate.0);
    let sample_rate = config.sample_rate.0;
    move |data: &[T]| {
        if stop.load(Ordering::Acquire)
            || muted.load(Ordering::Acquire)
            || deafened.load(Ordering::Acquire)
        {
            producer.request_clear();
            level.store(0, Ordering::Relaxed);
            resampler.reset();
            return;
        }
        let mut power = 0.0;
        let mut count = 0;
        let mut dropped_samples = 0usize;
        // Preserve a complete larger hardware callback after fallback.
        let capture_limit = capture_sample_limit(data.len() / channels, sample_rate);
        for frame in data.chunks_exact(channels) {
            let mono = frame
                .iter()
                .map(|v| <f32 as cpal::Sample>::from_sample(*v))
                .sum::<f32>()
                / channels as f32;
            let mono = if mono.is_finite() {
                mono.clamp(-1.0, 1.0)
            } else {
                0.0
            };
            power += mono * mono;
            count += 1;
            dropped_samples +=
                resampler.feed_into(mono, |value| producer.push_bounded(value, capture_limit));
        }
        if dropped_samples > 0 {
            counters.capture_drop.fetch_add(
                dropped_samples.div_ceil(AUDIO_FRAME_SAMPLES) as u64,
                Ordering::Relaxed,
            );
        }
        let rms = if count > 0 {
            (power / count as f32).sqrt()
        } else {
            0.0
        };
        let gain = f32::from_bits(controls.gain.load(Ordering::Relaxed));
        level.store((rms * gain * 3.0).min(1.0).to_bits(), Ordering::Relaxed);
        controls.capture_rms.store(rms.to_bits(), Ordering::Relaxed);
        controls.wake.notify();
    }
}

fn input_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    state: InputState,
) -> Result<cpal::Stream, String>
where
    T: cpal::SizedSample + 'static,
    f32: cpal::FromSample<T>,
{
    let errors = state.errors.clone();
    let error_stop = state.stop.clone();
    let error_controls = state.controls.clone();
    let mut callback = input_callback::<T>(config, state);
    device
        .build_input_stream(
            config,
            move |data, _| callback(data),
            move |e| record_device_error(&errors, &error_stop, &error_controls, true, e),
            None,
        )
        .map_err(|e| format!("Open microphone: {e}"))
}

struct InputResampler {
    previous: Option<f32>,
    phase: f64,
    step: f64,
    filter: Option<crate::audio_resample::SincFilter>,
}
impl InputResampler {
    fn new(rate: u32) -> Self {
        assert!(rate > 0);
        Self {
            previous: None,
            phase: 0.0,
            step: rate as f64 / AUDIO_SAMPLE_RATE as f64,
            // Preserve the exact 48 kHz path. Other device formats need a low-pass
            // filter before fractional interpolation or downsampling.
            filter: (rate != AUDIO_SAMPLE_RATE).then(|| {
                crate::audio_resample::SincFilter::new(
                    (f64::from(AUDIO_SAMPLE_RATE) / f64::from(rate)).min(1.0),
                )
            }),
        }
    }
    fn reset(&mut self) {
        self.previous = None;
        self.phase = 0.0;
        if let Some(filter) = self.filter.as_mut() {
            filter.reset();
        }
    }
    #[cfg(test)]
    fn feed(&mut self, sample: f32, out: &mut VecDeque<f32>) {
        self.feed_bounded(sample, out, usize::MAX);
    }
    #[cfg(any(test, feature = "native-rtc"))]
    fn feed_bounded(&mut self, sample: f32, out: &mut VecDeque<f32>, limit: usize) -> usize {
        self.feed_into(sample, |value| {
            let dropped = out.len() >= limit;
            if dropped {
                out.pop_front();
            }
            out.push_back(value);
            !dropped
        })
    }
    fn feed_into(&mut self, sample: f32, mut emit: impl FnMut(f32) -> bool) -> usize {
        if self.filter.is_none() && self.step == 1.0 {
            // 48 kHz needs no interpolation. Holding the last sample for the
            // next callback would leave every block one sample short until
            // then: a whole extra capture period of delay.
            return usize::from(!emit(sample));
        }
        let mut dropped = 0;
        if let Some(previous) = self.previous {
            if let Some(filter) = self.filter.as_mut() {
                filter.push(previous);
            }
            while self.phase < 1.0 {
                let value = self.filter.as_ref().map_or_else(
                    || previous + (sample - previous) * self.phase as f32,
                    |filter| filter.at(self.phase).clamp(-1.0, 1.0),
                );
                dropped += usize::from(!emit(value));
                self.phase += self.step;
            }
            self.phase -= 1.0;
        }
        self.previous = Some(sample);
        dropped
    }
}

#[derive(Clone)]
struct OutputState {
    playback_limit: Arc<AtomicUsize>,
    counters: Arc<Counters>,
    queue: SampleQueue,
    deafened: Arc<AtomicBool>,
    errors: ErrorSlot,
    controls: Arc<AudioControls>,
    tones: ToneQueue,
    stop: Arc<AtomicBool>,
}

fn make_output(
    device: &cpal::Device,
    config: &cpal::SupportedStreamConfig,
    state: OutputState,
) -> Result<DeviceStream, String> {
    state.controls.output_reserve.store(
        crate::voice_output::DEFAULT_RESERVE_SAMPLES,
        Ordering::Release,
    );
    if let Ok(mut timing) = state.controls.output_timing.lock() {
        *timing = None;
    }
    #[cfg(windows)]
    if state.controls.low_latency_device.load(Ordering::Relaxed)
        && config.sample_format() == cpal::SampleFormat::F32
    {
        if let Ok(name) = device.name() {
            let errors = state.errors.clone();
            let error_stop = state.stop.clone();
            let error_controls = state.controls.clone();
            let native = crate::windows_audio::Stream::output(
                name,
                config.sample_rate().0,
                config.channels(),
                output_callback::<f32>(&config.config(), state.clone()),
                move |error| {
                    record_device_error(&errors, &error_stop, &error_controls, false, error);
                },
            );
            if let Ok(stream) = native {
                if let Ok(mut timing) = state.controls.output_timing.lock() {
                    *timing = Some(device_timing(stream.info));
                }
                state.controls.output_reserve.store(
                    crate::voice_output::reserve_samples(Some(stream.info.period_ms())),
                    Ordering::Release,
                );
                eprintln!("WASAPI output: {:.3} ms period (device minimum {:.3} ms, step {:.3} ms), default {} frames, buffer {} frames, driver latency {:.3} ms, {} Hz, {} channels", stream.info.period_ms(), stream.info.frames_ms(stream.info.min_frames), stream.info.frames_ms(stream.info.fundamental_frames), stream.info.default_frames, stream.info.buffer_frames, stream.info.stream_latency_ms(), stream.info.sample_rate, stream.info.channels);
                return Ok(DeviceStream::Windows(stream));
            }
        }
    }
    macro_rules! build {
        ($t:ty) => {
            build_device_stream(config, |stream_config| {
                output_stream::<$t>(device, stream_config, state.clone()).map(DeviceStream::Cpal)
            })
        };
    }
    match config.sample_format() {
        cpal::SampleFormat::F32 => build!(f32),
        cpal::SampleFormat::I16 => build!(i16),
        cpal::SampleFormat::U16 => build!(u16),
        cpal::SampleFormat::I32 => build!(i32),
        cpal::SampleFormat::U32 => build!(u32),
        cpal::SampleFormat::F64 => build!(f64),
        cpal::SampleFormat::I8 => build!(i8),
        cpal::SampleFormat::U8 => build!(u8),
        format => Err(format!("Unsupported output sample format: {format:?}")),
    }
}

fn output_callback<T>(
    config: &cpal::StreamConfig,
    state: OutputState,
) -> impl FnMut(&mut [T]) + Send
where
    T: cpal::SizedSample + cpal::FromSample<f32> + 'static,
{
    output_callback_observed(config, state, |_, _| {})
}

/// Synthetic integration helper: the actual render callback with an owned
/// application sink, no device, voice, capture, or playback outside the buffer.
#[cfg(all(test, feature = "native-rtc"))]
pub(crate) fn application_audio_test_renderer(
    sink: ApplicationAudioSink,
    channels: u16,
) -> impl FnMut(&mut [f32]) + Send {
    let mut controls = AudioControls::new(&AudioSettings {
        playback_volume: 1.0,
        ..Default::default()
    });
    controls.application_audio = sink;
    let config = cpal::StreamConfig {
        channels,
        sample_rate: cpal::SampleRate(48_000),
        buffer_size: cpal::BufferSize::Default,
    };
    output_callback::<f32>(
        &config,
        OutputState {
            playback_limit: Arc::new(AtomicUsize::new(MAX_PLAYBACK_SAMPLES)),
            counters: Arc::new(Counters::default()),
            queue: sample_queue(VecDeque::new()),
            deafened: Arc::new(AtomicBool::new(false)),
            errors: Arc::new(Mutex::new(None)),
            controls: Arc::new(controls),
            tones: Arc::new(Mutex::new(VecDeque::new())),
            stop: Arc::new(AtomicBool::new(false)),
        },
    )
}

fn output_callback_observed<T>(
    config: &cpal::StreamConfig,
    state: OutputState,
    mut observe_voice: impl FnMut(usize, usize) + Send,
) -> impl FnMut(&mut [T]) + Send
where
    T: cpal::SizedSample + cpal::FromSample<f32> + 'static,
{
    let OutputState {
        queue,
        playback_limit,
        counters,
        deafened,
        errors: _,
        controls,
        tones,
        stop,
    } = state;
    let mut consumer = queue.consumer();
    let channels = config.channels as usize;
    let step = AUDIO_SAMPLE_RATE as f64 / config.sample_rate.0 as f64;
    let mut phase = 1.0;
    let mut voice_output = crate::voice_output::VoiceOutput::new(
        config.sample_rate.0,
        MAX_PLAYBACK_SAMPLES,
        controls.output_reserve.load(Ordering::Acquire),
    );
    let mut voice_seen = false;
    let mut preview_generation = controls.privacy_generation.load(Ordering::Acquire);
    let mut output_limiter = crate::voice_limiter::OutputLimiter::new(config.sample_rate.0);
    let mut previous_tone = 0.0f32;
    let mut next_tone = 0.0f32;
    #[cfg(feature = "native-rtc")]
    let mut application_output = crate::voice_output::PlaybackOutput::<2>::new(
        config.sample_rate.0,
        crate::application_audio::MAX_DEVICE_SAMPLES,
        crate::voice_output::DEFAULT_RESERVE_SAMPLES,
    );
    #[cfg(feature = "native-rtc")]
    let mut application_generation = None;
    #[cfg(feature = "native-rtc")]
    let mut echo_resampler = InputResampler::new(config.sample_rate.0);
    #[cfg(feature = "native-rtc")]
    let mut echo_samples = VecDeque::with_capacity(960);
    move |data: &mut [T]| {
        if stop.load(Ordering::Acquire) {
            voice_output.reset();
            voice_seen = false;
            output_limiter.reset();
            #[cfg(feature = "native-rtc")]
            application_output.reset();
            data.fill(T::from_sample(0.0));
            return;
        }
        let preview_closed = controls.local_preview
            && controls.transmission.load(Ordering::Acquire) == TransmissionMode::PushToTalk as u32
            && !controls.ptt_pressed.load(Ordering::Acquire);
        let muted = deafened.load(Ordering::Relaxed) || preview_closed;
        let volume = f32::from_bits(controls.playback_volume.load(Ordering::Relaxed));
        let mut tone_samples = tones.try_lock().ok();
        let mut call_tone_samples = controls.call_tones.try_lock().ok();
        consumer.synchronize();
        if controls.local_preview {
            let generation = controls.privacy_generation.load(Ordering::Acquire);
            if generation != preview_generation {
                consumer.clear();
                voice_output.reset();
                voice_seen = false;
                output_limiter.reset();
                #[cfg(feature = "native-rtc")]
                {
                    echo_samples.clear();
                    echo_resampler.reset();
                }
                preview_generation = generation;
            }
        }
        let mut samples = Some(&mut consumer);
        #[cfg(feature = "native-rtc")]
        let mut application_samples = controls.application_audio.try_queues();
        if muted {
            if let Some(samples) = samples.as_mut() {
                samples.clear();
            }
            voice_output.reset();
            voice_seen = false;
            output_limiter.reset();
            #[cfg(feature = "native-rtc")]
            {
                if let Some(samples) = application_samples.as_mut() {
                    samples.clear();
                }
                application_output.reset();
            }
        }
        let frames = data.len() / channels;
        voice_output.set_reserve_samples(controls.output_reserve.load(Ordering::Acquire));
        let target = voice_output.required_capacity(frames);
        let block_limit = target
            .saturating_add(PLAYBACK_QUANTUM_SAMPLES)
            .clamp(MAX_PLAYBACK_SAMPLES, MAX_DEVICE_PLAYBACK_SAMPLES);
        let limit = playback_limit
            .fetch_max(block_limit, Ordering::Relaxed)
            .max(block_limit);
        voice_output.set_max_samples(limit);
        #[cfg(feature = "native-rtc")]
        let mut application_ready = false;
        if !muted {
            if let Some(samples) = samples.as_mut() {
                if samples.len() > block_limit {
                    let excess = samples.len().saturating_sub(target.min(block_limit));
                    samples.drain(..excess);
                    counters.playback_drop.fetch_add(1, Ordering::Relaxed);
                }
            }
            #[cfg(feature = "native-rtc")]
            if let Some(samples) = application_samples.as_mut() {
                let generation = samples.clock_state().0;
                if application_generation != Some(generation) {
                    application_output.reset();
                    application_generation = Some(generation);
                }
                application_ready = samples.prepare_block(voice_output.source_samples(frames));
            }
        }
        #[cfg(feature = "native-rtc")]
        {
            // Never replay filter history while a producer/retirement owns the
            // sink lock. The real-time callback remains entirely nonblocking.
            if application_samples.is_none() {
                application_output.reset();
                application_generation = None;
            }
            let (_, available, decoded_frame) = application_samples
                .as_ref()
                .map_or((0, 0, 0), |samples| samples.clock_state());
            application_output.begin_prepared_block(
                application_ready.then_some(available),
                frames,
                // Keep one decoded frame of headroom; producer capacity also
                // leaves room for the next frame, without an additional wait.
                decoded_frame,
            );
        }
        voice_output.begin_block(samples.as_ref().map(|q| q.len()), frames);
        let (mut missing, mut priming) = (0, 0);
        for frame in data.chunks_mut(channels) {
            let voice = if muted {
                0.0
            } else {
                let sample = voice_output
                    .sample_from(|| samples.as_mut().and_then(|q| q.pop_front()), false);
                if sample.available {
                    voice_seen = true;
                } else if voice_seen {
                    missing += 1;
                } else {
                    priming += 1;
                }
                sample.value
            };
            while phase >= 1.0 {
                previous_tone = next_tone;
                next_tone = tone_samples
                    .as_mut()
                    .and_then(|q| q.pop_front())
                    .unwrap_or(0.0);
                let ring = call_tone_samples
                    .as_mut()
                    .and_then(|q| q.pop_front())
                    .unwrap_or(0.0);
                if !muted {
                    next_tone += ring;
                }
                phase -= 1.0;
            }
            // Notification cues keep their nominal clock. Application audio and
            // voice each follow their own bounded queue, independently.
            let tone = previous_tone + (next_tone - previous_tone) * phase as f32;
            #[cfg(feature = "native-rtc")]
            let application = if muted {
                [0.0; 2]
            } else {
                application_output
                    .frame_from(
                        || {
                            application_samples
                                .as_mut()
                                .and_then(|samples| samples.pop_frame(channels == 2))
                        },
                        true,
                    )
                    .values
            };
            #[cfg(not(feature = "native-rtc"))]
            let application = [0.0; 2];
            let rendered =
                output_limiter.frame(application.map(|sample| (voice + sample) * volume + tone));
            phase += step;
            if channels == 2 && frame.len() == 2 {
                frame[0] = T::from_sample(rendered[0]);
                frame[1] = T::from_sample(rendered[1]);
            } else {
                // Preserve existing mono fallback for non-stereo device layouts.
                frame.fill(T::from_sample(rendered[0]));
            }
            #[cfg(feature = "native-rtc")]
            if controls.echo_enabled.load(Ordering::Acquire) {
                // In Listen to Myself the voice is the person's own microphone.
                // As a reference it would make the canceller remove their live
                // voice, so the monitor rises and falls; only other sounds count.
                let reference = if controls.local_preview {
                    application.map(|sample| sample * volume + tone)
                } else {
                    rendered
                };
                echo_resampler.feed_bounded(
                    (reference[0] + reference[1]) * 0.5,
                    &mut echo_samples,
                    960,
                );
                while echo_samples.len() >= 480 {
                    let mut reference = [0.0; 480];
                    for sample in &mut reference {
                        *sample = echo_samples.pop_front().unwrap();
                    }
                    controls.echo_reference.rendered(&reference);
                }
            } else {
                echo_samples.clear();
                echo_resampler.reset();
            }
        }
        observe_voice(missing, priming);
    }
}

fn output_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    state: OutputState,
) -> Result<cpal::Stream, String>
where
    T: cpal::SizedSample + cpal::FromSample<f32> + 'static,
{
    let errors = state.errors.clone();
    let error_stop = state.stop.clone();
    let error_controls = state.controls.clone();
    let mut callback = output_callback::<T>(config, state);
    device
        .build_output_stream(
            config,
            move |data, _| callback(data),
            move |e| record_device_error(&errors, &error_stop, &error_controls, false, e),
            None,
        )
        .map_err(|e| format!("Open audio output: {e}"))
}

struct VoiceWorker {
    gateway: Option<crate::gateway_control::Session>,
    playback_limit: Arc<AtomicUsize>,
    socket: UdpSocket,
    remote: SocketAddr,
    session_id: u64,
    wire_version: u32,
    cipher: ChaCha20Poly1305,
    call: CallCipher,
    encoder: Encoder,
    capture: SampleQueue,
    playback: SampleQueue,
    stop: Arc<AtomicBool>,
    muted: Arc<AtomicBool>,
    deafened: Arc<AtomicBool>,
    counters: Arc<Counters>,
    errors: ErrorSlot,
    controls: Arc<AudioControls>,
    level: Arc<AtomicU32>,
}

fn capture_processing_samples(packet_samples: usize, controls: &AudioControls) -> usize {
    let needs_dsp = controls.suppression.load(Ordering::Relaxed);
    #[cfg(feature = "native-rtc")]
    let needs_dsp = needs_dsp || controls.echo_enabled.load(Ordering::Acquire);
    if needs_dsp {
        packet_samples.max(480)
    } else {
        packet_samples
    }
}

/// Automatic volume holds the speech gain at most this long after the last
/// speech block, while the level still sounds like speech.
const AGC_HOLD_SAMPLES: u32 = 300 * 48;
/// Automatic volume learns only from blocks above -60 dBFS.
const AGC_LEARN_FLOOR: f32 = 0.001;
/// Automatic volume never lifts the background above -50 dBFS.
const AGC_NOISE_CEILING: f32 = 0.00316;
/// Automatic volume rises at most this fast.
const AGC_RISE_DB_PER_SECOND: f32 = 15.0;
/// Below this fraction of the far end's level, a block with an echo path is
/// treated as the canceller's residue (-34 dB).
#[cfg(feature = "native-rtc")]
const ECHO_RESIDUE_RATIO: f32 = 0.02;

pub(crate) struct MicrophoneProcessor {
    /// The broadband noise suppressor, built on first use.
    denoiser: Option<Box<dyn crate::noise_suppressor::Suppressor>>,
    /// Which suppressor `denoiser` is, to rebuild it when the setting changes.
    denoiser_kind: Option<crate::noise_suppressor::SuppressorKind>,
    /// Fixed suppressor regardless of the noise model setting (audio bench).
    forced_suppressor: Option<crate::noise_suppressor::SuppressorKind>,
    #[cfg(feature = "native-rtc")]
    voice_detector: Option<crate::audio_activity::VoiceDetector>,
    clicks: crate::click_suppression::ClickSuppressor,
    gate: crate::voice_gate::NoiseGate,
    noise_floor: f32,
    noise_minimum: f32,
    noise_samples: usize,
    automatic_gain: f32,
    speech_output_gain: f32,
    speech_power: f32,
    /// Samples the speech gain is still held for after the last speech block.
    speech_hold: u32,
    #[cfg(feature = "native-rtc")]
    echo: Option<EchoCanceller>,
    #[cfg(feature = "native-rtc")]
    echo_error: Option<String>,
    #[cfg(feature = "native-rtc")]
    echo_outcome: crate::audio_echo::EchoOutcome,
    #[cfg(feature = "native-rtc")]
    privacy_closed: bool,
}
impl MicrophoneProcessor {
    pub(crate) fn new() -> Self {
        Self {
            denoiser: None,
            denoiser_kind: None,
            forced_suppressor: None,
            #[cfg(feature = "native-rtc")]
            voice_detector: None,
            clicks: crate::click_suppression::ClickSuppressor::new(),
            gate: crate::voice_gate::NoiseGate::new(),
            noise_floor: 0.0005,
            noise_minimum: f32::INFINITY,
            noise_samples: 0,
            automatic_gain: 1.0,
            speech_output_gain: 1.0,
            speech_power: 0.0,
            speech_hold: 0,
            #[cfg(feature = "native-rtc")]
            echo: None,
            #[cfg(feature = "native-rtc")]
            echo_error: None,
            #[cfg(feature = "native-rtc")]
            echo_outcome: Default::default(),
            #[cfg(feature = "native-rtc")]
            privacy_closed: false,
        }
    }
    /// A processor that always uses `kind`, whatever the noise model setting
    /// says; the setting still decides whether click suppression runs.
    #[cfg(any(test, feature = "audio-bench"))]
    pub(crate) fn with_suppressor(kind: crate::noise_suppressor::SuppressorKind) -> Self {
        Self {
            forced_suppressor: Some(kind),
            ..Self::new()
        }
    }
    pub(crate) fn process(&mut self, input: &mut [f32], controls: &AudioControls) -> bool {
        if input.is_empty() {
            return false;
        }
        let mode = controls.transmission.load(Ordering::Relaxed);
        if mode == TransmissionMode::PushToTalk as u32
            && !controls.ptt_pressed.load(Ordering::Acquire)
            && !(controls.whisper_enabled.load(Ordering::Acquire)
                && controls.whisper_pressed.load(Ordering::Acquire))
        {
            // Discard denoiser history while the talk key is up, so earlier private speech
            // cannot emerge from its lookahead when the key is next pressed.
            self.denoiser = None;
            self.denoiser_kind = None;
            #[cfg(feature = "native-rtc")]
            {
                self.voice_detector = None;
            }
            self.clicks = crate::click_suppression::ClickSuppressor::new();
            self.gate = crate::voice_gate::NoiseGate::new();
            self.automatic_gain = 1.0;
            self.speech_output_gain = 1.0;
            self.speech_power = 0.0;
            self.speech_hold = 0;
            #[cfg(feature = "native-rtc")]
            if !self.privacy_closed {
                if let Some(echo) = self.echo.as_mut() {
                    echo.reset();
                }
                controls.echo_reference.clear();
                self.privacy_closed = true;
            }
            return false;
        }
        #[cfg(feature = "native-rtc")]
        {
            self.privacy_closed = false;
            if controls.echo_enabled.load(Ordering::Acquire) {
                let echo = self
                    .echo
                    .get_or_insert_with(|| EchoCanceller::new(controls.echo_reference.clone()));
                match echo.process(input, controls.echo_delay_ms.load(Ordering::Relaxed)) {
                    Ok(outcome) => self.echo_outcome = outcome,
                    Err(error) => {
                        self.echo_error = Some(error);
                        return false;
                    }
                }
            } else {
                self.echo = None;
                self.echo_outcome = Default::default();
            }
        }
        let mut gain = f32::from_bits(controls.gain.load(Ordering::Relaxed));
        if controls.automatic_microphone_volume.load(Ordering::Relaxed) {
            let peak = input
                .iter()
                .filter(|sample| sample.is_finite())
                .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
            gain = gain.min(0.95 / peak.max(0.0001));
        }
        for sample in input.iter_mut() {
            *sample = if sample.is_finite() {
                (*sample * gain).clamp(-1.0, 1.0)
            } else {
                0.0
            };
        }
        #[cfg(feature = "native-rtc")]
        let mut detected = None;
        #[cfg(not(feature = "native-rtc"))]
        let detected: Option<bool> = None;
        if controls.suppression.load(Ordering::Relaxed) {
            let enhanced = controls.noise_model.load(Ordering::Relaxed)
                == crate::audio_settings::NoiseModel::Enhanced as u32;
            let strength = crate::click_suppression::NoiseStrength::from_u32(
                controls.noise_strength.load(Ordering::Relaxed),
            );
            let kind = self
                .forced_suppressor
                .unwrap_or_else(|| crate::noise_suppressor::for_setting(enhanced, strength));
            if self.denoiser.is_none() || self.denoiser_kind != Some(kind) {
                self.denoiser = Some(crate::noise_suppressor::make(kind));
                self.denoiser_kind = Some(kind);
            }
            #[cfg(feature = "native-rtc")]
            {
                let use_detector = controls.automatic_sensitivity.load(Ordering::Relaxed)
                    || controls.automatic_microphone_volume.load(Ordering::Relaxed);
                if use_detector && self.voice_detector.is_none() {
                    self.voice_detector = crate::audio_activity::VoiceDetector::new();
                } else if !use_detector {
                    self.voice_detector = None;
                }
            }
            let suppressor = self.denoiser.as_mut().unwrap();
            for chunk in input.chunks_exact_mut(crate::noise_suppressor::BLOCK) {
                let block: &mut [f32; crate::noise_suppressor::BLOCK] =
                    chunk.try_into().expect("chunks_exact yields whole blocks");
                suppressor.process(block);
                // Before speech detection, so a click cannot open the gate.
                // Click suppression needs a voice probability to spare speech.
                if let (true, Some(voice)) = (enhanced, suppressor.voice_probability()) {
                    self.clicks.process(chunk, voice, strength);
                }
                #[cfg(feature = "native-rtc")]
                if let Some(detector) = self.voice_detector.as_mut() {
                    let mut analysis = [0i16; 160];
                    for (to, from) in analysis.iter_mut().zip(chunk.chunks_exact(3)) {
                        // A fixed gain on the analysis path helps quiet microphones.
                        // Averaging three capture samples gives 16 kHz / 10 ms;
                        // transmitted PCM and packet timing remain unchanged.
                        let mean = from.iter().sum::<f32>() / 3.0;
                        *to = (mean * 8.0 * 32768.0).clamp(-32768.0, 32767.0) as i16;
                    }
                    *detected.get_or_insert(false) |= detector.is_speech(&analysis);
                }
            }
        } else {
            self.denoiser = None;
            self.denoiser_kind = None;
            self.clicks = crate::click_suppression::ClickSuppressor::new();
            #[cfg(feature = "native-rtc")]
            {
                self.voice_detector = None;
            }
        }
        // Detect before automatic volume adjustment: amplification must not turn
        // background noise into speech. The manual threshold keeps its meaning.
        let rms = (input.iter().map(|v| v * v).sum::<f32>() / input.len() as f32).sqrt();
        // With an echo path, a block far below the far end's level is what is
        // left of its echo: never boost it, nor let it open the gate.
        #[cfg(feature = "native-rtc")]
        let echo_like = self.echo_outcome.cancelling
            && rms < self.echo_outcome.far_rms * ECHO_RESIDUE_RATIO;
        #[cfg(not(feature = "native-rtc"))]
        let echo_like = false;
        // While the far end talks through an echo path, or before Relay knows
        // there is none, its echo could pass for speech: automatic volume
        // keeps what it learned from the person.
        #[cfg(feature = "native-rtc")]
        let far_talking = self.echo_outcome.path != crate::audio_echo::PathState::Absent
            && self.echo_outcome.far_rms > 0.003;
        #[cfg(not(feature = "native-rtc"))]
        let far_talking = false;
        let detected = detected.map(|speech| speech && !echo_like);
        if controls.automatic_microphone_volume.load(Ordering::Relaxed) {
            let peak = input
                .iter()
                .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
            let speech = detected.unwrap_or(rms >= 0.0015) && !echo_like;
            // A detector that fires on microphone hiss must not teach a huge gain.
            // Nor may a fan the detector mistakes for speech: learn only from
            // blocks that rise clear of the recent background.
            if speech
                && !far_talking
                && rms >= AGC_LEARN_FLOOR
                && self.gate.speech_like_level(rms)
            {
                // Learn from speech, retaining its level across normal pauses.
                // Explicit privacy boundaries and disabling AGC still reset it.
                // Track a speech envelope rather than flattening each 10 ms
                // syllable independently. This is a scalar level estimate, not
                // a buffered frame: packet and jitter timing do not change.
                let power = rms * rms;
                if self.speech_power <= 0.0 {
                    self.speech_power = power;
                } else {
                    let seconds = input.len() as f32 / AUDIO_SAMPLE_RATE as f32;
                    let time_constant = if power > self.speech_power {
                        0.04
                    } else {
                        0.30
                    };
                    self.speech_power +=
                        (power - self.speech_power) * (1.0 - (-seconds / time_constant).exp());
                }
                // About 3 dB more presence than the previous 0.06 RMS target;
                // the unchanged immediate peak ceiling protects loud inputs.
                let desired = (0.085 / self.speech_power.sqrt().max(0.00001)).clamp(0.25, 16.0);
                let desired = desired.min(0.95 / peak.max(0.0001));
                // Never lift the background of the last 2 s above -50 dBFS.
                let desired = self.gate.noise_floor().map_or(desired, |floor| {
                    desired.min((AGC_NOISE_CEILING / floor).max(1.0))
                });
                if desired < self.automatic_gain {
                    self.automatic_gain = desired;
                } else {
                    let blend = 1.0 - (-(input.len() as f32) / 4800.0).exp();
                    // At most 15 dB per second upward, so a sudden rise in background
                    // (RNNoise letting a fan through) cannot be lifted at once. The
                    // limit applies once the gate knows the room's noise floor, which
                    // is where a fan can be told from a voice; before that, and in
                    // modes where the gate learns no floor, quiet speech is lifted as
                    // quickly as before.
                    let step = (desired - self.automatic_gain) * blend;
                    let step = if self.gate.noise_floor().is_some() {
                        let seconds = input.len() as f32 / AUDIO_SAMPLE_RATE as f32;
                        let limit = self.automatic_gain
                            * (10f32.powf(AGC_RISE_DB_PER_SECOND * seconds / 20.0) - 1.0);
                        step.min(limit)
                    } else {
                        step
                    };
                    self.automatic_gain += step;
                }
            }
            if speech {
                self.speech_hold = AGC_HOLD_SAMPLES;
            } else {
                self.speech_hold = self.speech_hold.saturating_sub(input.len() as u32);
            }
            // A missed detection mid-word keeps the speech gain for up to 300 ms
            // while the level still sounds like speech (within 20 dB of it), so
            // the voice never jumps between the speech gain and unity. A real
            // pause is far quieter: it returns to unity at once, because a
            // remembered level must not amplify background noise.
            let held = !speech
                && self.speech_hold > 0
                && rms >= self.speech_power.sqrt() * 0.1;
            let ceiling = 0.95 / peak.max(0.0001);
            let gain = if speech || held { self.automatic_gain } else { 1.0 };
            let gain = gain.min(ceiling);
            // Interpolate speech gain inside this existing PCM block so a
            // continuous waveform does not acquire a step every packet. A
            // newly loud frame still takes the safe gain immediately.
            let from = if speech || held {
                self.speech_output_gain.min(ceiling)
            } else {
                gain
            };
            let step = (gain - from) / input.len() as f32;
            for (index, sample) in input.iter_mut().enumerate() {
                let level = from + step * (index + 1) as f32;
                *sample = (*sample * level).clamp(-0.95, 0.95);
            }
            if speech || held {
                self.speech_output_gain = gain;
            }
        } else {
            self.automatic_gain = 1.0;
            self.speech_output_gain = 1.0;
            self.speech_power = 0.0;
            self.speech_hold = 0;
        }
        let automatic = controls.automatic_sensitivity.load(Ordering::Relaxed);
        if mode == TransmissionMode::PushToTalk as u32 {
            self.gate = crate::voice_gate::NoiseGate::new();
            let held = controls.ptt_pressed.load(Ordering::Acquire)
                || (controls.whisper_enabled.load(Ordering::Acquire)
                    && controls.whisper_pressed.load(Ordering::Acquire));
            controls.gate_meter.record(rms, 0.0, held, false);
            return held;
        }
        if mode != TransmissionMode::VoiceActivation as u32 {
            self.gate = crate::voice_gate::NoiseGate::new();
            controls.gate_meter.record(rms, 0.0, true, false);
            return true;
        }
        let threshold = if automatic {
            // A bounded one-second minimum follows the quiet parts between words.
            // No extra audio frame is buffered, and speech cannot raise the gate
            // beyond 0.006 RMS (the former factory threshold was 0.02).
            let current = (self.noise_floor * 3.0).clamp(0.0015, 0.006);
            if rms < current && !self.gate.is_open() {
                self.noise_minimum = self.noise_minimum.min(rms);
                self.noise_samples += input.len();
                if self.noise_samples >= AUDIO_SAMPLE_RATE as usize {
                    self.noise_floor = self.noise_minimum.clamp(0.0001, 0.002);
                    self.noise_minimum = f32::INFINITY;
                    self.noise_samples = 0;
                }
            }
            (self.noise_floor * 3.0).clamp(0.0015, 0.006)
        } else {
            f32::from_bits(controls.threshold.load(Ordering::Relaxed))
        };
        let decision = if automatic {
            detected
        } else if echo_like {
            Some(false)
        } else {
            None
        };
        let speech = self.gate.detect(rms, threshold, decision);
        let timing = crate::voice_gate::GateTiming {
            attack_ms: controls.attack_ms.load(Ordering::Relaxed),
            hold_ms: controls.hold_ms.load(Ordering::Relaxed),
            release_ms: controls.release_ms.load(Ordering::Relaxed),
        };
        // A call prepends the retained speech start and fades that in instead.
        let staged_start =
            controls.preserve_speech_start.load(Ordering::Relaxed) && !controls.local_preview;
        let audible = self.gate.apply(input, speech, timing, staged_start);
        controls
            .gate_meter
            .record(rms, threshold, audible, automatic);
        audible
    }
}

pub(crate) fn validate_call_keys(keys: &CallKeys) -> Result<(), String> {
    let bounded = |value: &str, limit| !value.is_empty() && value.len() <= limit;
    if !bounded(&keys.group_id, 512)
        || !bounded(&keys.scope, 512)
        || !bounded(&keys.local_user_id, 128)
        || !bounded(&keys.local_device_id, 128)
        || keys.sender_keys.is_empty()
        || keys.sender_keys.len() > MAX_SPEAKERS * 2
        || keys
            .sender_keys
            .iter()
            .any(|(user, sender)| !bounded(user, 128) || !bounded(&sender.device_id, 128))
        || keys
            .sender_keys
            .get(&keys.local_user_id)
            .is_none_or(|sender| sender.device_id != keys.local_device_id)
    {
        return Err(
            "Verified call keys are missing or invalid. Verify the participants first.".into(),
        );
    }
    Ok(())
}

const PROTECTED_VOICE_FIXED_HEADER: usize = 60;
#[cfg(test)]
pub(crate) struct LivingVoiceProbe {
    controls: Arc<AudioControls>,
    muted: Arc<AtomicBool>,
    deafened: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
    socket: UdpSocket,
    outer: ChaCha20Poly1305,
    session: u64,
    capture: SampleQueue,
    playback: SampleQueue,
    counters: Arc<Counters>,
    errors: ErrorSlot,
    call: CallCipher,
}
#[cfg(test)]
impl LivingVoiceProbe {
    pub(crate) fn start(ticket: &VoiceTicket, keys: Arc<CallKeys>) -> Self {
        Self::start_with_settings(ticket, keys, None)
    }
    pub(crate) fn start_default_microphone(ticket: &VoiceTicket, keys: Arc<CallKeys>) -> Self {
        Self::start_with_settings(ticket, keys, Some(AudioSettings::default()))
    }
    fn start_with_settings(
        ticket: &VoiceTicket,
        keys: Arc<CallKeys>,
        settings: Option<AudioSettings>,
    ) -> Self {
        let remote = SocketAddr::from(([127, 0, 0, 1], ticket.udp_port));
        let (mut worker, stop, counters) = tests::probe_fixture(remote);
        if let Some(settings) = settings {
            assert!(settings.noise_suppression && settings.automatic_microphone_volume);
            assert!(settings.automatic_sensitivity());
            assert_eq!((settings.packet_ms, settings.jitter_ms), (10, 10));
            worker.controls = Arc::new(AudioControls::new(&settings));
            worker.encoder = crate::audio_settings::make_voice_encoder_with_recovery(
                settings.bitrate_kbps,
                settings.loss_recovery_enabled(),
            )
            .unwrap();
        }
        worker.session_id = ticket.session_id;
        worker.wire_version = ticket.wire_version;
        worker.cipher =
            ChaCha20Poly1305::new_from_slice(&STANDARD.decode(&ticket.key).unwrap()).unwrap();
        worker.call = CallCipher::new(keys.clone()).unwrap();
        worker.controls.packet_samples.store(480, Ordering::Relaxed);
        worker.muted.store(false, Ordering::Release);
        worker.deafened.store(false, Ordering::Release);
        let mut probe = Self {
            controls: worker.controls.clone(),
            muted: worker.muted.clone(),
            deafened: worker.deafened.clone(),
            stop,
            handle: None,
            socket: worker.socket.try_clone().unwrap(),
            outer: worker.cipher.clone(),
            session: ticket.session_id,
            capture: worker.capture.clone(),
            playback: worker.playback.clone(),
            counters,
            errors: worker.errors.clone(),
            call: CallCipher::new(keys).unwrap(),
        };
        probe.handle = Some(thread::spawn(move || voice_worker(worker)));
        probe
    }
    pub(crate) fn generated_quiet_speech() -> Vec<f32> {
        let samples = tests::owned_synthetic_speech();
        let peak = samples.iter().fold(0.0_f32, |p, s| p.max(s.abs()));
        samples
            .iter()
            .take(3 * 48_000)
            .map(|s| s * 0.005 / peak)
            .collect()
    }
    pub(crate) fn feed_generated_frame(&self, samples: &[f32]) {
        assert_eq!(samples.len(), 480);
        let mut queue = self.capture.lock().unwrap();
        assert!(queue.len() + samples.len() <= MAX_CAPTURE_SAMPLES);
        queue.extend(samples.iter().copied());
    }
    pub(crate) fn take_rendered(&self) -> Vec<f32> {
        self.playback.lock().unwrap().drain(..).collect()
    }
    pub(crate) fn set_microphone_privacy(&self, muted: bool, deafened: bool) {
        self.muted.store(muted, Ordering::Release);
        self.deafened.store(deafened, Ordering::Release);
        self.controls
            .privacy_generation
            .fetch_add(1, Ordering::AcqRel);
    }
    pub(crate) fn set_silent_gate(&self, muted: bool, closed_ptt: bool) {
        self.muted.store(muted, Ordering::Release);
        self.controls.ptt_key.store(0, Ordering::Release);
        self.controls.ptt_pressed.store(false, Ordering::Release);
        self.controls.transmission.store(
            if closed_ptt {
                TransmissionMode::PushToTalk
            } else {
                TransmissionMode::Continuous
            } as u32,
            Ordering::Release,
        );
        self.controls
            .privacy_generation
            .fetch_add(1, Ordering::AcqRel);
    }
    pub(crate) fn stats(&self) -> NetworkStats {
        self.counters.snapshot()
    }
    pub(crate) fn microphone_packets(&self) -> u64 {
        self.counters.sent_microphone_audio.load(Ordering::Relaxed)
    }
    pub(crate) fn feed(&self) {
        let mut queue = self.capture.lock().unwrap();
        if queue.len() < 480 * 4 {
            for n in 0..480 {
                queue.push_back((n as f32 * 440.0 * std::f32::consts::TAU / 48000.0).sin() * 0.3);
            }
        }
    }
    pub(crate) fn played(&self) -> bool {
        self.playback
            .lock()
            .unwrap()
            .iter()
            .any(|s| s.abs() > 0.001)
    }
    pub(crate) fn rejected(&self) -> u64 {
        self.counters.rejected.load(Ordering::Relaxed)
    }
    pub(crate) fn recording(&self) -> Vec<u8> {
        let mut encoder = Encoder::new(48000, Channels::Mono, Application::LowDelay).unwrap();
        let mut opus = [0; 1024];
        let length = encoder.encode_float(&[0.1; 480], &mut opus).unwrap();
        self.call
            .seal(self.session, 1, 480, true, &opus[..length])
            .unwrap()
    }
    pub(crate) fn inject(&self, sequence: u64, recording: &[u8]) {
        self.socket
            .send(&seal_packet(&self.outer, self.session, sequence, 0, recording).unwrap())
            .unwrap();
    }
    pub(crate) fn finish(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
    pub(crate) fn assert_healthy(&self) {
        let error = self.errors.lock().unwrap().clone();
        assert!(error.is_none(), "generated UDP worker failed: {error:?}");
    }
}
#[cfg(test)]
impl Drop for LivingVoiceProbe {
    fn drop(&mut self) {
        self.finish();
    }
}
#[cfg(test)]
pub(crate) fn actor_reopen_voice_probe(
    old_sender: Arc<CallKeys>,
    fresh_sender: Arc<CallKeys>,
    fresh_receiver: Arc<CallKeys>,
) -> Result<(), String> {
    let mut encoder = Encoder::new(AUDIO_SAMPLE_RATE, Channels::Mono, Application::LowDelay)
        .map_err(|_| "Synthetic Opus encoder could not start.")?;
    let mut opus = [0; 1024];
    let length = encoder
        .encode_float(&[0.0; 480], &mut opus)
        .map_err(|_| "Synthetic Opus frame could not be encoded.")?;
    let old = CallCipher::new(old_sender)?
        .seal(7, 1, 480, true, &opus[..length])
        .ok_or("Synthetic old protected frame could not be sealed.")?;
    let fresh_epoch = fresh_sender.epoch;
    let fresh = CallCipher::new(fresh_sender)?
        .seal(8, 1, 480, true, &opus[..length])
        .ok_or("Synthetic fresh protected frame could not be sealed.")?;
    let mut receiver = CallCipher::new(fresh_receiver)?;
    if receiver.open(&old).is_some() {
        return Err("A pre-reopen protected frame was accepted.".into());
    }
    let mut altered = old;
    altered[26..34].copy_from_slice(&fresh_epoch.to_le_bytes());
    if receiver.open(&altered).is_some() {
        return Err("A forged fresh epoch revived a pre-reopen frame.".into());
    }
    let frame = receiver
        .open(&fresh)
        .ok_or("Fresh protected Opus was rejected.")?;
    let mut decoder = Decoder::new(AUDIO_SAMPLE_RATE, Channels::Mono)
        .map_err(|_| "Synthetic Opus decoder could not start.")?;
    let mut pcm = [0.0; 960];
    if decoder
        .decode_float(&frame.opus, &mut pcm, false)
        .map_err(|_| "Fresh protected Opus could not be decoded.")?
        != 480
        || receiver.open(&fresh).is_some()
    {
        return Err("Fresh Opus duration or replay protection changed.".into());
    }
    Ok(())
}

struct ProtectedVoiceFrame {
    user_id: String,
    sequence: u64,
    timestamp: u64,
    restart: bool,
    opus: Vec<u8>,
    new_stream: bool,
}
struct CallCipher {
    keys: Arc<CallKeys>,
    nonce_prefix: [u8; 16],
}
#[derive(Default)]
pub(crate) struct VoiceReplay {
    // Never evict accepted nonce prefixes: otherwise the host could replay an
    // old recording under a new outer transport counter. A fresh verified epoch
    // is required at the bound; restarting a receiver must not clear this state.
    replay: HashMap<(String, [u8; 16]), ReplayWindow>,
    active_prefix: HashMap<String, [u8; 16]>,
}
impl CallCipher {
    fn new(keys: Arc<CallKeys>) -> Result<Self, String> {
        validate_call_keys(&keys)?;
        let mut nonce_prefix = [0; 16];
        getrandom::getrandom(&mut nonce_prefix)
            .map_err(|_| "Secure voice randomness is unavailable".to_string())?;
        Ok(Self { keys, nonce_prefix })
    }
    fn aad(&self, metadata: &[u8]) -> Vec<u8> {
        let mut result = b"relay-voice-e2ee-v1\0".to_vec();
        for value in [&self.keys.group_id, &self.keys.scope] {
            result.extend_from_slice(&(value.len() as u32).to_le_bytes());
            result.extend_from_slice(value.as_bytes());
        }
        result.extend_from_slice(metadata);
        result
    }
    fn seal(
        &self,
        routing_session: u64,
        sequence: u64,
        timestamp: u64,
        restart: bool,
        opus: &[u8],
    ) -> Option<Vec<u8>> {
        if sequence == 0 || opus.is_empty() {
            return None;
        }
        let sender = self.keys.sender_keys.get(&self.keys.local_user_id)?;
        let mut result = Vec::with_capacity(PROTECTED_VOICE_FIXED_HEADER + 256 + opus.len() + 16);
        result.push(AUDIO_PACKET_V3);
        result.extend_from_slice(&routing_session.to_le_bytes());
        result.extend_from_slice(&sequence.to_le_bytes());
        result.extend_from_slice(&timestamp.to_le_bytes());
        result.push(u8::from(restart));
        result.extend_from_slice(&self.keys.epoch.to_le_bytes());
        result.push(self.keys.local_user_id.len() as u8);
        result.push(self.keys.local_device_id.len() as u8);
        let mut nonce = [0; 24];
        nonce[..16].copy_from_slice(&self.nonce_prefix);
        nonce[16..].copy_from_slice(&sequence.to_le_bytes());
        result.extend_from_slice(&nonce);
        result.extend_from_slice(self.keys.local_user_id.as_bytes());
        result.extend_from_slice(self.keys.local_device_id.as_bytes());
        let cipher = XChaCha20Poly1305::new_from_slice(&*sender.opus).ok()?;
        let ciphertext = cipher
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: opus,
                    aad: &self.aad(&result[9..]),
                },
            )
            .ok()?;
        result.extend(ciphertext);
        (result.len() + UDP_HEADER_LEN + 16 <= UDP_MAX_PACKET).then_some(result)
    }
    fn open(&mut self, packet: &[u8]) -> Option<ProtectedVoiceFrame> {
        if packet.first() != Some(&AUDIO_PACKET_V3)
            || packet.len() < PROTECTED_VOICE_FIXED_HEADER + 2 + 17
            || packet.len() + UDP_HEADER_LEN + 16 > UDP_MAX_PACKET
            || packet[25] & !1 != 0
            || u64::from_le_bytes(packet[26..34].try_into().ok()?) != self.keys.epoch
        {
            return None;
        }
        let user_len = usize::from(packet[34]);
        let device_len = usize::from(packet[35]);
        if !(1..=128).contains(&user_len) || !(1..=128).contains(&device_len) {
            return None;
        }
        let end = PROTECTED_VOICE_FIXED_HEADER + user_len + device_len;
        if packet.len() <= end + 16 {
            return None;
        }
        let user = std::str::from_utf8(&packet[60..60 + user_len]).ok()?;
        let device = std::str::from_utf8(&packet[60 + user_len..end]).ok()?;
        let sender = self.keys.sender_keys.get(user)?;
        if sender.device_id != device || user == self.keys.local_user_id {
            return None;
        }
        let sequence = u64::from_le_bytes(packet[9..17].try_into().ok()?);
        if sequence == 0 || packet[52..60] != packet[9..17] {
            return None;
        }
        let cipher = XChaCha20Poly1305::new_from_slice(&*sender.opus).ok()?;
        let opus = cipher
            .decrypt(
                XNonce::from_slice(&packet[36..60]),
                Payload {
                    msg: &packet[end..],
                    aad: &self.aad(&packet[9..end]),
                },
            )
            .ok()?;
        let prefix: [u8; 16] = packet[36..52].try_into().ok()?;
        let replay_key = (user.to_owned(), prefix);
        // Authenticate first. The key manager retains this window across native
        // device/transport restarts under the same verified MLS context. Poison
        // fails closed; neither a new worker nor a changed outer counter resets it.
        let mut replay = self.keys.voice_replay.lock().ok()?;
        let new_stream = replay.active_prefix.get(user) != Some(&prefix);
        if new_stream && replay.replay.contains_key(&replay_key) {
            return None; // A retired sender instance cannot replace its successor.
        }
        if !replay.replay.contains_key(&replay_key) && replay.replay.len() >= MAX_SPEAKERS * 4 {
            return None;
        }
        if !replay
            .replay
            .entry(replay_key)
            .or_default()
            .accept(sequence)
        {
            return None;
        }
        replay.active_prefix.insert(user.to_owned(), prefix);
        Some(ProtectedVoiceFrame {
            user_id: user.to_owned(),
            sequence,
            timestamp: u64::from_le_bytes(packet[17..25].try_into().ok()?),
            restart: packet[25] & 1 != 0,
            opus,
            new_stream,
        })
    }
}

#[derive(Default)]
struct PlayoutSchedule {
    next: Option<Instant>,
    running: bool,
}
impl PlayoutSchedule {
    fn due<K>(&mut self, now: Instant, speakers: &HashMap<K, Speaker>, jitter_ms: u32) -> bool {
        if !self.running {
            self.next = speakers
                .values()
                .filter_map(|speaker| speaker.next_playout_deadline(now, jitter_ms))
                .min();
        }
        self.next.is_some_and(|deadline| now >= deadline)
    }
    #[cfg(test)]
    fn complete(&mut self, now: Instant, emitted: bool) {
        self.complete_interval(now, emitted, FRAME_TIME);
    }
    fn complete_interval(&mut self, now: Instant, emitted: bool, interval: Duration) {
        if emitted {
            let deadline = self.next.expect("playout was due");
            self.running = true;
            // Catch up less than one 10 ms packet, at most four 2.5 ms mixer
            // quanta. Larger stalls still resume at a future deadline.
            self.next = Some(
                if now.saturating_duration_since(deadline) >= (interval * 4).min(SHORT_WORKER_STALL)
                {
                    now + interval
                } else {
                    deadline + interval
                },
            );
        } else {
            self.running = false;
            self.next = None;
        }
    }
}

fn voice_worker(mut w: VoiceWorker) {
    if let Ok(mut direct) = w.controls.direct.lock() {
        *direct = Some(crate::direct_voice::DirectVoice::new(w.session_id));
    }
    let mut next_direct_request = Instant::now();
    let mut direct_was_enabled = false;
    let wait = crate::audio_wake::WorkerWait::new(&w.controls.wake, &w.socket);
    let mut sequence = 1u64;
    let mut media_sequence = 1u64;
    // Voice first: sustained incoming trouble throttles video, uploads and copies.
    let mut voice_first = crate::voice_priority::Detector::default();
    let _clear_voice_first = crate::voice_priority::ClearOnExit;
    let mut sample_clock = 0u64;
    let mut replay = ReplayWindow::default();
    let mut speakers: HashMap<String, Speaker> = HashMap::new();
    // Loss, jitter and lateness per person; outlives short pauses in speech.
    let mut arrivals: HashMap<String, crate::voice_quality::ArrivalStats> = HashMap::new();
    let mut heard_at = Instant::now();
    let mut private_receive_generation = w
        .controls
        .whisper_receive_generation
        .load(Ordering::Acquire);
    let mut buffer = [0u8; UDP_MAX_PACKET + 1];
    let mut encoded = [0u8; UDP_MAX_PACKET - UDP_HEADER_LEN - 16 - AUDIO_V2_HEADER_LEN];
    let mut microphone = MicrophoneProcessor::new();
    let mut encoded_history = false;
    let mut last_bitrate = 0;
    let mut last_recovery = w.encoder.get_inband_fec().unwrap_or(false);
    // Automatic loss recovery, from incoming loss.
    let mut loss_policy = crate::voice_quality::LossRecoveryPolicy::default();
    let mut last_packet_samples = 0;
    let mut last_whisper: Option<Vec<String>> = None;
    let mut last_privacy_generation = w.controls.privacy_generation.load(Ordering::Acquire);
    let mut playout = PlayoutSchedule::default();
    let mut last_capture = None;
    let mut speech_preroll = crate::voice_preroll::SpeechPreroll::default();
    let mut next_ptt_poll = Instant::now();
    // The relay pins this socket's address only after an authenticated keepalive.
    // Register it before the first Opus packet, otherwise initial speech is dropped.
    send_packet(&w, &mut sequence, &[0]);
    let mut last_keepalive = Instant::now();
    let started = Instant::now();
    let mut next_probe = started;
    let mut probe_id = 0_u64;
    let mut pending_probes = crate::voice_recovery::PendingProbes::default();
    let mut pending_tcp_probes = crate::voice_recovery::PendingProbes::default();
    let mut last_tcp_reply: Option<Instant> = None;
    let mut last_tcp_ready = false;
    let mut recovery = crate::voice_recovery::VoiceRecovery::new(started);
    while !w.stop.load(Ordering::Acquire) {
        let generation = w
            .controls
            .whisper_receive_generation
            .load(Ordering::Acquire);
        if generation != private_receive_generation {
            speakers.retain(|key, _| !key.starts_with("whisper:"));
            if let Ok(mut playback) = w.playback.lock() {
                playback.clear();
            }
            private_receive_generation = generation;
        }
        wait.begin_receive();
        // Drain in bounded batches so a busy peer cannot starve capture/playback.
        for _ in 0..64 {
            let tunnel_packet = w
                .controls
                .tunnel
                .lock()
                .ok()
                .and_then(|slot| slot.as_ref().and_then(|tunnel| tunnel.receive()));
            let via_tcp = tunnel_packet.is_some();
            let received = if let Some(packet) = tunnel_packet {
                let n = packet.len();
                buffer[..n].copy_from_slice(&packet);
                Ok((n, w.remote))
            } else {
                w.socket.recv_from(&mut buffer)
            };
            match received {
                Ok((n, mut from)) => {
                    if n > UDP_MAX_PACKET {
                        w.counters.rejected.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    // A frame from this person's gateway in the voice mesh between homes.
                    let mesh_frame = if !via_tcp && from != w.remote && buffer[..n].starts_with(b"RGW1") {
                        match w.gateway.as_ref().and_then(|gateway| gateway.mesh_receive(
                            from, &buffer[..n], w.deafened.load(Ordering::Acquire), Instant::now(),
                        )) {
                            Some(Some(frame)) => Some(frame),
                            Some(None) => continue,
                            None => None,
                        }
                    } else { None };
                    let via_gateway = mesh_frame.is_none() && !via_tcp && from != w.remote && buffer[..n].starts_with(b"RGW1")
                        && w.gateway.as_ref().and_then(|gateway| gateway.with_path(|path| path.address == from)).unwrap_or(false);
                    let n = if via_gateway {
                        let inner = w.gateway.as_ref().and_then(|gateway| gateway.with_path(
                            |path| path.receive(from, &buffer[..n], Instant::now())
                        )).flatten();
                        let Some(inner) = inner else { continue; };
                        let n = inner.len();
                        buffer[..n].copy_from_slice(&inner);
                        from = w.remote;
                        n
                    } else { n };
                    let plaintext = if let Some(frame) = mesh_frame {
                        frame
                    } else if !via_tcp
                        && from != w.remote
                        && buffer[..n].starts_with(relay_protocol::direct_voice::MARKER)
                    {
                        if !w.controls.direct_enabled.load(Ordering::Acquire)
                            || !w.counters.udp_reachable()
                        {
                            continue;
                        }
                        let received = w.controls.direct.lock().ok().and_then(|mut route| {
                            route.as_mut()?.receive(from, &buffer[..n], Instant::now())
                        });
                        match received {
                            Some(crate::direct_voice::Received::Audio(audio))
                                if !w.deafened.load(Ordering::Acquire) =>
                            {
                                audio
                            }
                            Some(crate::direct_voice::Received::Reply(address, reply)) => {
                                let _ = w.socket.send_to(&reply, address);
                                continue;
                            }
                            Some(crate::direct_voice::Received::Control) => continue,
                            _ => {
                                w.counters.rejected.fetch_add(1, Ordering::Relaxed);
                                continue;
                            }
                        }
                    } else {
                        if from != w.remote {
                            w.counters.rejected.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                        let Some((seq, plaintext)) =
                            open_packet(&w.cipher, w.session_id, 1, &buffer[..n])
                        else {
                            w.counters.rejected.fetch_add(1, Ordering::Relaxed);
                            continue;
                        };
                        if !replay.accept(seq) {
                            w.counters.rejected.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                        plaintext
                    };
                    if plaintext.first() == Some(&relay_protocol::direct_voice::ROSTER) {
                        if w.wire_version >= 4 && w.controls.direct_enabled.load(Ordering::Acquire)
                        {
                            if let Ok(roster) = serde_json::from_slice(&plaintext[1..]) {
                                if let Ok(mut direct) = w.controls.direct.lock() {
                                    if let Some(direct) = direct.as_mut() {
                                        direct.install(roster, Instant::now());
                                    }
                                }
                            }
                        }
                        continue;
                    }
                    w.counters.received.fetch_add(1, Ordering::Relaxed);
                    if plaintext.first() == Some(&relay_protocol::UDP_PATH_PROBE) {
                        if via_gateway {
                            if let Some(gateway) = &w.gateway {
                                gateway.with_path(|path| path.confirm_server(&plaintext, Instant::now()));
                            }
                        }
                        continue;
                    }
                    if plaintext.first() == Some(&UDP_PING) && plaintext.len() == 9 {
                        if via_tcp {
                            let now = Instant::now();
                            if let Some(sent) = pending_tcp_probes.take_reply(
                                u64::from_le_bytes(plaintext[1..9].try_into().unwrap()),
                                now,
                            ) {
                                w.counters.tcp_rtt_us.store(
                                    now.saturating_duration_since(sent)
                                        .as_micros()
                                        .clamp(1, u64::MAX as u128)
                                        as u64,
                                    Ordering::Relaxed,
                                );
                                w.counters
                                    .tcp_rtt_at
                                    .store(w.counters.clock.tick(now), Ordering::Release);
                                last_tcp_reply = Some(now);
                            }
                            continue;
                        }
                        let id = u64::from_le_bytes(plaintext[1..9].try_into().unwrap());
                        if let Some(sent) = pending_probes.take_reply(id, Instant::now()) {
                            if recovery.confirm_with_live_fallback(
                                Instant::now(),
                                last_tcp_reply
                                    .is_some_and(|reply| reply.elapsed() < Duration::from_secs(5)),
                            ) {
                                let now = Instant::now();
                                if !via_gateway {
                                    w.counters.udp_rtt_us.store(
                                    now.duration_since(sent)
                                        .as_micros()
                                        .max(1)
                                        .min(u64::MAX as u128)
                                        as u64,
                                    Ordering::Relaxed,
                                );
                                w.counters
                                    .udp_rtt_at
                                    .store(w.counters.clock.tick(now), Ordering::Release);
                                }
                            }
                        }
                        continue;
                    }
                    // The outer key is known to the relay. Only inner client-held
                    // keys authenticate the peer and protect its Opus payload.
                    let whisper = plaintext.first() == Some(&AUDIO_WHISPER);
                    let frame = if whisper && w.controls.whisper_enabled.load(Ordering::Acquire) {
                        relay_protocol::unwrap_whisper(&plaintext).and_then(|(target, inner)| {
                            if target != w.call.keys.local_user_id || inner.len() < 60 {
                                return None;
                            }
                            let user_len = inner[34] as usize;
                            let peer = std::str::from_utf8(inner.get(60..60 + user_len)?).ok()?;
                            w.controls
                                .whisper_ciphers
                                .lock()
                                .ok()?
                                .get_mut(peer)?
                                .open(inner)
                        })
                    } else if !whisper {
                        w.call.open(&plaintext)
                    } else {
                        None
                    };
                    let Some(frame) = frame else {
                        w.counters.rejected.fetch_add(1, Ordering::Relaxed);
                        continue;
                    };
                    let speaker_key = if whisper {
                        format!("whisper:{}", frame.user_id)
                    } else {
                        frame.user_id.clone()
                    };
                    if (frame.new_stream || !speakers.contains_key(&speaker_key))
                        && (speakers.contains_key(&speaker_key)
                            || speakers.len() < MAX_SPEAKERS * 2)
                    {
                        if let Ok(speaker) = Speaker::new(Instant::now()) {
                            speakers.insert(speaker_key.clone(), speaker);
                        }
                    }
                    if let Some(speaker) = speakers.get_mut(&speaker_key) {
                        speaker.configure_adaptation(
                            w.controls.adaptive_jitter.load(Ordering::Relaxed),
                        );
                        let arrived = Instant::now();
                        let late_before = speaker.stats.late;
                        let arrival = arrivals
                            .entry(speaker_key.clone())
                            .or_insert_with(|| crate::voice_quality::ArrivalStats::new(arrived));
                        arrival.observe(frame.sequence, frame.timestamp, frame.restart, arrived);
                        speaker.push_timed(
                            frame.sequence,
                            frame.timestamp,
                            frame.restart,
                            frame.opus,
                            arrived,
                        );
                        // Too late to play, or pushed out of a full queue.
                        for _ in late_before..speaker.stats.late {
                            arrival.late(arrived);
                        }
                    }
                }
                Err(e) if voice_io_retryable(&e) => break,
                Err(e) => {
                    record_error(&w.errors, format!("Voice receive failed: {e}"));
                    break;
                }
            }
        }
        let now = Instant::now();
        if gateway_tick(&w, &mut sequence, now) {
            next_probe = now;
            if !w.counters.gateway_active.load(Ordering::Acquire)
                && !w.counters.clock.age(w.counters.udp_rtt_at.load(Ordering::Acquire), now)
                    .is_some_and(|age| age < Duration::from_secs(5))
            {
                recovery.route_lost(now);
                w.counters.publish_udp_link(recovery.state(now));
                send_tunnel_control(&w, &mut sequence, &[0]);
            }
        }
        let direct_enabled = w.wire_version >= 4
            && w.controls.direct_enabled.load(Ordering::Acquire)
            && !w.counters.gateway_active.load(Ordering::Acquire)
            && w.counters.udp_reachable();
        if direct_enabled != direct_was_enabled || (direct_enabled && now >= next_direct_request) {
            next_direct_request = now + Duration::from_millis(200);
            let request = w.controls.direct.lock().ok().and_then(|mut direct| {
                Some(direct.as_mut()?.request(
                    direct_enabled,
                    w.counters.udp_rtt_us.load(Ordering::Relaxed),
                    now,
                ))
            });
            if let Some(request) = request {
                send_packet(&w, &mut sequence, &request);
            }
            direct_was_enabled = direct_enabled;
        }
        if let Ok(mut direct) = w.controls.direct.lock() {
            if let Some(direct) = direct.as_mut() {
                if !direct_enabled {
                    direct.disable(now);
                }
                if direct_enabled {
                    if let Some((address, packet)) = direct.probe(now) {
                        let _ = w.socket.send_to(&packet, address);
                    }
                }
                w.counters.direct_active.store(
                    direct_enabled
                        && direct.active(w.counters.udp_rtt_us.load(Ordering::Relaxed), now),
                    Ordering::Release,
                );
                w.counters.direct_rtt_us.store(
                    if direct_enabled {
                        direct.rtt_us().unwrap_or(0)
                    } else {
                        0
                    },
                    Ordering::Relaxed,
                );
                w.counters.direct_rtt_at.store(
                    if direct_enabled {
                        direct
                            .rtt_measured_at()
                            .map(|at| w.counters.clock.tick(at))
                            .unwrap_or(0)
                    } else {
                        0
                    },
                    Ordering::Release,
                );
            }
        }
        if w.wire_version >= 2 {
            if now >= next_probe {
                next_probe = now + Duration::from_secs(2);
                if let Some(next) = probe_id.checked_add(1) {
                    probe_id = next;
                    let mut probe = vec![UDP_PING];
                    probe.extend_from_slice(&probe_id.to_le_bytes());
                    // UDP may never have reached the server. Register this
                    // authenticated transport before probing, just as UDP does.
                    if send_tunnel_control(&w, &mut sequence, &[0])
                        && send_tunnel_control(&w, &mut sequence, &probe)
                    {
                        pending_tcp_probes.record(probe_id, now);
                    }
                    if send_packet(&w, &mut sequence, &probe) {
                        pending_probes.record(probe_id, now);
                    }
                }
            }
            let link = recovery.state(now);
            w.counters.publish_udp_link(link);
            let tcp_ready = if let Ok(mut tunnel) = w.controls.tunnel.lock() {
                let enabled = w.controls.tcp_allowed();
                if !enabled || link == crate::voice_recovery::LinkState::Connected {
                    tunnel.take();
                } else if tunnel.is_none()
                    && matches!(
                        link,
                        crate::voice_recovery::LinkState::Reconnecting { .. }
                            | crate::voice_recovery::LinkState::Expired
                    )
                {
                    if let Some(config) = w
                        .controls
                        .tunnel_config
                        .lock()
                        .ok()
                        .and_then(|config| config.clone())
                    {
                        let controls = Arc::downgrade(&w.controls);
                        let muted = w.muted.clone();
                        let deafened = w.deafened.clone();
                        *tunnel = Some(crate::voice_tunnel::Tunnel::new(
                            config,
                            w.stop.clone(),
                            move |generation| {
                                !muted.load(Ordering::Acquire)
                                    && !deafened.load(Ordering::Acquire)
                                    && controls.upgrade().is_some_and(|controls| {
                                        controls.privacy_generation.load(Ordering::Acquire)
                                            == generation
                                            && (controls.transmission.load(Ordering::Relaxed)
                                                != TransmissionMode::PushToTalk as u32
                                                || controls.ptt_pressed.load(Ordering::Acquire)
                                                || (controls
                                                    .whisper_enabled
                                                    .load(Ordering::Acquire)
                                                    && controls
                                                        .whisper_pressed
                                                        .load(Ordering::Acquire)))
                                    })
                            },
                        ));
                    }
                }
                tunnel.as_ref().is_some_and(|tunnel| tunnel.ready())
            } else {
                false
            };
            if tcp_ready && !last_tcp_ready {
                send_tunnel_control(&w, &mut sequence, &[0]);
                next_probe = now;
            }
            last_tcp_ready = tcp_ready;
            w.counters.tcp_active.store(
                tcp_ready
                    && last_tcp_reply.is_some_and(|reply| reply.elapsed() < Duration::from_secs(5)),
                Ordering::Release,
            );
            if link == crate::voice_recovery::LinkState::Expired
                && last_tcp_reply.is_none_or(|reply| reply.elapsed() >= Duration::from_secs(35))
            {
                if w.stop
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    record_error(&w.errors, "Voice could not reconnect within 30 seconds. Check the host or connection, then join the voice room again.".into());
                }
                w.controls.transmitting.store(false, Ordering::Release);
                if let Ok(mut queue) = w.capture.try_lock() {
                    queue.clear();
                }
                if let Ok(mut queue) = w.playback.try_lock() {
                    queue.clear();
                }
                break;
            }
        }
        if now >= next_ptt_poll {
            next_ptt_poll = now
                + if w.controls.transmission.load(Ordering::Relaxed)
                    == TransmissionMode::PushToTalk as u32
                    || w.controls.whisper_enabled.load(Ordering::Acquire)
                {
                    Duration::from_micros(2500)
                } else {
                    Duration::from_millis(10)
                };
            poll_push_to_talk(&w.controls);
        }
        let whisper_target = if w.controls.whisper_enabled.load(Ordering::Acquire)
            && w.controls.whisper_pressed.load(Ordering::Acquire)
        {
            Some(
                w.controls
                    .whisper_target
                    .lock()
                    .map(|targets| targets.clone())
                    .unwrap_or_default(),
            )
        } else {
            None
        };
        let capture_generation = w.controls.privacy_generation.load(Ordering::Acquire);
        if whisper_target != last_whisper || capture_generation != last_privacy_generation {
            speech_preroll.clear();
            discard_encoder_history(&mut w.encoder, &w.errors, &w.stop);
            encoded_history = false;
            microphone = MicrophoneProcessor::new();
            if let Ok(mut capture) = w.capture.lock() {
                capture.clear();
            }
            last_whisper = whisper_target.clone();
            last_privacy_generation = capture_generation;
        }
        // The native capture callback determines when a complete processing PCM
        // frame exists. Do not add an unrelated wall-clock frame phase to it.
        // At most one bounded frame is processed per worker iteration.
        {
            let packet_samples = w.controls.packet_samples.load(Ordering::Relaxed) as usize;
            if last_packet_samples != packet_samples {
                speech_preroll.clear();
                discard_encoder_history(&mut w.encoder, &w.errors, &w.stop);
                encoded_history = false;
                microphone = MicrophoneProcessor::new();
                last_packet_samples = packet_samples;
            }
            let choice = if w.controls.recover_packet_loss.load(Ordering::Relaxed) {
                crate::audio_settings::LossRecovery::Always
            } else if w.controls.automatic_loss_recovery.load(Ordering::Relaxed) {
                crate::audio_settings::LossRecovery::Automatic
            } else {
                crate::audio_settings::LossRecovery::Off
            };
            // The worst incoming loss over the 5 s window among people with
            // enough packets to measure it.
            let high_loss = loss_policy.observe(
                crate::voice_quality::loss_evidence(
                    arrivals.values_mut().filter_map(|arrival| arrival.quality(now)),
                ),
                now,
            );
            let recovery = choice.active(high_loss, packet_samples);
            if recovery != last_recovery {
                match crate::audio_settings::make_voice_encoder_with_recovery(
                    w.controls.bitrate_kbps.load(Ordering::Relaxed),
                    recovery,
                ) {
                    Ok(encoder) => {
                        // Not a restart: listeners keep decoding across the
                        // rebuilt encoder. A restart would make their buffer
                        // start over mid-sentence (a gap on older Relay).
                        w.encoder = encoder;
                        last_recovery = recovery;
                    }
                    Err(error) => {
                        record_error(&w.errors, format!("Configure voice recovery: {error}"));
                        w.stop.store(true, Ordering::Release);
                        break;
                    }
                }
            }
            let processing_samples = capture_processing_samples(packet_samples, &w.controls);
            let mut storage = [0.0f32; AUDIO_FRAME_SAMPLES];
            let input = &mut storage[..processing_samples];
            let have_frame = if let Ok(mut samples) = w.capture.try_lock() {
                if w.muted.load(Ordering::Relaxed) || w.deafened.load(Ordering::Relaxed) {
                    samples.clear();
                    false
                } else if samples.len() >= processing_samples {
                    for sample in input.iter_mut() {
                        *sample = samples.pop_front().unwrap_or(0.0);
                    }
                    true
                } else {
                    false
                }
            } else {
                false
            };
            if have_frame {
                if last_capture
                    .is_some_and(|at| now.saturating_duration_since(at) >= FRAME_TIME * 3)
                {
                    speech_preroll.clear();
                }
                last_capture = Some(now);
                let frame_timestamp = sample_clock;
                let Some(next_clock) = sample_clock.checked_add(processing_samples as u64) else {
                    record_error(&w.errors, "Voice media clock exhausted".into());
                    break;
                };
                sample_clock = next_clock;
                let bitrate = w.controls.bitrate_kbps.load(Ordering::Relaxed);
                if bitrate != last_bitrate {
                    match w
                        .encoder
                        .set_bitrate(Bitrate::Bits((bitrate * 1000) as i32))
                    {
                        Ok(()) => last_bitrate = bitrate,
                        Err(e) => record_error(&w.errors, format!("Set voice bitrate: {e}")),
                    }
                }
                let transmit = microphone.process(input, &w.controls);
                #[cfg(feature = "native-rtc")]
                if let Some(error) = microphone.echo_error.take() {
                    record_error(&w.errors, error);
                    w.stop.store(true, Ordering::Release);
                    break;
                }
                let rms = (input.iter().map(|sample| sample * sample).sum::<f32>()
                    / processing_samples as f32)
                    .sqrt();
                w.level
                    .store((rms * 3.0).min(1.0).to_bits(), Ordering::Relaxed);
                w.controls.transmitting.store(transmit, Ordering::Relaxed);
                let allowed = capture_send_allowed(&w);
                if !allowed {
                    speech_preroll.clear();
                }
                let retain_start = allowed
                    && w.controls.preserve_speech_start.load(Ordering::Relaxed)
                    && w.controls.transmission.load(Ordering::Relaxed)
                        == TransmissionMode::VoiceActivation as u32;
                let mut send_pcm = [0.0; crate::voice_preroll::MAX_OUTPUT];
                let staged = speech_preroll.stage(
                    input,
                    frame_timestamp,
                    packet_samples,
                    retain_start,
                    transmit && allowed,
                    w.controls.attack_ms.load(Ordering::Relaxed) as usize * 48,
                    &mut send_pcm,
                );
                if let Some((frame_timestamp, input)) = staged {
                    for (packet_index, packet_input) in
                        input.chunks_exact(packet_samples).enumerate()
                    {
                        if !capture_send_allowed(&w) {
                            discard_encoder_history(&mut w.encoder, &w.errors, &w.stop);
                            encoded_history = false;
                            break;
                        }
                        match w.encoder.encode_float(packet_input, &mut encoded) {
                            Ok(n) => {
                                let restart = !encoded_history;
                                encoded_history = true;
                                let Some(next) = media_sequence.checked_add(1) else {
                                    record_error(
                                        &w.errors,
                                        "Voice source sequence exhausted".into(),
                                    );
                                    break;
                                };
                                let reserved_media_sequence = media_sequence;
                                media_sequence = next;
                                let protected = if let Some(peers) = &whisper_target {
                                    w.controls.whisper_ciphers.lock().ok().and_then(|ciphers| {
                                        if peers.is_empty() {
                                            return None;
                                        }
                                        peers
                                            .iter()
                                            .map(|peer| {
                                                ciphers
                                                    .get(peer)
                                                    .and_then(|cipher| {
                                                        cipher.seal(
                                                            w.session_id,
                                                            reserved_media_sequence,
                                                            frame_timestamp
                                                                + (packet_index * packet_samples)
                                                                    as u64,
                                                            restart,
                                                            &encoded[..n],
                                                        )
                                                    })
                                                    .and_then(|inner| {
                                                        relay_protocol::wrap_whisper(peer, &inner)
                                                    })
                                            })
                                            .collect::<Option<Vec<_>>>()
                                    })
                                } else {
                                    w.call
                                        .seal(
                                            w.session_id,
                                            reserved_media_sequence,
                                            frame_timestamp
                                                + (packet_index * packet_samples) as u64,
                                            restart,
                                            &encoded[..n],
                                        )
                                        .map(|packet| vec![packet])
                                };
                                let Some(plaintext) = protected else {
                                    w.controls.transmitting.store(false, Ordering::Release);
                                    discard_encoder_history(&mut w.encoder, &w.errors, &w.stop);
                                    encoded_history = false;
                                    break;
                                };
                                for plaintext in plaintext {
                                    if !send_packet_at(
                                        &w,
                                        &mut sequence,
                                        &plaintext,
                                        capture_generation,
                                    ) {
                                        discard_encoder_history(&mut w.encoder, &w.errors, &w.stop);
                                        encoded_history = false;
                                    }
                                }
                            }
                            Err(e) => {
                                record_error(&w.errors, format!("Encode microphone audio: {e}"));
                                discard_encoder_history(&mut w.encoder, &w.errors, &w.stop);
                                encoded_history = false;
                            }
                        }
                    }
                } else if encoded_history {
                    discard_encoder_history(&mut w.encoder, &w.errors, &w.stop);
                    encoded_history = false;
                }
            } else if !capture_send_allowed(&w) {
                speech_preroll.clear();
                w.controls.transmitting.store(false, Ordering::Relaxed);
                microphone = MicrophoneProcessor::new();
                if encoded_history {
                    discard_encoder_history(&mut w.encoder, &w.errors, &w.stop);
                    encoded_history = false;
                }
            } else if last_capture
                .is_none_or(|at| now.saturating_duration_since(at) >= FRAME_TIME * 3)
            {
                speech_preroll.clear();
                w.controls.transmitting.store(false, Ordering::Relaxed);
                if encoded_history {
                    discard_encoder_history(&mut w.encoder, &w.errors, &w.stop);
                    encoded_history = false;
                }
            }
        }
        let jitter_ms = w.controls.jitter_ms.load(Ordering::Relaxed);
        for speaker in speakers.values_mut() {
            speaker.configure_adaptation(w.controls.adaptive_jitter.load(Ordering::Relaxed));
        }
        w.counters.receive_target_ms.store(
            speakers
                .values()
                .map(|speaker| speaker.buffer_target_ms(jitter_ms))
                .max()
                .unwrap_or(0),
            Ordering::Relaxed,
        );
        arrivals.retain(|_, arrival| !arrival.expired(now));
        let incoming = crate::voice_quality::worst(
            arrivals.values_mut().filter_map(|arrival| arrival.quality(now)),
        );
        w.counters.publish_voice_quality(incoming);
        // Per speaker, for the host overview's measurements (#19); once a second.
        if now >= heard_at {
            heard_at = now + Duration::from_secs(1);
            if let Some(gateway) = &w.gateway {
                gateway.set_heard(
                    arrivals
                        .iter_mut()
                        .filter(|(speaker, _)| !speaker.starts_with("whisper:"))
                        .filter_map(|(speaker, arrival)| Some((speaker.clone(), arrival.quality(now)?)))
                        .collect(),
                );
            }
        }
        crate::voice_priority::publish(voice_first.observe(incoming, now));
        let priority_users = w
            .controls
            .priority_users
            .lock()
            .map(|users| users.clone())
            .unwrap_or_default();
        let priority_active = w.controls.priority_enabled.load(Ordering::Relaxed)
            && w.controls.speaking.lock().is_ok_and(|speaking| {
                speaking.iter().any(|(id, at)| {
                    priority_users.contains(id)
                        && now.saturating_duration_since(*at) < Duration::from_millis(250)
                })
            });
        if playout.due(now, &speakers, jitter_ms) {
            let mut mixed = [0.0f32; 120];
            let mut active = false;
            for (speaker_key, speaker) in speakers.iter_mut() {
                let speaker_id = speaker_key.strip_prefix("whisper:").unwrap_or(speaker_key);
                let mut decoded = [0.0f32; 120];
                if speaker.render_quantum(now, &mut decoded, jitter_ms) {
                    active = true;
                    let power = decoded.iter().map(|v| v * v).sum::<f32>() / decoded.len() as f32;
                    if power > 0.0001 {
                        if let Ok(mut speaking) = w.controls.speaking.lock() {
                            speaking.insert(speaker_id.to_owned(), now);
                        }
                    }
                    if received_audio_activity(power) {
                        if let Ok(mut heard) = w.controls.heard.lock() {
                            heard.insert(speaker_id.to_owned(), now);
                        }
                    }
                    let volume = w
                        .controls
                        .peer_volumes
                        .lock()
                        .ok()
                        .and_then(|v| v.get(speaker_id).copied())
                        .unwrap_or(1.0);
                    let volume = if priority_active && !priority_users.contains(speaker_id) {
                        volume * f32::from_bits(w.controls.priority_volume.load(Ordering::Relaxed))
                    } else {
                        volume
                    };
                    for (out, input) in mixed.iter_mut().zip(decoded) {
                        *out += input * volume;
                    }
                }
                w.counters
                    .late
                    .fetch_add(speaker.stats.late, Ordering::Relaxed);
                w.counters
                    .reordered
                    .fetch_add(speaker.stats.reordered, Ordering::Relaxed);
                w.counters
                    .concealed
                    .fetch_add(speaker.stats.concealed, Ordering::Relaxed);
                w.counters
                    .held
                    .fetch_add(speaker.stats.held, Ordering::Relaxed);
                w.counters
                    .quiet_skips
                    .fetch_add(speaker.stats.quiet_skips, Ordering::Relaxed);
                speaker.stats = Default::default();
            }
            if let Ok(mut speaking) = w.controls.speaking.lock() {
                speaking.retain(|id, at| {
                    (speakers.contains_key(id) || speakers.contains_key(&format!("whisper:{id}")))
                        && now.saturating_duration_since(*at) < Duration::from_millis(250)
                });
            }
            if let Ok(mut output) = w.playback.lock() {
                if w.deafened.load(Ordering::Relaxed) {
                    output.clear();
                } else if active
                    && append_playback_frame(
                        &mut output,
                        &mixed,
                        w.playback_limit.load(Ordering::Relaxed),
                    )
                {
                    w.counters.playback_drop.fetch_add(1, Ordering::Relaxed);
                }
            }
            playout.complete_interval(now, active, Duration::from_micros(2500));
        }
        speakers.retain(|_, speaker| {
            now.duration_since(speaker.last_received) < Duration::from_secs(3)
        });
        if now.duration_since(last_keepalive) >= Duration::from_secs(1) {
            send_packet(&w, &mut sequence, &[0]);
            send_tunnel_control(&w, &mut sequence, &[0]);
            last_keepalive = now;
        }
        let at = Instant::now();
        let packet_samples = w.controls.packet_samples.load(Ordering::Relaxed) as usize;
        let processing_samples = capture_processing_samples(packet_samples, &w.controls);
        let capture_ready = capture_send_allowed(&w)
            && !w.muted.load(Ordering::Relaxed)
            && !w.deafened.load(Ordering::Relaxed)
            && w.capture
                .try_lock()
                .is_ok_and(|samples| samples.len() >= processing_samples);
        if !capture_ready {
            let deadline = playout.next.or_else(|| {
                speakers
                    .values()
                    .filter_map(|speaker| speaker.next_playout_deadline(at, jitter_ms))
                    .min()
            });
            let remaining = deadline
                .map_or(Duration::from_millis(10), |deadline| {
                    deadline.saturating_duration_since(at)
                })
                .min(next_ptt_poll.saturating_duration_since(at))
                .min(Duration::from_millis(10));
            wait.wait(remaining);
        }
    }
    w.counters.stop_udp_link();
    w.counters.gateway_available.store(false, Ordering::Release);
    w.counters.gateway_active.store(false, Ordering::Release);
    w.counters.direct_active.store(false, Ordering::Release);
    w.counters.direct_rtt_us.store(0, Ordering::Relaxed);
    if let Ok(mut direct) = w.controls.direct.lock() {
        direct.take();
    }
    w.counters.tcp_active.store(false, Ordering::Release);
}

fn poll_push_to_talk(controls: &AudioControls) {
    let key = controls.ptt_key.load(Ordering::Acquire);
    #[cfg(target_os = "windows")]
    {
        let pressed = controls.whisper_enabled.load(Ordering::Acquire)
            && unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(0x77) } < 0;
        if controls.whisper_pressed.swap(pressed, Ordering::AcqRel) != pressed {
            controls.privacy_generation.fetch_add(1, Ordering::AcqRel);
        }
    }
    if key == 0 {
        return;
    }
    #[cfg(target_os = "windows")]
    {
        // A bounded key-state query runs only while the voice worker exists. No hook.
        let pressed =
            unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(key) } < 0;
        if controls.ptt_pressed.swap(pressed, Ordering::AcqRel) != pressed {
            controls.privacy_generation.fetch_add(1, Ordering::AcqRel);
        }
    }
    #[cfg(not(target_os = "windows"))]
    controls.ptt_pressed.store(false, Ordering::Release);
}

fn discard_encoder_history(encoder: &mut Encoder, errors: &ErrorSlot, stop: &AtomicBool) {
    if let Err(error) = encoder.reset_state() {
        record_error(errors, format!("Reset private voice history: {error}"));
        stop.store(true, Ordering::Release);
    }
}

fn gateway_tick(w: &VoiceWorker, sequence: &mut u64, now: Instant) -> bool {
    if w.stop.load(Ordering::Acquire) { return false; }
    let Some(gateway) = &w.gateway else { return false; };
    for (address, probe) in gateway.mesh_probes(now) {
        let _ = w.socket.send_to(&probe, address);
    }
    gateway.set_host_rtt_us(w.counters.udp_rtt_us.load(Ordering::Relaxed));
    gateway.with_path(|path| {
        if let Some(packet) = path.hop_probe(now) {
            let _ = w.socket.send_to(&packet, path.address);
        }
        if let Some(probe) = path.server_challenge(now) {
            // Reserve the ordinary session nonce even when the candidate fails.
            if let Some(next) = sequence.checked_add(1) {
                let reserved = *sequence;
                *sequence = next;
                if let Some(packet) = seal_packet(&w.cipher, w.session_id, reserved, 0, &probe)
                    .and_then(|packet| path.wrap(&packet, now))
                {
                    let _ = w.socket.send_to(&packet, path.address);
                }
            }
        }
    });
    let tcp = w.counters.tcp_active.load(Ordering::Acquire) && !w.counters.udp_reachable();
    let previous = if w.counters.direct_active.load(Ordering::Acquire) {
        w.counters.direct_rtt_us.load(Ordering::Relaxed)
    } else if tcp {
        w.counters.tcp_rtt_us.load(Ordering::Relaxed)
    } else {
        w.counters.udp_rtt_us.load(Ordering::Relaxed)
    };
    let failed = !tcp && w.counters.udp_link.load(Ordering::Acquire) & 2 != 0;
    let (active, changed) = gateway.select(now, previous, failed);
    w.counters.gateway_active.store(active, Ordering::Release);
    let sample = gateway.with_path(|path| (path.server_rtt_us(), path.server_measured_at()));
    w.counters.gateway_rtt_us.store(sample.and_then(|s| s.0).unwrap_or(0), Ordering::Relaxed);
    w.counters.gateway_rtt_at.store(sample.and_then(|s| s.1).map(|at| w.counters.clock.tick(at)).unwrap_or(0), Ordering::Release);
    if changed {
        // Both selection and fallback must bind the new path before its first
        // audio. A failed admission never changes the server's active binding.
        send_packet(w, sequence, &[0]);
        if !active && !w.counters.udp_reachable() {
            send_tunnel_control(w, sequence, &[0]);
        }
    }
    changed
}

fn send_tunnel_control(w: &VoiceWorker, sequence: &mut u64, plaintext: &[u8]) -> bool {
    if w.stop.load(Ordering::Acquire) || !w.controls.tcp_allowed()
        || w.counters.gateway_active.load(Ordering::Acquire) {
        return false;
    }
    let Ok(tunnel) = w.controls.tunnel.lock() else {
        return false;
    };
    let Some(tunnel) = tunnel.as_ref().filter(|tunnel| tunnel.ready()) else {
        return false;
    };
    let Some(next) = sequence.checked_add(1) else {
        return false;
    };
    let reserved = *sequence;
    *sequence = next;
    let sent = seal_packet(&w.cipher, w.session_id, reserved, 0, plaintext)
        .is_some_and(|packet| tunnel.send(packet, None));
    if sent {
        w.counters.sent.fetch_add(1, Ordering::Relaxed);
    }
    sent
}
fn send_packet(w: &VoiceWorker, sequence: &mut u64, plaintext: &[u8]) -> bool {
    send_packet_at(
        w,
        sequence,
        plaintext,
        w.controls.privacy_generation.load(Ordering::Acquire),
    )
}
fn send_packet_at(w: &VoiceWorker, sequence: &mut u64, plaintext: &[u8], generation: u64) -> bool {
    let audio = matches!(
        plaintext.first().copied(),
        Some(1 | AUDIO_PACKET_V2 | AUDIO_PACKET_V3 | AUDIO_WHISPER)
    );
    if audio && w.controls.privacy_generation.load(Ordering::Acquire) != generation {
        return false;
    }
    if !packet_send_allowed(w, plaintext) {
        return false;
    }
    if *sequence == u64::MAX {
        record_error(
            &w.errors,
            "Voice session must be rejoined (packet counter exhausted)".into(),
        );
        w.stop.store(true, Ordering::Release);
        return false;
    }
    // Reserve before encryption; even a subsequently muted/discarded packet never
    // reuses its AEAD nonce for a later frame or keepalive under the same ticket.
    let reserved_sequence = *sequence;
    *sequence += 1;
    if plaintext.first() == Some(&AUDIO_PACKET_V3)
        && w.controls.direct_enabled.load(Ordering::Acquire)
        && !w.counters.gateway_active.load(Ordering::Acquire)
        && w.counters.udp_reachable()
    {
        let direct = w.controls.direct.lock().ok().and_then(|mut route| {
            route.as_mut()?.audio(
                plaintext,
                w.counters.udp_rtt_us.load(Ordering::Relaxed),
                Instant::now(),
            )
        });
        if let Some((address, packet)) = direct {
            if !packet_send_allowed(w, plaintext)
                || w.controls.privacy_generation.load(Ordering::Acquire) != generation
                || !w.controls.direct_enabled.load(Ordering::Acquire)
            {
                return false;
            }
            if w.socket.send_to(&packet, address).is_ok() {
                w.counters.sent.fetch_add(1, Ordering::Relaxed);
                #[cfg(test)]
                if audio {
                    w.counters
                        .sent_microphone_audio
                        .fetch_add(1, Ordering::Relaxed);
                }
                return true;
            }
        }
    }
    if let Some(packet) = seal_packet(&w.cipher, w.session_id, reserved_sequence, 0, plaintext) {
        if !packet_send_allowed(w, plaintext)
            || (audio && w.controls.privacy_generation.load(Ordering::Acquire) != generation)
        {
            return false;
        }
        if let Some(gateway) = &w.gateway {
            // The voice mesh between homes gets the same protected frame,
            // unchanged; the host still gets its copy and skips only the
            // people the mesh verifiably carries.
            for (address, packet) in gateway.mesh_frame(plaintext, Instant::now()) {
                let _ = w.socket.send_to(&packet, address);
            }
            match gateway.wrap_selected(&packet, Instant::now()) {
                Ok(Some((address, packet))) => {
                    if !packet_send_allowed(w, plaintext)
                        || (audio && w.controls.privacy_generation.load(Ordering::Acquire) != generation)
                    { return false; }
                    let sent = w.socket.send_to(&packet, address).is_ok();
                    if sent {
                        w.counters.sent.fetch_add(1, Ordering::Relaxed);
                        #[cfg(test)]
                        if audio { w.counters.sent_microphone_audio.fetch_add(1, Ordering::Relaxed); }
                    }
                    return sent;
                }
                Err(()) => return false,
                Ok(None) => {}
            }
        }
        if !matches!(plaintext.first().copied(), Some(0 | UDP_PING))
            && !w.counters.udp_reachable()
            && w.controls.tcp_allowed()
        {
            if let Ok(tunnel) = w.controls.tunnel.lock() {
                if let Some(tunnel) = tunnel.as_ref().filter(|tunnel| tunnel.ready()) {
                    let audio = matches!(
                        plaintext.first().copied(),
                        Some(1 | AUDIO_PACKET_V2 | AUDIO_PACKET_V3 | AUDIO_WHISPER)
                    );
                    let sent = tunnel.send(packet, audio.then_some(generation));
                    if sent {
                        w.counters.sent.fetch_add(1, Ordering::Relaxed);
                        #[cfg(test)]
                        if audio {
                            w.counters
                                .sent_microphone_audio
                                .fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    return sent;
                }
            }
        }
        match w.socket.send_to(&packet, w.remote) {
            Ok(_) => {
                w.counters.sent.fetch_add(1, Ordering::Relaxed);
                #[cfg(test)]
                if audio {
                    w.counters
                        .sent_microphone_audio
                        .fetch_add(1, Ordering::Relaxed);
                }
                return true;
            }
            Err(e) if voice_io_retryable(&e) => {}
            Err(e) => record_error(&w.errors, format!("Voice send failed: {e}")),
        }
    }
    false
}

fn capture_send_allowed(w: &VoiceWorker) -> bool {
    !w.stop.load(Ordering::Acquire)
        && !w.muted.load(Ordering::Acquire)
        && !w.deafened.load(Ordering::Acquire)
        && (w.controls.transmission.load(Ordering::Relaxed) != TransmissionMode::PushToTalk as u32
            || w.controls.ptt_pressed.load(Ordering::Acquire)
            || (w.controls.whisper_enabled.load(Ordering::Acquire)
                && w.controls.whisper_pressed.load(Ordering::Acquire)))
}

fn voice_io_retryable(error: &std::io::Error) -> bool {
    // A connected UDP socket on Windows reports server shutdown as ICMP reset.
    // Keep the existing authenticated-probe deadline; do not turn this into a
    // user Leave before the network worker can observe the process replacement.
    matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::Interrupted
            | std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionRefused
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::NetworkUnreachable
            | std::io::ErrorKind::HostUnreachable
    )
}
fn packet_send_allowed(w: &VoiceWorker, plaintext: &[u8]) -> bool {
    !w.stop.load(Ordering::Acquire)
        && (!matches!(
            plaintext.first().copied(),
            Some(1 | AUDIO_PACKET_V2 | AUDIO_PACKET_V3)
        ) || !(w.controls.whisper_enabled.load(Ordering::Acquire)
            && w.controls.whisper_pressed.load(Ordering::Acquire)))
        && (plaintext.first() != Some(&AUDIO_WHISPER)
            || (w.controls.whisper_enabled.load(Ordering::Acquire)
                && w.controls.whisper_pressed.load(Ordering::Acquire)
                && relay_protocol::unwrap_whisper(plaintext).is_some_and(|(peer, _)| {
                    w.controls
                        .whisper_target
                        .lock()
                        .is_ok_and(|target| target.iter().any(|target| target == peer))
                })))
        && (!matches!(
            plaintext.first().copied(),
            Some(1 | AUDIO_PACKET_V2 | AUDIO_PACKET_V3 | AUDIO_WHISPER)
        ) || (!w.muted.load(Ordering::Acquire)
            && !w.deafened.load(Ordering::Acquire)
            && (w.controls.transmission.load(Ordering::Relaxed)
                != TransmissionMode::PushToTalk as u32
                || w.controls.ptt_pressed.load(Ordering::Acquire)
                || (w.controls.whisper_enabled.load(Ordering::Acquire)
                    && w.controls.whisper_pressed.load(Ordering::Acquire)))))
}

fn packet_nonce(direction: u32, sequence: u64) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce[..4].copy_from_slice(&direction.to_le_bytes());
    nonce[4..].copy_from_slice(&sequence.to_le_bytes());
    nonce
}
fn seal_packet(
    cipher: &ChaCha20Poly1305,
    session: u64,
    sequence: u64,
    direction: u32,
    plaintext: &[u8],
) -> Option<Vec<u8>> {
    let mut header = [0u8; UDP_HEADER_LEN];
    header[..8].copy_from_slice(&session.to_le_bytes());
    header[8..].copy_from_slice(&sequence.to_le_bytes());
    let nonce = packet_nonce(direction, sequence);
    let ciphertext = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad: &header,
            },
        )
        .ok()?;
    let mut packet = Vec::from(header);
    packet.extend(ciphertext);
    (packet.len() <= UDP_MAX_PACKET).then_some(packet)
}
fn open_packet(
    cipher: &ChaCha20Poly1305,
    session: u64,
    direction: u32,
    packet: &[u8],
) -> Option<(u64, Vec<u8>)> {
    if packet.len() < UDP_HEADER_LEN + 17 || packet.len() > UDP_MAX_PACKET {
        return None;
    }
    if u64::from_le_bytes(packet[..8].try_into().ok()?) != session {
        return None;
    }
    let sequence = u64::from_le_bytes(packet[8..16].try_into().ok()?);
    let nonce = packet_nonce(direction, sequence);
    let plain = cipher
        .decrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: &packet[UDP_HEADER_LEN..],
                aad: &packet[..UDP_HEADER_LEN],
            },
        )
        .ok()?;
    Some((sequence, plain))
}

#[derive(Default)]
struct ReplayWindow {
    highest: u64,
    mask: u128,
}
impl ReplayWindow {
    fn accept(&mut self, sequence: u64) -> bool {
        if sequence == 0 {
            return false;
        }
        if sequence > self.highest {
            let shift = sequence - self.highest;
            self.mask = if shift >= 128 {
                1
            } else {
                (self.mask << shift) | 1
            };
            self.highest = sequence;
            true
        } else {
            let behind = self.highest - sequence;
            if behind >= 128 || self.mask & (1u128 << behind) != 0 {
                return false;
            }
            self.mask |= 1u128 << behind;
            true
        }
    }
}

// Read by the delay tests and docs/send-delay-budget-20261028.md; the settings
// page still shows voice_delay_budget().
#[cfg_attr(not(test), allow(dead_code))]
/// The Windows shared-mode default period, and what most devices negotiate.
const DEFAULT_CAPTURE_PERIOD_MS: f32 = 10.0;

#[cfg_attr(not(test), allow(dead_code))]
/// Send-side delay from the capture callback to the packet leaving, part by
/// part, in milliseconds. Built on [`AudioSettings::voice_delay_budget`] (the
/// settings page's figures) and adds the parts that budget leaves implicit.
/// Algorithmic waits only: thread wakeups and the device's own buffering
/// before the callback are measured separately.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct DelayBudget {
    /// How long a whole block can wait beyond its own length because device
    /// callbacks do not line up with it.
    pub capture_period: f32,
    /// Conversion to 48 kHz: none at 48 kHz, the sinc filter otherwise.
    pub resample: f32,
    /// AEC3 runs on the current 10 ms block in place.
    pub aec: f32,
    /// RNNoise's overlap-add synthesis returns each frame one frame late.
    pub suppression: f32,
    /// Click suppression works on the current block only.
    pub clicks: f32,
    /// The attack is a fade on the current block, never a lookahead.
    pub gate_attack: f32,
    /// Opus LowDelay lookahead, as the linked encoder reports it.
    pub encoder_lookahead: f32,
    /// The oldest sample of a packet waits for its newest one.
    pub packetization: f32,
    /// Only the first packet of a talkspurt: the retained speech start is sent
    /// with it, so that packet's oldest audio is one block older.
    pub speech_start: f32,
}

#[cfg_attr(not(test), allow(dead_code))]
impl DelayBudget {
    /// Steady speech: every part that applies to every packet.
    pub(crate) fn total(&self) -> f32 {
        self.capture_period
            + self.resample
            + self.aec
            + self.suppression
            + self.clicks
            + self.gate_attack
            + self.encoder_lookahead
            + self.packetization
    }

    /// The first packet after the voice gate opens.
    pub(crate) fn first_packet_total(&self) -> f32 {
        self.total() + self.speech_start
    }
}

#[cfg_attr(not(test), allow(dead_code))]
/// The send budget for these settings on a 48 kHz device with the Windows
/// default 10 ms period.
pub(crate) fn send_delay_budget_ms(settings: &AudioSettings) -> DelayBudget {
    send_delay_budget_for_capture(settings, AUDIO_SAMPLE_RATE, DEFAULT_CAPTURE_PERIOD_MS)
}

#[cfg_attr(not(test), allow(dead_code))]
/// The send budget for a capture device running at `sample_rate` with a
/// callback every `period_ms`.
pub(crate) fn send_delay_budget_for_capture(
    settings: &AudioSettings,
    sample_rate: u32,
    period_ms: f32,
) -> DelayBudget {
    let settings = settings.clone().normalized();
    let shown = settings.voice_delay_budget();
    let packet_samples = settings.packet_samples();
    let needs_dsp = settings.noise_suppression;
    #[cfg(feature = "native-rtc")]
    let needs_dsp = needs_dsp || settings.echo_cancellation;
    let block = if needs_dsp {
        packet_samples.max(480)
    } else {
        packet_samples
    };
    let period = (period_ms * AUDIO_SAMPLE_RATE as f32 / 1000.0).round() as usize;
    let capture_period = if sample_rate != AUDIO_SAMPLE_RATE {
        // Resampled callbacks never line up with a block: up to a period more.
        period_ms
    } else if period == 0 {
        0.0
    } else {
        // Block k is complete with the callback that delivers its last sample.
        // Over all k the longest wait past the block's end is period - gcd.
        let (mut a, mut b) = (period, block);
        while b != 0 {
            (a, b) = (b, a % b);
        }
        (period - a) as f32 * 1000.0 / AUDIO_SAMPLE_RATE as f32
    };
    let resample = if sample_rate == AUDIO_SAMPLE_RATE {
        0.0
    } else {
        // Half the sinc filter plus the one native sample interpolation needs.
        (crate::audio_resample::TAPS / 2 + 1) as f32 * 1000.0 / sample_rate as f32
    };
    DelayBudget {
        capture_period,
        resample,
        aec: 0.0,
        suppression: shown.suppression_ms as f32,
        clicks: 0.0,
        gate_attack: 0.0,
        encoder_lookahead: shown.codec_ms.unwrap_or(0.0),
        // A DSP block longer than the packet is sent as several packets at once.
        packetization: block as f32 * 1000.0 / AUDIO_SAMPLE_RATE as f32,
        speech_start: shown.speech_start_ms,
    }
}

pub use crate::capture::CaptureFrame;

// Opt-in local measurement of the production voice encryption (R4.5).
#[cfg(test)]
#[path = "media_crypto_cost_tests.rs"]
mod crypto_cost_tests;

#[cfg(test)]
mod tests {
    use super::*;
    include!("media_recovery_tests.rs");

    #[test]
    fn small_device_buffer_respects_driver_limits_and_retries_default_if_rejected() {
        use cpal::{
            BufferSize, SampleFormat, SampleRate, SupportedBufferSize, SupportedStreamConfig,
        };
        assert_eq!(
            preferred_device_buffer(
                48_000,
                &SupportedBufferSize::Range {
                    min: 128,
                    max: 4096
                }
            ),
            BufferSize::Fixed(240)
        );
        assert_eq!(
            preferred_device_buffer(
                44_100,
                &SupportedBufferSize::Range {
                    min: 512,
                    max: 2048
                }
            ),
            BufferSize::Fixed(512)
        );
        assert_eq!(
            preferred_device_buffer(48_000, &SupportedBufferSize::Range { min: 1, max: 256 }),
            BufferSize::Fixed(240)
        );
        assert_eq!(
            preferred_device_buffer(48_000, &SupportedBufferSize::Unknown),
            BufferSize::Default
        );
        let supported = SupportedStreamConfig::new(
            2,
            SampleRate(48_000),
            SupportedBufferSize::Range {
                min: 128,
                max: 4096,
            },
            SampleFormat::F32,
        );
        let mut attempts = vec![];
        let result = build_device_stream(&supported, |config| {
            attempts.push(config.buffer_size);
            if config.buffer_size == BufferSize::Default {
                Ok(())
            } else {
                Err("driver rejected small buffer".into())
            }
        });
        assert!(result.is_ok());
        assert_eq!(attempts, [BufferSize::Fixed(240), BufferSize::Default]);
        let mut calls = 0;
        let result: Result<(), String> = build_device_stream(&supported, |_| {
            calls += 1;
            Err("device unavailable".into())
        });
        assert_eq!(result.unwrap_err(), "device unavailable");
        assert_eq!(calls, 2);
    }

    #[test]
    fn large_output_demand_is_bounded_and_recovers_to_recent_audio() {
        for rate in [44_100_u32, 48_000, 96_000] {
            let limit = Arc::new(AtomicUsize::new(MAX_PLAYBACK_SAMPLES));
            let counters = Arc::new(Counters::default());
            let queue = sample_queue(VecDeque::new());
            let controls = Arc::new(AudioControls::new(&AudioSettings::default()));
            let config = cpal::StreamConfig {
                channels: 1,
                sample_rate: cpal::SampleRate(rate),
                buffer_size: cpal::BufferSize::Default,
            };
            let mut output = output_callback::<f32>(
                &config,
                OutputState {
                    playback_limit: limit.clone(),
                    counters: counters.clone(),
                    queue: queue.clone(),
                    deafened: Arc::new(AtomicBool::new(false)),
                    errors: Arc::new(Mutex::new(None)),
                    controls,
                    tones: Arc::new(Mutex::new(VecDeque::new())),
                    stop: Arc::new(AtomicBool::new(false)),
                },
            );
            let allocated = queue.lock().unwrap().capacity();
            assert!(allocated >= MAX_DEVICE_PLAYBACK_SAMPLES);
            output(&mut vec![0.0; rate as usize / 200]);
            assert_eq!(limit.load(Ordering::Relaxed), MAX_PLAYBACK_SAMPLES);
            output(&mut vec![0.0; rate as usize / 10]);
            let large_limit = limit.load(Ordering::Relaxed);
            assert!((5160..=5161).contains(&large_limit));
            {
                let mut samples = queue.lock().unwrap();
                samples.extend(std::iter::repeat_n(-0.25, large_limit - 480));
                samples.extend([0.25; 480]);
            }
            let mut recent = vec![0.0; rate as usize / 200];
            output(&mut recent);
            assert!(
                queue.lock().unwrap().len() <= 241,
                "Large catch-up must not retain100ms of old speech"
            );
            assert_eq!(counters.playback_drop.load(Ordering::Relaxed), 1);
            assert!(recent.iter().skip(70).all(|v| *v > 0.2));
            output(&mut vec![0.0; rate as usize]);
            assert_eq!(limit.load(Ordering::Relaxed), MAX_DEVICE_PLAYBACK_SAMPLES);
            assert_eq!(
                queue.lock().unwrap().capacity(),
                allocated,
                "No callback grows the voice allocation"
            );
        }
    }

    #[test]
    fn playback_after_a_stall_keeps_recent_audio_instead_of_delayed_history() {
        let mut output = VecDeque::new();
        assert!(!append_playback_frame(
            &mut output,
            &[1.0; AUDIO_FRAME_SAMPLES],
            MAX_PLAYBACK_SAMPLES
        ));
        assert!(!append_playback_frame(
            &mut output,
            &[2.0; AUDIO_FRAME_SAMPLES],
            MAX_PLAYBACK_SAMPLES
        ));
        for value in 3..=10 {
            assert!(append_playback_frame(
                &mut output,
                &[value as f32; AUDIO_FRAME_SAMPLES],
                MAX_PLAYBACK_SAMPLES
            ));
        }
        let first: Vec<_> = output.drain(..AUDIO_FRAME_SAMPLES).collect();
        assert!(first.iter().all(|value| *value == 9.0));
        assert_eq!(output.len(), AUDIO_FRAME_SAMPLES);
        assert!(output.iter().all(|value| *value == 10.0));
    }

    #[test]
    fn larger_fallback_capture_callbacks_remain_complete_without_an_unbounded_queue() {
        assert_eq!(capture_sample_limit(480, 48_000), 1920);
        assert_eq!(capture_sample_limit(441, 44_100), 1920);
        let limit = capture_sample_limit(4800, 48_000);
        assert_eq!(limit, 4800);
        let mut resampler = InputResampler::new(48_000);
        let mut queue = VecDeque::with_capacity(MAX_CAPTURE_SAMPLES);
        let mut dropped = 0;
        for sample in 0..4800 {
            dropped += resampler.feed_bounded(sample as f32, &mut queue, limit);
        }
        assert_eq!(dropped, 0);
        assert!(queue.len() >= 4799);
        assert_eq!(capture_sample_limit(usize::MAX, 1), MAX_CAPTURE_SAMPLES);
    }

    fn capture_test_state() -> InputState {
        InputState {
            queue: sample_queue(VecDeque::new()),
            level: Arc::new(AtomicU32::new(0)),
            muted: Arc::new(AtomicBool::new(false)),
            deafened: Arc::new(AtomicBool::new(false)),
            counters: Arc::new(Counters::default()),
            errors: Arc::new(Mutex::new(None)),
            controls: Arc::new(AudioControls::new(&AudioSettings::default())),
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    #[test]
    fn capture_at_48khz_completes_each_10ms_block_in_the_callback_that_delivers_it() {
        // A 10 ms device period delivers exactly one processing block. Holding
        // back its last sample would leave the block one sample short until
        // the next callback: a whole extra capture period of delay.
        let state = capture_test_state();
        let config = cpal::StreamConfig {
            channels: 2,
            sample_rate: cpal::SampleRate(48_000),
            buffer_size: cpal::BufferSize::Default,
        };
        let mut capture = input_callback::<f32>(&config, state.clone());
        for callback in 1..=3 {
            capture(&[0.25; 960]);
            assert_eq!(
                state.queue.lock().unwrap().len(),
                480 * callback,
                "callback {callback} must leave its whole 10 ms in the queue"
            );
        }
    }

    #[test]
    fn send_delay_budget_sums_its_parts_for_default_settings() {
        let settings = AudioSettings::default();
        let budget = send_delay_budget_ms(&settings);
        println!("default send delay budget (ms): {budget:#?}");
        println!(
            "steady {:.2} ms, first packet of a talkspurt {:.2} ms",
            budget.total(),
            budget.first_packet_total()
        );
        let parts = [
            budget.capture_period,
            budget.resample,
            budget.aec,
            budget.suppression,
            budget.clicks,
            budget.gate_attack,
            budget.encoder_lookahead,
            budget.packetization,
        ];
        assert_eq!(budget.total(), parts.iter().sum::<f32>());
        assert_eq!(budget.first_packet_total(), budget.total() + budget.speech_start);
        // The default 10 ms Windows period lines up with the 10 ms block.
        assert_eq!(budget.capture_period, 0.0);
        assert_eq!(budget.resample, 0.0, "48 kHz capture passes straight through");
        assert_eq!(budget.aec, 0.0, "echo cancellation is off by default");
        assert_eq!(budget.suppression, 10.0, "RNNoise overlap-add holds one frame");
        assert_eq!(budget.clicks, 0.0);
        assert_eq!(budget.gate_attack, 0.0, "the attack fades in, it never looks ahead");
        assert_eq!(budget.encoder_lookahead, 2.5);
        assert_eq!(budget.packetization, 10.0);
        assert_eq!(budget.speech_start, 10.0);
        assert_eq!(budget.total(), 22.5);
        assert!(budget.first_packet_total() <= 35.0);
        // It extends the settings page's budget rather than competing with it.
        let shown = settings.voice_delay_budget();
        assert_eq!(budget.suppression, shown.suppression_ms as f32);
        assert_eq!(Some(budget.encoder_lookahead), shown.codec_ms);
        assert_eq!(budget.speech_start, shown.speech_start_ms);
    }

    #[test]
    fn send_delay_budget_counts_a_capture_period_that_does_not_fit_the_block() {
        let settings = AudioSettings::default();
        let at = |rate, period| send_delay_budget_for_capture(&settings, rate, period);
        assert_eq!(at(48_000, 10.0).capture_period, 0.0);
        assert_eq!(at(48_000, 5.0).capture_period, 0.0);
        assert_eq!(at(48_000, 2.5).capture_period, 0.0);
        // 3 ms (144 frames) and 480 share 48 frames: a block can wait 2 ms more.
        assert_eq!(at(48_000, 3.0).capture_period, 2.0);
        // The measured Scarlett Solo: 7 ms (336 frames), up to 6 ms more.
        let measured = at(48_000, 7.0);
        assert_eq!(measured.capture_period, 6.0);
        assert_eq!(measured.total(), 28.5);
        assert_eq!(measured.first_packet_total(), 38.5);
        // A 20 ms period delivers two blocks at once; the first waited 10 ms.
        assert_eq!(at(48_000, 20.0).capture_period, 10.0);
        // Resampled capture never lines up with the block; count the whole period.
        let resampled = at(44_100, 10.0);
        assert_eq!(resampled.capture_period, 10.0);
        let filter = (crate::audio_resample::TAPS / 2 + 1) as f32 * 1000.0 / 44_100.0;
        assert!((resampled.resample - filter).abs() < 1e-4);
        let mut off = settings.clone();
        off.noise_suppression = false;
        let plain = send_delay_budget_ms(&off);
        assert_eq!(plain.suppression, 0.0);
        assert_eq!(plain.total(), 12.5);
    }

    #[test]
    fn agc_after_suppression_does_not_raise_the_noise_floor_between_words() {
        use crate::audio_bench::{corpus, run_chain, CurrentChain, SendChain, BLOCK};
        let corpus = corpus();
        let row = |name: &str| corpus.iter().find(|item| item.name == name).unwrap();
        // Chain gain over whole 10 ms frames: what reaches the listener against
        // what the microphone heard, with the chain's own delay taken out.
        let gain_db = |output: &[f32], input: &[f32], frames: &[usize], delay: usize| {
            let (mut out, mut heard) = (0.0f64, 0.0f64);
            for &start in frames {
                for index in start..start + BLOCK {
                    heard += f64::from(input[index]).powi(2);
                    out += f64::from(output[index + delay]).powi(2);
                }
            }
            10.0 * (out.max(1e-20) / heard).log10()
        };
        let energy = |samples: &[f32]| samples.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>();

        let talk = row("keyboard_between_words");
        let noisy = talk.noisy();
        let mut chain = CurrentChain::rnnoise_enhanced();
        let delay = chain.algorithmic_delay_samples();
        let output = run_chain(&mut chain, &noisy).output;
        let frames = (noisy.len() - delay) / BLOCK;
        let clean: Vec<f64> = (0..frames)
            .map(|f| energy(&talk.clean[f * BLOCK..(f + 1) * BLOCK]))
            .collect();
        let peak = clean.iter().copied().fold(0.0, f64::max);
        let speech: Vec<usize> = (0..frames)
            .filter(|f| clean[*f] >= peak * 1e-3)
            .map(|f| f * BLOCK)
            .collect();
        // Pauses that still hold keyboard noise above -70 dBFS.
        let pauses: Vec<usize> = (0..frames)
            .filter(|f| {
                clean[*f] <= peak * 1e-6
                    && energy(&talk.noise[f * BLOCK..(f + 1) * BLOCK]) > 1e-7 * BLOCK as f64
            })
            .map(|f| f * BLOCK)
            .collect();
        assert!(speech.len() > 100 && pauses.len() > 20);
        let speech_gain = gain_db(&output, &noisy, &speech, delay);
        let pause_gain = gain_db(&output, &noisy, &pauses, delay);

        let quiet = row("silence_floor");
        let noisy = quiet.noisy();
        let mut chain = CurrentChain::rnnoise_enhanced();
        let output = run_chain(&mut chain, &noisy).output;
        let all: Vec<usize> = (0..(noisy.len() - delay) / BLOCK).map(|f| f * BLOCK).collect();
        let floor_gain = gain_db(&output, &noisy, &all, delay);
        println!(
            "speech gain {speech_gain:.1} dB, keyboard pauses {pause_gain:.1} dB, fan alone {floor_gain:.1} dB"
        );
        assert!(
            pause_gain <= speech_gain + 3.0,
            "pauses raised to {pause_gain:.1} dB against speech {speech_gain:.1} dB"
        );
        assert!(
            floor_gain <= speech_gain + 3.0,
            "a fan with nobody talking raised to {floor_gain:.1} dB against speech {speech_gain:.1} dB"
        );
    }
    pub(super) fn test_call_keys(local: &str) -> Arc<CallKeys> {
        let sender_keys = [("local", 31_u8), ("remote", 47_u8)]
            .into_iter()
            .map(|(user, byte)| {
                (
                    user.to_owned(),
                    crate::e2ee::SenderKeys {
                        device_id: format!("device-{user}"),
                        opus: zeroize::Zeroizing::new([byte; 32]),
                        rtc: zeroize::Zeroizing::new([byte + 1; 32]),
                    },
                )
            })
            .collect();
        Arc::new(CallKeys {
            group_id: "owned-call-fixture".into(),
            scope: "owned-room".into(),
            epoch: 7,
            local_user_id: local.into(),
            local_device_id: format!("device-{local}"),
            sender_keys,
            voice_replay: Mutex::default(),
        })
    }

    #[test]
    fn whisper_private_scope_cannot_be_opened_by_legitimate_room_participant() {
        fn private_keys(local: &str) -> Arc<CallKeys> {
            let mut keys = test_call_keys(local);
            let keys = Arc::get_mut(&mut keys).unwrap();
            keys.scope = "owned-server/dm/local/remote".into();
            keys.group_id = "owned-private-pair".into();
            for sender in keys.sender_keys.values_mut() {
                sender.opus.fill(71);
            }
            test_private_keys(local, keys)
        }
        fn test_private_keys(local: &str, keys: &CallKeys) -> Arc<CallKeys> {
            let mut result = test_call_keys(local);
            let output = Arc::get_mut(&mut result).unwrap();
            output.scope = keys.scope.clone();
            output.group_id = keys.group_id.clone();
            for sender in output.sender_keys.values_mut() {
                sender.opus.fill(71);
            }
            result
        }
        let sender = CallCipher::new(private_keys("remote")).unwrap();
        let inner = sender.seal(1, 1, 480, true, &playout_packet()).unwrap();
        let packet = relay_protocol::wrap_whisper("local", &inner).unwrap();
        let (target, captured) = relay_protocol::unwrap_whisper(&packet).unwrap();
        assert_eq!(target, "local");
        let mut room_member = CallCipher::new(test_call_keys("local")).unwrap();
        assert!(room_member.open(captured).is_none());
        let mut intended = CallCipher::new(private_keys("local")).unwrap();
        assert_eq!(intended.open(captured).unwrap().user_id, "remote");
        assert!(
            intended.open(captured).is_none(),
            "TCP/UDP cannot replay private speech"
        );
    }
    #[test]
    fn whisper_hold_never_allows_regular_room_audio_or_missing_audience() {
        let remote = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let (worker, _, _) = probe_fixture(remote.local_addr().unwrap());
        worker.muted.store(false, Ordering::Release);
        worker.deafened.store(false, Ordering::Release);
        worker
            .controls
            .whisper_enabled
            .store(true, Ordering::Release);
        worker
            .controls
            .whisper_pressed
            .store(true, Ordering::Release);
        assert!(!packet_send_allowed(&worker, &[AUDIO_PACKET_V3]));
        assert!(packet_send_allowed(&worker, &[0]));
        let inner = worker
            .call
            .seal(1, 1, 480, true, &playout_packet())
            .unwrap();
        let private = relay_protocol::wrap_whisper("remote", &inner).unwrap();
        assert!(!packet_send_allowed(&worker, &private));
        *worker.controls.whisper_target.lock().unwrap() = vec!["remote".into()];
        assert!(packet_send_allowed(&worker, &private));
        worker
            .controls
            .whisper_pressed
            .store(false, Ordering::Release);
        assert!(!packet_send_allowed(&worker, &private));
    }
    #[test]
    fn protected_voice_authenticates_peer_scope_epoch_clock_and_not_relay_routing_id() {
        let remote = CallCipher::new(test_call_keys("remote")).unwrap();
        let mut receiver = CallCipher::new(test_call_keys("local")).unwrap();
        let packet = remote.seal(99, 1, 960, true, &playout_packet()).unwrap();
        for field in [9, 17, 25, 26, 34, 35, 36, 52, 60, packet.len() - 1] {
            let mut tampered = packet.clone();
            tampered[field] ^= 1;
            assert!(
                receiver.open(&tampered).is_none(),
                "unauthenticated field {field}"
            );
        }
        let mut forwarded = packet.clone();
        forwarded[1..9].copy_from_slice(&1234_u64.to_le_bytes());
        let frame = receiver.open(&forwarded).unwrap();
        assert_eq!(frame.user_id, "remote");
        assert_eq!(
            (frame.sequence, frame.timestamp, frame.restart),
            (1, 960, true)
        );
        assert!(
            receiver.open(&packet).is_none(),
            "new outer routing/counters cannot replay inner audio"
        );
        let mut decoder = Decoder::new(AUDIO_SAMPLE_RATE, Channels::Mono).unwrap();
        assert_eq!(
            decoder
                .decode_float(&frame.opus, &mut [0.0; AUDIO_FRAME_SAMPLES], false)
                .unwrap(),
            AUDIO_FRAME_SAMPLES
        );
        for context in ["group", "scope", "epoch", "key", "device"] {
            let mut keys = test_call_keys("local");
            let keys_mut = Arc::get_mut(&mut keys).unwrap();
            match context {
                "group" => keys_mut.group_id.push_str("-other"),
                "scope" => keys_mut.scope.push_str("-other"),
                "epoch" => keys_mut.epoch += 1,
                "key" => keys_mut.sender_keys.get_mut("remote").unwrap().opus[0] ^= 1,
                "device" => keys_mut
                    .sender_keys
                    .get_mut("remote")
                    .unwrap()
                    .device_id
                    .push_str("-other"),
                _ => unreachable!(),
            }
            assert!(
                CallCipher::new(keys).unwrap().open(&packet).is_none(),
                "wrong {context} accepted"
            );
        }
        assert!(
            receiver.open(&[1, 0, 0, 0]).is_none(),
            "no legacy plaintext fallback"
        );
    }
    #[test]
    fn protected_voice_replay_window_allows_reordering_without_accepting_nonce_reuse() {
        let remote = CallCipher::new(test_call_keys("remote")).unwrap();
        let mut receiver = CallCipher::new(test_call_keys("local")).unwrap();
        let packet = |seq| {
            remote
                .seal(99, seq, seq * 960, false, &playout_packet())
                .unwrap()
        };
        let one = packet(1);
        assert!(receiver.open(&one).is_some());
        assert!(receiver.open(&packet(3)).is_some());
        assert!(receiver.open(&packet(2)).is_some());
        assert!(receiver.open(&packet(2)).is_none());
        assert!(receiver.open(&packet(200)).is_some());
        assert!(receiver.open(&packet(4)).is_none());
        assert!(receiver.open(&one).is_none());
        // A new sender socket has a fresh random prefix under the same epoch;
        // retaining the previous prefix prevents replaying its earlier audio.
        let rejoined = CallCipher::new(test_call_keys("remote")).unwrap();
        assert_ne!(remote.nonce_prefix, rejoined.nonce_prefix);
        assert!(receiver
            .open(&rejoined.seal(100, 1, 0, true, &playout_packet()).unwrap())
            .is_some());
        assert!(receiver.open(&one).is_none());
        assert!(
            receiver.open(&packet(201)).is_none(),
            "retired sender instance cannot resume after rejoin"
        );
    }

    #[test]
    fn protected_voice_receiver_restart_keeps_accepted_and_retired_prefixes() {
        let keys = test_call_keys("local");
        let sender = CallCipher::new(test_call_keys("remote")).unwrap();
        let first = sender.seal(99, 1, 0, true, &playout_packet()).unwrap();
        let mut receiver = CallCipher::new(keys.clone()).unwrap();
        assert!(receiver.open(&first).is_some());
        drop(receiver);
        let mut reopened = CallCipher::new(keys.clone()).unwrap();
        assert!(
            reopened.open(&first).is_none(),
            "reopening a device under the same epoch must not accept recorded audio"
        );
        let next = sender.seal(99, 2, 960, false, &playout_packet()).unwrap();
        assert!(reopened.open(&next).is_some());
        let successor = CallCipher::new(test_call_keys("remote")).unwrap();
        let fresh = successor.seal(100, 1, 0, true, &playout_packet()).unwrap();
        assert!(reopened.open(&fresh).is_some());
        drop(reopened);
        let mut reopened = CallCipher::new(keys).unwrap();
        assert!(reopened.open(&first).is_none());
        assert!(reopened.open(&next).is_none());
        assert!(reopened.open(&fresh).is_none());
        assert!(
            reopened
                .open(&sender.seal(99, 3, 1920, false, &playout_packet()).unwrap())
                .is_none(),
            "a retired sender remains retired after receiver restart"
        );
        assert!(reopened
            .open(
                &successor
                    .seal(100, 2, 960, false, &playout_packet())
                    .unwrap()
            )
            .is_some());
    }

    #[test]
    fn protected_voice_prefix_limit_survives_restart_without_evicting_retired_senders() {
        let keys = test_call_keys("local");
        let mut receiver = CallCipher::new(keys.clone()).unwrap();
        let opus = playout_packet();
        let mut first = None;
        let mut last = None;
        for _ in 0..MAX_SPEAKERS * 4 {
            let sender = CallCipher::new(test_call_keys("remote")).unwrap();
            let packet = sender.seal(99, 1, 0, true, &opus).unwrap();
            assert!(receiver.open(&packet).is_some());
            first.get_or_insert(packet);
            last = Some(sender);
        }
        drop(receiver);
        let mut receiver = CallCipher::new(keys).unwrap();
        assert!(receiver.open(&first.unwrap()).is_none());
        let extra = CallCipher::new(test_call_keys("remote")).unwrap();
        assert!(receiver
            .open(&extra.seal(100, 1, 0, true, &opus).unwrap())
            .is_none());
        assert!(
            receiver
                .open(&last.unwrap().seal(99, 2, 960, false, &opus).unwrap())
                .is_some(),
            "the active sender remains usable at capacity"
        );
    }

    struct DelayedBackend {
        entered: mpsc::Sender<String>,
        release: Receiver<()>,
        published: Arc<AtomicU64>,
        current_stop: Option<Arc<AtomicBool>>,
        _thread_local: std::rc::Rc<()>, // Proves the native owner needs no unsafe Send backend.
    }
    impl AudioBackend for DelayedBackend {
        fn voice(&mut self, request: VoiceStart) -> Result<(), String> {
            self.entered.send(request.host.clone()).unwrap();
            if request.host.starts_with("slow") {
                self.release.recv_timeout(Duration::from_secs(2)).unwrap();
            }
            if request.host == "slow-failure" || request.host == "failure" {
                return Err("late driver failure".into());
            }
            ensure_not_cancelled(&request.stop)?;
            self.current_stop = Some(request.stop);
            self.published.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
        fn reopen_devices(
            &mut self,
            settings: AudioSettings,
            _: u64,
        ) -> Result<DeviceReopen, String> {
            self.entered
                .send(format!(
                    "devices:{}",
                    settings.input_device.as_deref().unwrap_or("default")
                ))
                .unwrap();
            if settings.input_device.as_deref() == Some("slow-device") {
                self.release.recv_timeout(Duration::from_secs(2)).unwrap();
            }
            if settings.input_device.as_deref() == Some("missing-device") {
                return Ok(DeviceReopen::Retained {
                    settings: Box::new(AudioSettings::default()),
                    error: "device unavailable".into(),
                });
            }
            Ok(DeviceReopen::Applied)
        }
        fn stop_voice(&mut self) {
            self.current_stop.take();
        }
        fn microphone(&mut self, request: MicrophoneStart) -> Result<(), String> {
            self.entered.send("microphone".into()).unwrap();
            self.release.recv_timeout(Duration::from_secs(2)).unwrap();
            if request.settings.input_device.as_deref() == Some("failure") {
                return Err("microphone startup failed".into());
            }
            ensure_not_cancelled(&request.stop)?;
            self.current_stop = Some(request.stop);
            self.published.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
        fn stop_microphone(&mut self) {
            self.current_stop.take();
        }
        fn tone(&mut self, _: AudioCue, _: AudioSettings, _: bool) -> Result<(), String> {
            Err("cue output unavailable".into())
        }
        fn stop_sound(&mut self) {}
        fn cleanup_cancelled(&mut self) {
            if self
                .current_stop
                .as_ref()
                .is_some_and(|stop| stop.load(Ordering::Acquire))
            {
                self.current_stop.take();
            }
        }
        fn has_devices(&self) -> bool {
            self.current_stop.is_some()
        }
    }
    fn delayed_audio() -> (
        MediaEngine,
        Receiver<String>,
        mpsc::Sender<()>,
        Arc<AtomicU64>,
    ) {
        let mut engine = MediaEngine::new().unwrap();
        let (sender, receiver) = mpsc::sync_channel(8);
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let published = Arc::new(AtomicU64::new(0));
        let published_clone = published.clone();
        let state = engine.state.clone();
        let pending_devices = engine.pending_devices.clone();
        engine.sender = Some(sender);
        engine.owner = Some(thread::spawn(move || {
            audio_owner(receiver, state, pending_devices, || DelayedBackend {
                entered: entered_tx,
                release: release_rx,
                published: published_clone,
                current_stop: None,
                _thread_local: std::rc::Rc::new(()),
            })
        }));
        (engine, entered_rx, release_tx, published)
    }
    fn voice_ticket() -> VoiceTicket {
        VoiceTicket {
            resume: None,
            resumed_state: None,
            tunnel_url: None,
            session_id: 7,
            wire_version: 3,
            key: STANDARD.encode([17; 32]),
            udp_port: 4581,
        }
    }

    #[test]
    fn device_switch_keeps_call_worker_counters_and_mute_deafen() {
        let (mut engine, entered, _release, published) = delayed_audio();
        engine.set_muted(true);
        engine.set_deafened(true);
        engine
            .start_voice("ready", voice_ticket(), test_call_keys("local"))
            .unwrap();
        assert_eq!(
            entered.recv_timeout(Duration::from_secs(1)).unwrap(),
            "ready"
        );
        let until = Instant::now() + Duration::from_secs(1);
        while !engine.is_active() && Instant::now() < until {
            thread::yield_now();
        }
        assert!(engine.is_active());
        let stop = engine.stop.clone().unwrap();
        let counters = engine.counters.clone();
        let generation = engine.state.lock().unwrap().generation;
        assert!(engine.configure(AudioSettings {
            input_device: Some("headset".into()),
            ..Default::default()
        }));
        engine.reopen_voice_devices().unwrap();
        assert_eq!(
            entered.recv_timeout(Duration::from_secs(1)).unwrap(),
            "devices:headset"
        );
        let until = Instant::now() + Duration::from_secs(1);
        while !engine.is_active() && Instant::now() < until {
            thread::yield_now();
        }
        assert!(engine.is_active());
        assert!(Arc::ptr_eq(&stop, engine.stop.as_ref().unwrap()));
        assert!(Arc::ptr_eq(&counters, &engine.counters));
        assert!(!stop.load(Ordering::Acquire));
        assert_eq!(generation, engine.state.lock().unwrap().generation);
        assert_eq!(
            published.load(Ordering::Relaxed),
            1,
            "no second call/transport start"
        );
        assert!(engine.shared.muted.load(Ordering::Acquire));
        assert!(engine.shared.deafened.load(Ordering::Acquire));
        engine.stop_voice();
    }

    #[test]
    fn rapid_device_choices_coalesce_while_initial_driver_start_is_busy() {
        let (mut engine, entered, release, published) = delayed_audio();
        engine
            .start_voice("slow", voice_ticket(), test_call_keys("local"))
            .unwrap();
        assert_eq!(
            entered.recv_timeout(Duration::from_secs(1)).unwrap(),
            "slow"
        );
        for name in ["obsolete", "latest"] {
            engine.configure(AudioSettings {
                input_device: Some(name.into()),
                ..Default::default()
            });
            engine.reopen_voice_devices().unwrap();
        }
        release.send(()).unwrap();
        assert_eq!(
            entered.recv_timeout(Duration::from_secs(1)).unwrap(),
            "devices:latest"
        );
        assert_eq!(published.load(Ordering::Relaxed), 1);
        engine.stop_voice();
    }

    #[test]
    fn a_missing_device_retains_the_call_and_restores_only_its_selection() {
        let (mut engine, entered, _release, published) = delayed_audio();
        engine
            .start_voice("ready", voice_ticket(), test_call_keys("local"))
            .unwrap();
        assert_eq!(
            entered.recv_timeout(Duration::from_secs(1)).unwrap(),
            "ready"
        );
        let until = Instant::now() + Duration::from_secs(1);
        while !engine.is_active() && Instant::now() < until {
            thread::yield_now();
        }
        engine.configure(AudioSettings {
            input_device: Some("missing-device".into()),
            microphone_gain: 1.8,
            ..Default::default()
        });
        engine.reopen_voice_devices().unwrap();
        assert_eq!(
            entered.recv_timeout(Duration::from_secs(1)).unwrap(),
            "devices:missing-device"
        );
        let until = Instant::now() + Duration::from_secs(1);
        while engine.device_failures.lock().unwrap().is_none() && Instant::now() < until {
            thread::yield_now();
        }
        let failure = engine
            .take_device_change_failure()
            .expect("recoverable device error");
        assert_eq!(failure.error, "device unavailable");
        assert_eq!(engine.active_settings().input_device, None);
        assert_eq!(engine.active_settings().microphone_gain, 1.8);
        assert_eq!(published.load(Ordering::Relaxed), 1);
        assert!(
            engine.take_error().is_none(),
            "fallback is not a terminal call failure"
        );
        engine.stop_voice();
    }

    #[test]
    fn a_late_device_result_cannot_reactivate_a_left_or_replaced_call() {
        let (mut engine, entered, release, published) = delayed_audio();
        engine
            .start_voice("ready", voice_ticket(), test_call_keys("local"))
            .unwrap();
        assert_eq!(
            entered.recv_timeout(Duration::from_secs(1)).unwrap(),
            "ready"
        );
        let until = Instant::now() + Duration::from_secs(1);
        while !engine.is_active() && Instant::now() < until {
            thread::yield_now();
        }
        engine.configure(AudioSettings {
            input_device: Some("slow-device".into()),
            ..Default::default()
        });
        engine.reopen_voice_devices().unwrap();
        assert_eq!(
            entered.recv_timeout(Duration::from_secs(1)).unwrap(),
            "devices:slow-device"
        );
        engine.stop_voice();
        engine
            .start_voice("next", voice_ticket(), test_call_keys("local"))
            .unwrap();
        release.send(()).unwrap();
        assert_eq!(
            entered.recv_timeout(Duration::from_secs(1)).unwrap(),
            "next"
        );
        let until = Instant::now() + Duration::from_secs(1);
        while !engine.is_active() && Instant::now() < until {
            thread::yield_now();
        }
        assert!(engine.is_active());
        assert_eq!(published.load(Ordering::Relaxed), 2);
        assert!(engine.take_device_change_failure().is_none());
        engine.stop_voice();
    }

    #[test]
    fn retired_device_errors_cannot_stop_or_poison_replacement_audio() {
        let controls = AudioControls::new(&AudioSettings::default());
        let retired = Arc::new(Mutex::new(None));
        let active = Arc::new(Mutex::new(None));
        let destination = Arc::new(Mutex::new(None));
        *controls.device_errors.lock().unwrap() = Some((active.clone(), destination.clone()));
        let stop = AtomicBool::new(false);
        record_device_error(&retired, &stop, &controls, true, "late retired error");
        assert!(!stop.load(Ordering::Acquire));
        assert!(destination.lock().unwrap().is_none());
        record_device_error(&active, &stop, &controls, false, "current device failed");
        assert!(stop.load(Ordering::Acquire));
        let first = destination.lock().unwrap().clone();
        record_device_error(&retired, &stop, &controls, true, "another retired error");
        assert_eq!(*destination.lock().unwrap(), first);
    }

    #[test]
    fn playback_or_receive_adjustments_do_not_reset_local_speech_processing() {
        let mut settings = AudioSettings::default();
        let controls = AudioControls::new(&settings);
        let before = controls.privacy_generation.load(Ordering::Acquire);
        settings.playback_volume = 0.6;
        settings.jitter_ms = 20;
        settings.adaptive_jitter = false;
        settings.sound_volume = 0.1;
        controls.apply(&settings);
        assert_eq!(controls.privacy_generation.load(Ordering::Acquire), before);
        assert_eq!(
            f32::from_bits(controls.playback_volume.load(Ordering::Relaxed)),
            0.6
        );
        settings.transmission = TransmissionMode::PushToTalk;
        controls.apply(&settings);
        assert_ne!(controls.privacy_generation.load(Ordering::Acquire), before);
        assert_eq!(
            controls.transmission.load(Ordering::Relaxed),
            TransmissionMode::PushToTalk as u32
        );
    }
    fn wait_active(engine: &MediaEngine) {
        let deadline = Instant::now() + Duration::from_secs(1);
        while !engine.is_active() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(2));
        }
        assert!(engine.is_active());
    }
    #[test]
    fn an_unplugged_device_is_followed_instead_of_ending_the_call() {
        let controls = AudioControls::new(&AudioSettings::default());
        let stop = AtomicBool::new(false);
        let errors: ErrorSlot = Arc::new(Mutex::new(None));
        // Without following, the old behaviour stays: the call stops with a message.
        record_device_error(&errors, &stop, &controls, true, "The requested device is no longer available.");
        assert!(stop.load(Ordering::Acquire));
        let stop = AtomicBool::new(false);
        let errors: ErrorSlot = Arc::new(Mutex::new(None));
        controls.follow_devices.store(true, Ordering::Release);
        record_device_error(&errors, &stop, &controls, false, "Windows audio: unplugged (0x88890004)");
        assert!(!stop.load(Ordering::Acquire), "the call keeps running");
        assert!(errors.lock().unwrap().is_none());
        assert!(controls.device_lost.load(Ordering::Acquire));
        // Other device failures still stop as before.
        record_device_error(&errors, &stop, &controls, true, "format not supported");
        assert!(stop.load(Ordering::Acquire));
    }

    #[test]
    fn following_a_device_change_keeps_the_call_mute_deafen_and_volumes() {
        let mut engine = MediaEngine::new().unwrap();
        let saved = AudioSettings {
            input_device: Some("USB Headset".into()),
            ..Default::default()
        }
        .normalized();
        engine.configure(saved.clone());
        let (sender, receiver) = mpsc::sync_channel(8);
        engine.sender = Some(sender);
        engine
            .start_voice("host", voice_ticket(), test_call_keys("local"))
            .unwrap();
        assert!(matches!(receiver.recv().unwrap(), AudioCommand::Voice { .. }));
        engine.set_muted(true);
        engine.set_deafened(true);
        engine.set_user_volume("friend", 0.4);
        let revision = engine.shared.controls.device_revision.load(Ordering::Acquire);
        // The headset was unplugged: open the Windows default instead.
        let fallback = crate::device_follow::effective(&saved, &[], &[]);
        engine.follow_device_change(fallback).unwrap();
        assert_eq!(engine.device_selection(), (None, None));
        assert!(engine.shared.controls.device_revision.load(Ordering::Acquire) > revision);
        assert!(engine.pending_devices.lock().unwrap().as_ref().is_some_and(|r| r.follow), "only the devices reopen");
        assert!(engine.operation == Some(AudioOperation::Voice), "the call keeps running");
        assert!(engine.shared.muted.load(Ordering::Acquire));
        assert!(engine.shared.deafened.load(Ordering::Acquire));
        assert_eq!(engine.shared.controls.peer_volumes.lock().unwrap().get("friend"), Some(&0.4));
        // A default change with the same names still forces a reopen.
        let revision = engine.shared.controls.device_revision.load(Ordering::Acquire);
        engine.follow_device_change(crate::device_follow::effective(&saved, &[], &[])).unwrap();
        assert!(engine.shared.controls.device_revision.load(Ordering::Acquire) > revision);
    }

    #[test]
    fn microphone_preview_hears_edits_live_and_reopens_only_for_devices() {
        use crate::self_listen::PreviewChange;
        let mut engine = MediaEngine::new().unwrap();
        let applied = AudioSettings::default().normalized();
        engine.configure(applied.clone());
        let (sender, receiver) = mpsc::sync_channel(8);
        engine.sender = Some(sender);
        assert_eq!(engine.update_microphone_preview(&applied).unwrap(), PreviewChange::None);
        let draft = AudioSettings {
            transmission: TransmissionMode::Continuous,
            noise_suppression: true,
            whisper_enabled: true,
            ..applied.clone()
        };
        engine.preview_microphone(draft.clone()).unwrap();
        let first = match receiver.recv().unwrap() {
            AudioCommand::Microphone { request, .. } => request,
            _ => panic!("expected microphone preview"),
        };
        assert!(!first.controls.whisper_enabled.load(Ordering::Acquire));
        assert!(first.controls.local_preview);
        let generation = first.controls.privacy_generation.load(Ordering::Acquire);
        // Dragging gain or threshold must not restart noise suppression.
        let mut edited = draft.clone();
        edited.microphone_gain = 2.5;
        edited.playback_volume = 0.4;
        edited.noise_suppression = false;
        assert_eq!(engine.update_microphone_preview(&edited).unwrap(), PreviewChange::Live);
        assert_eq!(f32::from_bits(first.controls.gain.load(Ordering::Relaxed)), 2.5);
        assert_eq!(f32::from_bits(first.controls.playback_volume.load(Ordering::Relaxed)), 0.4);
        assert!(!first.controls.suppression.load(Ordering::Relaxed));
        assert_eq!(first.controls.privacy_generation.load(Ordering::Acquire), generation);
        assert!(!first.controls.whisper_enabled.load(Ordering::Acquire));
        // A transmission-mode change is a privacy boundary: queued audio is discarded.
        edited.transmission = TransmissionMode::PushToTalk;
        assert_eq!(engine.update_microphone_preview(&edited).unwrap(), PreviewChange::Live);
        assert!(first.controls.privacy_generation.load(Ordering::Acquire) > generation);
        // The push-to-talk key reaches the preview without restarting it, and
        // drops anything queued under the old key.
        let before_key = first.controls.privacy_generation.load(Ordering::Acquire);
        first.controls.ptt_pressed.store(true, Ordering::Release);
        engine.set_push_to_talk_key(0x42);
        assert_eq!(first.controls.ptt_key.load(Ordering::Acquire), 0x42);
        assert!(!first.controls.ptt_pressed.load(Ordering::Acquire));
        assert!(first.controls.privacy_generation.load(Ordering::Acquire) > before_key);
        // The saved call settings are untouched by any of this.
        assert_eq!(engine.settings, applied);
        assert_eq!(f32::from_bits(engine.shared.controls.gain.load(Ordering::Relaxed)), applied.microphone_gain);
        // A new output device reopens only the preview's own streams.
        edited.output_device = Some("USB headset".into());
        assert_eq!(engine.update_microphone_preview(&edited).unwrap(), PreviewChange::Reopen);
        assert!(first.stop.load(Ordering::Acquire));
        assert!(matches!(receiver.recv().unwrap(), AudioCommand::StopMicrophone));
        let second = match receiver.recv().unwrap() {
            AudioCommand::Microphone { request, .. } => request,
            _ => panic!("expected a reopened preview"),
        };
        assert_eq!(second.settings.output_device.as_deref(), Some("USB headset"));
        assert_eq!(f32::from_bits(second.controls.gain.load(Ordering::Relaxed)), 2.5);
        assert_eq!(second.controls.ptt_key.load(Ordering::Acquire), 0x42);
        second.taps.record_raw(0.25);
        let (sample, _, _, held) = engine.microphone_preview_meter().unwrap();
        assert_eq!(sample.raw_peak, 0.25);
        assert!(!held);
        engine.stop_microphone_test();
        assert!(engine.microphone_preview_meter().is_none());
        assert_eq!(engine.update_microphone_preview(&draft).unwrap(), PreviewChange::None);
    }
    #[test]
    fn microphone_preview_uses_draft_without_changing_the_next_call() {
        let mut engine = MediaEngine::new().unwrap();
        let applied = AudioSettings {
            input_device: Some("saved microphone".into()),
            transmission: TransmissionMode::PushToTalk,
            microphone_gain: 1.25,
            ..Default::default()
        }
        .normalized();
        engine.configure(applied.clone());
        engine.set_push_to_talk_key(0x06);
        let draft = AudioSettings {
            input_device: Some("test microphone".into()),
            transmission: TransmissionMode::Continuous,
            microphone_gain: 3.0,
            ..applied.clone()
        };
        // Observe the actual owner requests without ever opening a device.
        let (sender, receiver) = mpsc::sync_channel(8);
        engine.sender = Some(sender);
        engine.preview_microphone(draft.clone()).unwrap();
        let request = match receiver.recv().unwrap() {
            AudioCommand::Microphone { request, .. } => request,
            _ => panic!("expected microphone preview"),
        };
        assert_eq!(request.settings, draft);
        assert_eq!(request.controls.ptt_key.load(Ordering::Acquire), 0x06);
        assert_eq!(engine.settings, applied);
        assert_eq!(
            engine.shared.controls.transmission.load(Ordering::Relaxed),
            TransmissionMode::PushToTalk as u32
        );
        engine.stop_microphone_test();
        assert!(request.stop.load(Ordering::Acquire));
        assert!(matches!(
            receiver.recv().unwrap(),
            AudioCommand::StopMicrophone
        ));
        engine
            .start_voice("next", voice_ticket(), test_call_keys("local"))
            .unwrap();
        match receiver.recv().unwrap() {
            AudioCommand::Voice { request, .. } => assert_eq!(request.settings, applied),
            _ => panic!("expected the next call to use saved settings"),
        }
    }
    #[cfg(feature = "native-rtc")]
    #[test]
    fn busy_application_audio_producer_cannot_block_the_voice_output_callback() {
        let controls = Arc::new(AudioControls::new(&AudioSettings::default()));
        let queue = sample_queue(VecDeque::from([0.25; 960]));
        let state = OutputState {
            playback_limit: Arc::new(AtomicUsize::new(MAX_PLAYBACK_SAMPLES)),
            counters: Arc::new(Counters::default()),
            queue,
            deafened: Arc::new(AtomicBool::new(false)),
            errors: Arc::new(Mutex::new(None)),
            controls: controls.clone(),
            tones: Arc::new(Mutex::new(VecDeque::new())),
            stop: Arc::new(AtomicBool::new(false)),
        };
        let busy = controls.application_audio.try_queues().unwrap();
        let (finished, rendered) = mpsc::channel();
        let worker = thread::spawn(move || {
            let config = cpal::StreamConfig {
                channels: 1,
                sample_rate: cpal::SampleRate(AUDIO_SAMPLE_RATE),
                buffer_size: cpal::BufferSize::Default,
            };
            let mut callback = output_callback::<f32>(&config, state);
            let mut output = [0.0; 480];
            callback(&mut output);
            finished.send(output).unwrap();
        });
        let output = rendered.recv_timeout(Duration::from_secs(1));
        drop(busy);
        worker.join().unwrap();
        let output = output.expect("Voice output waited for a busy application-audio producer");
        let delay = (crate::voice_output::FILTER_DELAY_MS * 48.0) as usize;
        // The clock filter has a bounded start transient; its response and tail
        // are checked separately. Retain the busy-lock deadline and require
        // actual steady voice, rather than the old interpolator's first sample.
        assert!(output
            .iter()
            .all(|sample| sample.is_finite() && sample.abs() < 0.3));
        assert!(output[delay * 2..]
            .iter()
            .all(|sample| (*sample - 0.25).abs() < 0.0001));
    }

    #[test]
    fn output_deafen_and_stop_clear_clock_filter_history() {
        for rate in [44_100, 48_000, 96_000] {
            let state = OutputState {
                playback_limit: Arc::new(AtomicUsize::new(MAX_PLAYBACK_SAMPLES)),
                counters: Arc::new(Counters::default()),
                queue: sample_queue(VecDeque::from([0.75; 960])),
                deafened: Arc::new(AtomicBool::new(false)),
                errors: Arc::new(Mutex::new(None)),
                controls: Arc::new(AudioControls::new(&AudioSettings::default())),
                tones: Arc::new(Mutex::new(VecDeque::new())),
                stop: Arc::new(AtomicBool::new(false)),
            };
            let config = cpal::StreamConfig {
                channels: 1,
                sample_rate: cpal::SampleRate(rate),
                buffer_size: cpal::BufferSize::Default,
            };
            let mut output = output_callback::<f32>(&config, state.clone());
            let mut data = vec![0.0; rate as usize / 200];
            output(&mut data);
            assert!(data.iter().any(|v| v.abs() > 0.5));
            state.deafened.store(true, Ordering::Release);
            output(&mut data);
            assert!(data.iter().all(|v| *v == 0.0));
            assert!(state.queue.lock().unwrap().is_empty());
            state.deafened.store(false, Ordering::Release);
            output(&mut data);
            assert!(
                data.iter().all(|v| *v == 0.0),
                "Old FIR tail survived deafen"
            );
            state.queue.lock().unwrap().extend([0.125; 960]);
            output(&mut data);
            assert!(data.iter().any(|v| *v > 0.1));
            assert!(data.iter().all(|v| v.abs() < 0.2));
            state.stop.store(true, Ordering::Release);
            output(&mut data);
            assert!(data.iter().all(|v| *v == 0.0));
            output(&mut data);
            assert!(data.iter().all(|v| *v == 0.0));
        }
    }

    #[test]
    fn voice_clock_correction_does_not_retime_cues_or_application_audio() {
        for rate in [44_100, 48_000, 96_000] {
            let make_state = || OutputState {
                playback_limit: Arc::new(AtomicUsize::new(MAX_PLAYBACK_SAMPLES)),
                counters: Arc::new(Counters::default()),
                queue: sample_queue(VecDeque::new()),
                deafened: Arc::new(AtomicBool::new(false)),
                errors: Arc::new(Mutex::new(None)),
                controls: Arc::new(AudioControls::new(&AudioSettings::default())),
                tones: Arc::new(Mutex::new(
                    (0..20_000).map(|n| (n as f32 * 0.17).sin() * 0.1).collect(),
                )),
                stop: Arc::new(AtomicBool::new(false)),
            };
            let paced = make_state();
            let empty = make_state();
            let config = cpal::StreamConfig {
                channels: 2,
                sample_rate: cpal::SampleRate(rate),
                buffer_size: cpal::BufferSize::Default,
            };
            let mut with_voice_clock = output_callback::<f32>(&config, paced.clone());
            let mut without_voice = output_callback::<f32>(&config, empty.clone());
            for block in 0..30 {
                // Force different clock corrections without contributing sound.
                {
                    let mut queued = paced.queue.lock().unwrap();
                    queued.clear();
                    queued.extend(vec![0.0; if block % 2 == 0 { 1800 } else { 600 }]);
                }
                #[cfg(feature = "native-rtc")]
                for state in [&paced, &empty] {
                    let pcm: Vec<_> = (0..960)
                        .map(|n| ((n as f32 * 0.11).sin() * 2048.0) as i16)
                        .collect();
                    state.controls.application_audio.push_guarded(
                        "synthetic-screen",
                        &pcm,
                        &state.stop,
                    );
                }
                let frames = rate as usize / [400, 200, 100][block % 3];
                let mut a = vec![0.0; frames * 2];
                let mut b = a.clone();
                with_voice_clock(&mut a);
                without_voice(&mut b);
                assert_eq!(
                    a, b,
                    "Voice retimed a cue or application sample at {rate}Hz"
                );
                assert!(a.iter().all(|v| v.is_finite()));
                assert!(a.iter().any(|v| v.abs() > 0.01));
            }
        }
    }

    #[test]
    fn device_failure_stops_capture_and_playback_before_a_ui_poll() {
        for input in [true, false] {
            let mut engine = MediaEngine::new().unwrap();
            let applied = AudioSettings {
                input_device: Some("chosen microphone".into()),
                output_device: Some("chosen headset".into()),
                transmission: TransmissionMode::PushToTalk,
                ..Default::default()
            }
            .normalized();
            engine.configure(applied.clone());
            let (sender, receiver) = mpsc::sync_channel(8);
            engine.sender = Some(sender);
            engine
                .start_voice("ready", voice_ticket(), test_call_keys("local"))
                .unwrap();
            let request = match receiver.recv().unwrap() {
                AudioCommand::Voice { request, .. } => request,
                _ => panic!("expected voice request"),
            };
            let config = cpal::StreamConfig {
                channels: 1,
                sample_rate: cpal::SampleRate(AUDIO_SAMPLE_RATE),
                buffer_size: cpal::BufferSize::Default,
            };
            let mut capture = input_callback::<f32>(
                &config,
                InputState {
                    queue: request.capture.clone(),
                    level: engine.shared.level.clone(),
                    muted: engine.shared.muted.clone(),
                    deafened: engine.shared.deafened.clone(),
                    counters: engine.counters.clone(),
                    errors: request.errors.clone(),
                    controls: engine.shared.controls.clone(),
                    stop: request.stop.clone(),
                },
            );
            let mut output = output_callback::<f32>(
                &config,
                OutputState {
                    playback_limit: Arc::new(AtomicUsize::new(MAX_PLAYBACK_SAMPLES)),
                    counters: Arc::new(Counters::default()),
                    queue: request.playback.clone(),
                    deafened: engine.shared.deafened.clone(),
                    errors: request.errors.clone(),
                    controls: engine.shared.controls.clone(),
                    tones: engine.shared.tones.clone(),
                    stop: request.stop.clone(),
                },
            );
            // Actual data callbacks, synthetic samples, no device or network worker.
            capture(&[0.2; AUDIO_FRAME_SAMPLES]);
            assert!(!request.capture.lock().unwrap().is_empty());
            request
                .playback
                .lock()
                .unwrap()
                .extend([0.3; AUDIO_FRAME_SAMPLES + crate::voice_output::DEFAULT_RESERVE_SAMPLES]);
            let mut rendered = [0.0; AUDIO_FRAME_SAMPLES];
            output(&mut rendered);
            assert!(rendered.iter().any(|sample| *sample > 0.1));
            record_device_error(
                &request.errors,
                &request.stop,
                &engine.shared.controls,
                input,
                "device disconnected",
            );
            assert!(request.stop.load(Ordering::Acquire));
            // Teardown callbacks must not replace the actionable first cause.
            record_device_error(
                &request.errors,
                &request.stop,
                &engine.shared.controls,
                !input,
                "late teardown error",
            );
            capture(&[0.4; AUDIO_FRAME_SAMPLES]);
            assert!(request.capture.lock().unwrap().is_empty());
            assert_eq!(engine.input_level(), 0.0);
            rendered.fill(0.9);
            output(&mut rendered);
            assert!(rendered.iter().all(|sample| *sample == 0.0));
            let error = engine.take_error().unwrap();
            assert!(error.starts_with(if input {
                "Microphone stopped."
            } else {
                "Audio output stopped."
            }));
            assert!(error.contains("Voice & Video") && error.contains("device disconnected"));
            assert!(!error.contains("late teardown"));
            assert_eq!(
                engine.settings, applied,
                "never silently select another device"
            );
        }
    }
    #[test]
    fn device_failure_from_a_retired_stream_cannot_interrupt_the_next_call() {
        let (mut engine, entered, _, _) = delayed_audio();
        engine
            .start_voice("first", voice_ticket(), test_call_keys("local"))
            .unwrap();
        entered.recv_timeout(Duration::from_secs(1)).unwrap();
        wait_active(&engine);
        let old_stop = engine.stop.clone().unwrap();
        let old_errors = engine.errors.clone();
        engine.stop_voice();
        engine
            .start_voice("next", voice_ticket(), test_call_keys("local"))
            .unwrap();
        entered.recv_timeout(Duration::from_secs(1)).unwrap();
        wait_active(&engine);
        record_device_error(
            &old_errors,
            &old_stop,
            &engine.shared.controls,
            true,
            "old microphone unplugged",
        );
        assert!(old_errors.lock().unwrap().is_none());
        assert!(engine.take_error().is_none() && engine.is_active());
        assert!(!engine.stop.as_ref().unwrap().load(Ordering::Acquire));
    }
    #[test]
    fn device_failure_in_sound_preview_leaves_the_voice_call_running() {
        let (mut engine, entered, _, _) = delayed_audio();
        engine
            .start_voice("ready", voice_ticket(), test_call_keys("local"))
            .unwrap();
        entered.recv_timeout(Duration::from_secs(1)).unwrap();
        wait_active(&engine);
        let preview_stop = AtomicBool::new(false);
        let preview_controls = AudioControls::new(&AudioSettings::default());
        record_device_error(
            &engine.shared.sound_errors,
            &preview_stop,
            &preview_controls,
            false,
            "preview speakers unplugged",
        );
        assert!(preview_stop.load(Ordering::Acquire));
        assert!(engine
            .take_sound_error()
            .unwrap()
            .contains("preview speakers unplugged"));
        assert!(engine.take_error().is_none() && engine.is_active());
        assert!(!engine.stop.as_ref().unwrap().load(Ordering::Acquire));
    }
    #[test]
    fn call_ring_cancellation_rejects_queued_audio_without_touching_voice_or_normal_cues() {
        for cancelled in [true, false] {
            let mut engine = MediaEngine::new().unwrap();
            let (sender, receiver) = mpsc::sync_channel(8);
            engine.sender = Some(sender.clone());
            let audio = engine.call_alert_audio().unwrap();
            let privacy = engine
                .shared
                .controls
                .privacy_generation
                .load(Ordering::Acquire);
            engine.shared.tones.lock().unwrap().push_back(0.25);
            engine
                .shared
                .controls
                .call_tones
                .lock()
                .unwrap()
                .push_back(0.5);
            audio.play(AudioCue::Ring, AudioSettings::default());
            if cancelled {
                audio.stop();
            }
            sender.send(AudioCommand::Shutdown).unwrap();
            let (entered, _events) = mpsc::channel();
            let (_release, release) = mpsc::channel();
            audio_owner(
                receiver,
                engine.state.clone(),
                engine.pending_devices.clone(),
                || DelayedBackend {
                    entered,
                    release,
                    published: Arc::new(AtomicU64::new(0)),
                    current_stop: None,
                    _thread_local: std::rc::Rc::new(()),
                },
            );
            // The real owner dispatches valid tones to this device-free backend,
            // whose tone method deliberately reports an error if reached.
            assert_eq!(engine.take_sound_error().is_none(), cancelled);
            assert_eq!(
                engine
                    .shared
                    .controls
                    .privacy_generation
                    .load(Ordering::Acquire),
                privacy
            );
            assert_eq!(engine.shared.tones.lock().unwrap().front(), Some(&0.25));
            if cancelled {
                assert!(engine.shared.controls.call_tones.lock().unwrap().is_empty());
            }
        }
    }
    #[test]
    fn sound_preview_preserves_active_call_capture_and_normal_cue_settings() {
        let mut engine = MediaEngine::new().unwrap();
        let applied = AudioSettings {
            output_device: Some("saved headset".into()),
            transmission: TransmissionMode::PushToTalk,
            ..Default::default()
        }
        .normalized();
        engine.configure(applied.clone());
        let (sender, receiver) = mpsc::sync_channel(8);
        engine.sender = Some(sender);
        engine
            .start_voice("ready", voice_ticket(), test_call_keys("local"))
            .unwrap();
        let (generation, voice) = match receiver.recv().unwrap() {
            AudioCommand::Voice {
                generation,
                request,
            } => (generation, request),
            _ => panic!("expected voice request"),
        };
        assert!(publish_phase(
            &engine.state,
            generation,
            AudioPhase::ActiveVoice
        ));
        let privacy = engine
            .shared
            .controls
            .privacy_generation
            .load(Ordering::Acquire);
        let draft = AudioSettings {
            output_device: Some("test speakers".into()),
            input_device: Some("unsaved microphone".into()),
            transmission: TransmissionMode::Continuous,
            sound_volume: 0.7,
            ..applied.clone()
        };
        engine.preview_sound(draft.clone()).unwrap();
        match receiver.recv().unwrap() {
            AudioCommand::Tone {
                generation: tone_generation,
                settings,
                preview,
                ..
            } => {
                assert_eq!(tone_generation, generation);
                assert_eq!(settings, draft);
                assert!(preview);
            }
            _ => panic!("expected output-only preview"),
        }
        assert!(engine.is_active() && !voice.stop.load(Ordering::Acquire));
        assert!(Arc::ptr_eq(
            &engine.voice_queues.as_ref().unwrap().0,
            &voice.capture
        ));
        assert_eq!(engine.settings, applied);
        assert_eq!(
            engine
                .shared
                .controls
                .privacy_generation
                .load(Ordering::Acquire),
            privacy
        );
        assert_eq!(
            engine.shared.controls.transmission.load(Ordering::Relaxed),
            TransmissionMode::PushToTalk as u32
        );
        // A local microphone test must still refuse to interrupt a call.
        assert!(engine.preview_microphone(draft).is_err());
        assert!(engine.is_active() && !voice.stop.load(Ordering::Acquire));
        engine.play_tone(AudioCue::Mute).unwrap();
        match receiver.recv().unwrap() {
            AudioCommand::Tone {
                settings, preview, ..
            } => {
                assert_eq!(settings, applied);
                assert!(!preview);
            }
            _ => panic!("expected normal cue"),
        }
    }
    #[test]
    fn disabled_or_silent_sound_preview_never_starts_an_audio_owner() {
        for settings in [
            AudioSettings {
                sounds_enabled: false,
                ..Default::default()
            },
            AudioSettings {
                sound_volume: 0.0,
                ..Default::default()
            },
        ] {
            let mut engine = MediaEngine::new().unwrap();
            let applied = engine.settings.clone();
            engine.preview_sound(settings).unwrap();
            assert!(engine.owner.is_none() && engine.sender.is_none());
            assert_eq!(engine.settings, applied);
        }
    }
    #[test]
    fn chosen_event_preview_keeps_the_selected_event_and_applied_settings() {
        let mut engine = MediaEngine::new().unwrap();
        let applied = engine.settings.clone();
        let (sender, receiver) = mpsc::sync_channel(8);
        engine.sender = Some(sender);
        let mut edited = applied.clone();
        edited.event_sounds.get_mut(AudioCue::Ring).source =
            crate::sound_customization::SoundSource::BuiltIn { cue: AudioCue::Join };
        engine.preview_cue(AudioCue::Ring, edited.clone()).unwrap();
        match receiver.recv().unwrap() {
            AudioCommand::Tone { cue, settings, preview, .. } => {
                assert_eq!(cue, AudioCue::Ring);
                assert!(preview);
                assert_eq!(settings, edited);
            }
            _ => panic!("expected selected event preview"),
        }
        assert_eq!(engine.settings, applied);
        assert!(!engine.is_active() && !engine.is_microphone_test_active());
    }

    #[test]
    fn disabled_individual_events_open_no_audio_owner() {
        for cue in AudioCue::ALL {
            let mut settings = AudioSettings::default();
            settings.event_sounds.get_mut(cue).enabled = false;
            let mut engine = MediaEngine::new().unwrap();
            engine.configure(settings.clone());
            engine.preview_cue(cue, settings).unwrap();
            engine.play_tone(cue).unwrap();
            assert!(engine.owner.is_none() && engine.sender.is_none());
        }
    }

    #[test]
    fn applying_sound_choices_stops_queued_cues_without_restarting_the_active_call() {
        for change in 0..4 {
            let mut engine = MediaEngine::new().unwrap();
            let applied = engine.settings.clone();
            let (sender, receiver) = mpsc::sync_channel(8);
            engine.sender = Some(sender);
            engine.start_voice("ready", voice_ticket(), test_call_keys("local")).unwrap();
            let (generation, voice) = match receiver.recv().unwrap() {
                AudioCommand::Voice { generation, request } => (generation, request),
                _ => panic!("expected active voice request"),
            };
            assert!(publish_phase(&engine.state, generation, AudioPhase::ActiveVoice));
            engine.shared.tones.lock().unwrap().push_back(0.25);
            engine.shared.controls.call_tones.lock().unwrap().push_back(0.5);
            let preview_stop = Arc::new(AtomicBool::new(false));
            *engine.shared.sound_cancel.lock().unwrap() = Some(preview_stop.clone());
            let privacy = engine.shared.controls.privacy_generation.load(Ordering::Acquire);
            let old_ring = engine.shared.controls.call_tone_epoch.load(Ordering::Acquire);
            let mut edited = applied;
            match change {
                0 => edited.sounds_enabled = false,
                1 => edited.sound_volume = 0.0,
                2 => edited.event_sounds.get_mut(AudioCue::Message).enabled = false,
                _ => edited.event_sounds.get_mut(AudioCue::Join).source =
                    crate::sound_customization::SoundSource::BuiltIn { cue: AudioCue::Leave },
            }
            assert!(!engine.configure(edited), "Sound changes must not reopen voice devices");
            assert!(matches!(receiver.recv().unwrap(), AudioCommand::StopSound));
            assert!(preview_stop.load(Ordering::Acquire));
            assert!(engine.shared.tones.lock().unwrap().is_empty());
            assert!(engine.shared.controls.call_tones.lock().unwrap().is_empty());
            assert!(engine.shared.controls.call_tone_epoch.load(Ordering::Acquire) > old_ring);
            assert_eq!(engine.shared.controls.privacy_generation.load(Ordering::Acquire), privacy);
            assert!(engine.is_active() && !voice.stop.load(Ordering::Acquire));
            assert!(Arc::ptr_eq(&engine.voice_queues.as_ref().unwrap().0, &voice.capture));
        }
    }
    #[test]
    fn delayed_start_cancel_and_new_join_do_not_publish_or_surface_old_failure() {
        let (mut engine, entered, release, published) = delayed_audio();
        engine
            .start_voice("slow-failure", voice_ticket(), test_call_keys("local"))
            .unwrap();
        assert_eq!(
            entered.recv_timeout(Duration::from_secs(1)).unwrap(),
            "slow-failure"
        );
        assert!(engine.is_starting() && !engine.is_active());
        let old_errors = engine.errors.clone();
        engine.stop_voice(); // Returns while the fake driver is still blocked.
        assert!(!engine.is_starting() && !engine.is_active());
        engine
            .start_voice("new", voice_ticket(), test_call_keys("local"))
            .unwrap();
        release.send(()).unwrap();
        assert_eq!(entered.recv_timeout(Duration::from_secs(1)).unwrap(), "new");
        wait_active(&engine);
        record_error(&old_errors, "late old callback".into());
        assert_eq!(published.load(Ordering::Relaxed), 1);
        assert!(engine.take_error().is_none());
    }
    #[test]
    fn cancelled_delayed_microphone_test_never_becomes_active_or_publishes() {
        let (mut engine, entered, release, published) = delayed_audio();
        engine.start_microphone_test().unwrap();
        assert_eq!(
            entered.recv_timeout(Duration::from_secs(1)).unwrap(),
            "microphone"
        );
        assert!(engine.is_microphone_test_active());
        engine.stop_microphone_test();
        assert!(!engine.is_microphone_test_active());
        release.send(()).unwrap();
        // A subsequent request confirms the cancelled request has finished.
        engine
            .start_voice("next", voice_ticket(), test_call_keys("local"))
            .unwrap();
        assert_eq!(
            entered.recv_timeout(Duration::from_secs(1)).unwrap(),
            "next"
        );
        wait_active(&engine);
        assert_eq!(published.load(Ordering::Relaxed), 1);
        assert!(engine.take_error().is_none());
    }
    #[test]
    fn completed_failed_operations_are_revoked_before_the_next_join() {
        for microphone in [false, true] {
            let (mut engine, entered, release, published) = delayed_audio();
            if microphone {
                engine.configure(AudioSettings {
                    input_device: Some("failure".into()),
                    ..Default::default()
                });
                engine.start_microphone_test().unwrap();
                assert_eq!(
                    entered.recv_timeout(Duration::from_secs(1)).unwrap(),
                    "microphone"
                );
                release.send(()).unwrap();
            } else {
                engine
                    .start_voice("failure", voice_ticket(), test_call_keys("local"))
                    .unwrap();
                assert_eq!(
                    entered.recv_timeout(Duration::from_secs(1)).unwrap(),
                    "failure"
                );
            }
            let until = Instant::now() + Duration::from_secs(1);
            while Instant::now() < until
                && (engine.phase() != AudioPhase::Idle || engine.errors.lock().unwrap().is_none())
            {
                thread::sleep(Duration::from_millis(2));
            }
            assert_eq!(engine.phase(), AudioPhase::Idle);
            assert!(engine.errors.lock().unwrap().is_some());
            let failed_errors = engine.errors.clone();
            if microphone {
                engine.stop_microphone_test();
            } else {
                engine.stop_voice();
            }
            assert!(
                engine.take_error().is_none(),
                "explicit Stop clears a completed failure before a new HTTP ticket arrives"
            );
            assert!(engine.stop.is_none());
            engine
                .start_voice("next", voice_ticket(), test_call_keys("local"))
                .unwrap();
            assert_eq!(
                entered.recv_timeout(Duration::from_secs(1)).unwrap(),
                "next"
            );
            wait_active(&engine);
            record_error(&failed_errors, "old driver callback after stop".into());
            assert!(engine.take_error().is_none() && engine.is_active());
            assert_eq!(published.load(Ordering::Relaxed), 1);
        }
    }
    #[test]
    fn cue_failure_is_nonfatal_and_manual_ptt_works_without_device_or_key_hooks() {
        let (mut engine, entered, _, _) = delayed_audio();
        engine
            .start_voice("ready", voice_ticket(), test_call_keys("local"))
            .unwrap();
        entered.recv_timeout(Duration::from_secs(1)).unwrap();
        wait_active(&engine);
        engine.set_push_to_talk_key(0);
        engine.set_push_to_talk_pressed(true);
        poll_push_to_talk(&engine.shared.controls);
        assert!(engine.shared.controls.ptt_pressed.load(Ordering::Acquire));
        engine.play_tone(AudioCue::Mute).unwrap();
        let until = Instant::now() + Duration::from_secs(1);
        let mut sound_error = None;
        while Instant::now() < until {
            sound_error = engine.take_sound_error();
            if sound_error.is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(sound_error.as_deref(), Some("cue output unavailable"));
        assert!(engine.take_error().is_none() && engine.is_active());
    }
    #[test]
    fn actual_owner_reports_invalid_ticket_asynchronously_before_opening_devices() {
        let mut engine = MediaEngine::new().unwrap();
        assert!(engine.owner.is_none());
        let mut ticket = voice_ticket();
        ticket.key = "not-base64!".into();
        engine
            .start_voice("127.0.0.1", ticket, test_call_keys("local"))
            .unwrap();
        let until = Instant::now() + Duration::from_secs(1);
        let mut error = None;
        while Instant::now() < until {
            error = engine.take_error();
            if error.is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert!(error.unwrap().contains("invalid key"));
        assert!(!engine.is_starting() && !engine.is_active());
    }
    fn test_audio_controls() -> AudioControls {
        AudioControls::new(&AudioSettings {
            packet_ms: 20,
            transmission: TransmissionMode::Continuous,
            noise_suppression: false,
            automatic_sensitivity: Some(false),
            automatic_microphone_volume: false,
            microphone_gain: 1.0,
            // The former hard gate: these tests count whole open blocks.
            activation_release_ms: 0,
            ..Default::default()
        })
    }
    /// The capture chain over a synthetic call: the far end is rendered (and
    /// becomes the echo reference) 10 ms before its echo, delayed by
    /// `echo_delay` samples and low-passed, reaches the microphone with the
    /// near-end talker. Returns what would be transmitted per sample.
    #[cfg(feature = "native-rtc")]
    pub(super) fn run_capture_chain(
        settings: &AudioSettings,
        near: &[f32],
        far: &[f32],
        echo_delay: usize,
        echo_gain: f32,
    ) -> Vec<f32> {
        let controls = AudioControls::new(settings);
        let mut processor = MicrophoneProcessor::new();
        let mut path = 0.0f32;
        // A real microphone always has a little self-noise (about -70 dBFS).
        let mut noise = 0x2468_ace1u32;
        let mut sent = Vec::with_capacity(near.len());
        for (index, chunk) in near.chunks_exact(480).enumerate() {
            let start = index * 480;
            let mut reference = [0.0f32; 480];
            reference.copy_from_slice(&far[start..start + 480]);
            controls.echo_reference.rendered(&reference);
            let mut block = [0.0f32; 480];
            for (offset, sample) in block.iter_mut().enumerate() {
                let at = start + offset;
                let echo = if at >= echo_delay { far[at - echo_delay] } else { 0.0 };
                path += (echo - path) * 0.5;
                noise = noise.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let hiss = ((noise >> 8) as f32 / 16_777_215.0 - 0.5) * 0.001;
                *sample = chunk[offset] + path * echo_gain + hiss;
            }
            let audible = processor.process(&mut block, &controls);
            assert!(processor.echo_error.is_none());
            sent.extend_from_slice(if audible { &block } else { &[0.0; 480] });
        }
        sent
    }

    #[cfg(feature = "native-rtc")]
    #[test]
    #[ignore = "diagnostic: prints each stage of the capture chain"]
    fn diagnose_capture_chain_stages_with_echo() {
        use crate::voice_test_signals::{db, rms, talker, RATE};
        let (far, mut near) = match std::env::var_os("VOICE_TEST_WAV") {
            Some(path) => {
                std::env::set_var("RELAY_SYNTHETIC_SPEECH_WAV", path);
                let speech = owned_synthetic_speech();
                let len = (9.0 * RATE) as usize;
                let take = |from: usize, level: f32| -> Vec<f32> { (0..len).map(|i| speech[(from + i) % speech.len()] * level).collect() };
                (take(0, 1.0), take(speech.len() / 2, 0.6))
            }
            None => (talker(9.0, (190.0, 230.0), 7, 0.5), talker(9.0, (105.0, 135.0), 3, 0.3)),
        };
        let near_from = (2.5 * RATE) as usize;
        near[..near_from].fill(0.0);
        for gain in [0.0f32, 0.1, 0.25, 0.5] {
        for (label, ns, agc, aec) in [("aec only", false, false, true), ("all", true, true, true)] {
            let settings = AudioSettings { transmission: TransmissionMode::Continuous, echo_cancellation: aec, noise_suppression: ns, automatic_microphone_volume: agc, microphone_gain: 1.0, ..Default::default() };
            let alone = run_capture_chain(&settings, &near, &vec![0.0; near.len()], 2_400, 0.0);
            let with_echo = run_capture_chain(&settings, &near, &far, 2_400, gain);
            let echo_in: Vec<f32> = (RATE as usize..near_from).map(|n| far[n - 2_400] * gain.max(1e-6)).collect();
            let erle = db(rms(&echo_in)) - db(rms(&with_echo[RATE as usize..near_from]));
            let late = db(rms(&echo_in[..])) - db(rms(&with_echo[(1.8*RATE) as usize..near_from]));
            let mut ratios: Vec<f32> = alone[near_from..].chunks_exact(960).zip(with_echo[near_from..].chunks_exact(960)).filter(|(a, _)| db(rms(a)) > -40.0).map(|(a, b)| db(rms(b)) - db(rms(a))).collect();
            ratios.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let chopped = ratios.iter().filter(|r| **r < -6.0).count() as f32 / ratios.len() as f32;
            println!("echo x{gain} {label}: erle {erle:.1} dB (late {late:.1}), chopped {:.0} %, median {:+.1}, p10 {:+.1}", chopped*100.0, ratios[ratios.len()/2], ratios[ratios.len()/10]);
        }
        }
    }

    #[cfg(feature = "native-rtc")]
    #[test]
    fn call_echo_cancellation_removes_echo_without_chopping_double_talk() {
        use crate::voice_test_signals::{db, rms, talker, RATE};
        let seconds = 9.0;
        let far = talker(seconds, (190.0, 230.0), 7, 0.5);
        let mut near = talker(seconds, (105.0, 135.0), 3, 0.3);
        let near_from = (2.5 * RATE) as usize;
        near[..near_from].fill(0.0);
        let silent = vec![0.0; near.len()];
        for (device, echo_gain) in [("headphones", 0.0f32), ("speakers", 0.25)] {
            for (label, transmission) in [
                ("continuous", TransmissionMode::Continuous),
                ("voice activation", TransmissionMode::VoiceActivation),
            ] {
                let settings = AudioSettings {
                    transmission,
                    echo_cancellation: true,
                    ..Default::default()
                };
                let alone = run_capture_chain(&settings, &near, &silent, 2_400, 0.0);
                let with_echo = run_capture_chain(&settings, &near, &far, 2_400, echo_gain);
                // Far end only, once the echo path had 2 s to be heard: what of
                // its echo the others would hear.
                let from = (2.0 * RATE) as usize;
                let echo_in: Vec<f32> = (from..near_from).map(|n| far[n - 2_400] * echo_gain).collect();
                let erle = db(rms(&echo_in)) - db(rms(&with_echo[from..near_from]));
                // Double talk: 20 ms windows where the near end alone is audible.
                let mut ratios: Vec<f32> = alone[near_from..]
                    .chunks_exact(960)
                    .zip(with_echo[near_from..].chunks_exact(960))
                    .filter(|(a, _)| db(rms(a)) > -40.0)
                    .map(|(a, b)| db(rms(b)) - db(rms(a)))
                    .collect();
                ratios.sort_by(|a, b| a.partial_cmp(b).unwrap());
                let chopped = ratios.iter().filter(|r| **r < -6.0).count() as f32 / ratios.len() as f32;
                let median = ratios[ratios.len() / 2];
                println!("Call chain, {device}, {label}, AEC on: echo reduced {erle:.1} dB; double talk: {:.0} % of {} speech windows more than 6 dB quieter than without far end, median {median:+.1} dB, 10th percentile {:+.1} dB", chopped * 100.0, ratios.len(), ratios[ratios.len() / 10]);
                assert!(median > -4.5, "{device}/{label}: near end attenuated by {median:.1} dB");
                if echo_gain == 0.0 {
                    assert!(chopped <= 0.05, "{device}/{label}: voice chopped in {:.0} % of windows", chopped * 100.0);
                } else {
                    // Recorded speech leaves far less (see the diagnostic above).
                    assert!(erle >= 18.0, "{device}/{label}: echo must drop by at least 18 dB, got {erle:.1}");
                    // AEC3 still softens syllables while both talk over loud speakers:
                    // about 16 % with recorded speech (diagnose_capture_chain_stages_with_echo
                    // with VOICE_TEST_WAV), more with these steady synthetic vowels.
                    assert!(chopped <= 0.5, "{device}/{label}: voice chopped in {:.0} % of windows", chopped * 100.0);
                }
            }
        }
    }

    /// Speech, keyboard clicks and a fan through the real capture chain for
    /// every noise suppression choice: clicks left in pauses, what happens to
    /// speech, the processing time per 10 ms block and the added delay.
    #[cfg(feature = "native-rtc")]
    fn noise_suppression_report() -> Vec<(String, f32, f32, f32, f32)> {
        use crate::audio_settings::NoiseModel;
        use crate::click_suppression::NoiseStrength;
        use crate::voice_test_signals::{clicks, db, rms, talker};
        let seconds = 8.0;
        let speech = match std::env::var_os("VOICE_TEST_WAV") {
            Some(path) => {
                std::env::set_var("RELAY_SYNTHETIC_SPEECH_WAV", path);
                let recorded = owned_synthetic_speech();
                let peak = recorded.iter().fold(0.0f32, |p, s| p.max(s.abs()));
                (0..(seconds * 48_000.0) as usize).map(|i| recorded[i % recorded.len()] * 0.3 / peak).collect()
            }
            None => talker(seconds, (105.0, 135.0), 13, 0.3),
        };
        let (keys, onsets) = clicks(seconds, 2.5, 0.25, 21);
        let mut fan = vec![0.0f32; speech.len()];
        let mut state = 0x1357_9bdfu32;
        let mut low = 0.0f32;
        for sample in &mut fan {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            low += (((state >> 8) as f32 / 16_777_215.0 - 0.5) - low) * 0.05;
            *sample = low * 0.06;
        }
        let clean: Vec<f32> = speech.iter().zip(&fan).map(|(s, f)| s + f).collect();
        let noisy: Vec<f32> = clean.iter().zip(&keys).map(|(c, k)| c + k).collect();
        let silent = vec![0.0; speech.len()];
        let pauses: Vec<usize> = onsets
            .iter()
            .copied()
            .filter(|&at| at > 4_800 && at + 2_400 < speech.len() && rms(&speech[at - 960..at + 1_920]) < 0.002)
            .collect();
        let near_click = |at: usize| onsets.iter().any(|&o| o + 1_920 > at && o < at + 960 + 1_920);
        let mut rows = Vec::new();
        let mut reference: Option<Vec<f32>> = None;
        for (label, model, strength) in [
            ("Standard (RNNoise)", NoiseModel::Standard, NoiseStrength::Normal),
            ("Enhanced Low", NoiseModel::Enhanced, NoiseStrength::Low),
            ("Enhanced Normal", NoiseModel::Enhanced, NoiseStrength::Normal),
            ("Enhanced High", NoiseModel::Enhanced, NoiseStrength::High),
        ] {
            let settings = AudioSettings {
                transmission: TransmissionMode::Continuous,
                automatic_microphone_volume: false,
                microphone_gain: 1.0,
                noise_model: model,
                noise_strength: strength,
                ..Default::default()
            };
            let started = Instant::now();
            let out = run_capture_chain(&settings, &noisy, &silent, 0, 0.0);
            let per_block_ms = started.elapsed().as_secs_f32() * 1000.0 / (noisy.len() / 480) as f32;
            let clean_out = run_capture_chain(&settings, &clean, &silent, 0, 0.0);
            let residue: f32 = pauses.iter().map(|&at| out[at..at + 1_440].iter().map(|s| s * s).sum::<f32>()).sum();
            let click_db = 10.0 * (residue.max(1e-12)).log10();
            // Speech level against Standard's output of the same clean input.
            let base = reference.get_or_insert_with(|| clean_out.clone()).clone();
            let mut changes: Vec<f32> = (0..speech.len() / 960)
                .filter(|w| rms(&speech[w * 960..w * 960 + 960]) > 0.02 && !near_click(w * 960))
                .map(|w| db(rms(&out[w * 960..w * 960 + 960])) - db(rms(&base[w * 960..w * 960 + 960])))
                .collect();
            changes.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let speech_db = changes[changes.len() / 2];
            // Added delay: the lag with the strongest match to the clean input.
            let lag = (0..1_440usize)
                .max_by(|a, b| {
                    let score = |lag: usize| (48_000..240_000).map(|n| clean[n] * clean_out[n + lag]).sum::<f32>();
                    score(*a).partial_cmp(&score(*b)).unwrap()
                })
                .unwrap();
            rows.push((label.to_owned(), click_db, speech_db, per_block_ms, lag as f32 / 48.0));
        }
        rows
    }

    #[cfg(feature = "native-rtc")]
    #[test]
    fn enhanced_noise_suppression_removes_the_clicks_rnnoise_lets_through() {
        let rows = noise_suppression_report();
        // Recorded speech (VOICE_TEST_WAV) lets RNNoise recognise the voice as
        // it would in a call; the steady synthetic vowels confuse it more.
        let recorded = std::env::var_os("VOICE_TEST_WAV").is_some();
        let wanted = if recorded { [8.0f32, 20.0, 25.0] } else { [6.0, 8.0, 12.0] };
        let speech_floor = if recorded { -1.5 } else { -4.0 };
        println!("Noise suppression bench source: {}", if recorded { "recorded speech" } else { "synthetic talker" });
        for (label, click_db, speech_db, per_block_ms, delay_ms) in &rows {
            println!("Noise suppression bench, {label}: clicks between words {click_db:.1} dB energy, speech {speech_db:+.1} dB vs Standard, {per_block_ms:.3} ms CPU per 10 ms block, delay {delay_ms:.2} ms");
        }
        let standard = rows[0].1;
        for (index, wanted) in [(1usize, wanted[0]), (2, wanted[1]), (3, wanted[2])] {
            let (label, click_db, speech_db, per_block_ms, delay_ms) = &rows[index];
            assert!(standard - click_db >= wanted, "{label}: clicks only {:.1} dB below Standard", standard - click_db);
            assert!(*speech_db > speech_floor, "{label}: speech {speech_db:.1} dB");
            // Real time with a wide margin even in an unoptimised test build.
            assert!(*per_block_ms < 5.0, "{label}: {per_block_ms:.2} ms per 10 ms block");
            assert!(*delay_ms <= rows[0].4 + 1.0, "{label}: adds delay: {delay_ms:.2} ms");
        }
    }

    #[test]
    fn voice_gate_ramps_with_hysteresis_and_meters_the_level_it_compares() {
        let controls = AudioControls::new(&AudioSettings {
            packet_ms: 10,
            transmission: TransmissionMode::VoiceActivation,
            noise_suppression: false,
            automatic_sensitivity: Some(false),
            automatic_microphone_volume: false,
            microphone_gain: 1.0,
            activation_threshold: 0.02,
            activation_attack_ms: 5,
            activation_hold_ms: 20,
            activation_release_ms: 40,
            preserve_speech_start: false,
            ..Default::default()
        });
        let mut processor = MicrophoneProcessor::new();
        let mut quiet = [0.01; 480];
        assert!(!processor.process(&mut quiet, &controls));
        assert!(quiet.iter().all(|s| *s == 0.01), "closed audio is never altered");
        let reading = controls.gate_meter.take();
        assert!((reading.level - 0.01).abs() < 1e-6 && reading.threshold == 0.02 && !reading.open);
        let mut speech = [0.05; 480];
        assert!(processor.process(&mut speech, &controls));
        assert!(speech[0] < 0.001 && (speech[239] - 0.05).abs() < 1e-6, "5 ms attack instead of a click");
        let reading = controls.gate_meter.take();
        assert!((reading.level - 0.05).abs() < 1e-6 && reading.open, "the bar shows what the gate measured");
        // 0.012 is below the 0.02 threshold but within the 6 dB hysteresis: still speech.
        for _ in 0..5 {
            let mut hovering = [0.012; 480];
            assert!(processor.process(&mut hovering, &controls));
            assert!(hovering.iter().all(|s| *s == 0.012), "no pumping while hovering");
        }
        // Below the close level: 20 ms hold, then a 40 ms fade, then closed.
        let mut previous = 0.009;
        let mut open_blocks = 0;
        for _ in 0..10 {
            let mut fading = [0.009; 480];
            if processor.process(&mut fading, &controls) {
                open_blocks += 1;
                assert!(fading[0] <= previous + 1e-6, "no step up between blocks");
                previous = fading[479];
            }
        }
        assert_eq!((open_blocks, previous), (2 + 4, 0.0));
        controls.release_ms.store(0, Ordering::Relaxed);
        controls.attack_ms.store(0, Ordering::Relaxed);
        let mut speech = [0.05; 480];
        assert!(processor.process(&mut speech, &controls));
        assert_eq!(speech[0], 0.05, "live edits apply on the next block");
    }
    #[test]
    fn microphone_gain_gate_hangover_and_ptt_change_actual_transmission() {
        let controls = test_audio_controls();
        let mut processor = MicrophoneProcessor::new();
        controls.gain.store(2.0f32.to_bits(), Ordering::Relaxed);
        let mut input = [0.75; AUDIO_FRAME_SAMPLES];
        assert!(processor.process(&mut input, &controls));
        assert!(input.iter().all(|sample| *sample == 1.0));
        controls.gain.store(1.0f32.to_bits(), Ordering::Relaxed);
        controls
            .transmission
            .store(TransmissionMode::VoiceActivation as u32, Ordering::Relaxed);
        controls.hold_ms.store(60, Ordering::Relaxed);
        let mut quiet = [0.005; AUDIO_FRAME_SAMPLES];
        assert!(!processor.process(&mut quiet, &controls));
        let mut speech = [0.1; AUDIO_FRAME_SAMPLES];
        assert!(processor.process(&mut speech, &controls));
        for _ in 0..3 {
            assert!(processor.process(&mut quiet, &controls));
        }
        assert!(!processor.process(&mut quiet, &controls));
        controls
            .transmission
            .store(TransmissionMode::PushToTalk as u32, Ordering::Relaxed);
        assert!(!processor.process(&mut speech, &controls));
        controls.ptt_pressed.store(true, Ordering::Relaxed);
        assert!(processor.process(&mut speech, &controls));
        controls.ptt_pressed.store(false, Ordering::Relaxed);
        assert!(!processor.process(&mut speech, &controls));
        assert!(
            processor.denoiser.is_none(),
            "closed PTT never retains private denoiser history"
        );
    }
    #[test]
    fn automatic_voice_activity_passes_quiet_speech_without_learning_it_as_noise() {
        let settings = AudioSettings {
            microphone_gain: 1.0,
            noise_suppression: false,
            automatic_microphone_volume: false,
            activation_release_ms: 0,
            ..Default::default()
        };
        let controls = AudioControls::new(&settings);
        let mut processor = MicrophoneProcessor::new();
        for _ in 0..300 {
            let mut speech = [0.005; 480];
            assert!(
                processor.process(&mut speech, &controls),
                "quiet ongoing speech must stay open"
            );
        }
        for _ in 0..21 {
            processor.process(&mut [0.0; 480], &controls);
        }
        assert!(!processor.process(&mut [0.0; 480], &controls));
        controls
            .automatic_sensitivity
            .store(false, Ordering::Relaxed);
        assert!(
            !processor.process(&mut [0.005; 480], &controls),
            "an explicit manual threshold remains respected"
        );
    }
    #[test]
    fn automatic_microphone_volume_boosts_quiet_audio_and_limits_loud_peaks() {
        let controls = AudioControls::new(&AudioSettings {
            microphone_gain: 1.0,
            noise_suppression: false,
            transmission: TransmissionMode::Continuous,
            ..Default::default()
        });
        let mut processor = MicrophoneProcessor::new();
        let mut quiet = [0.005; 480];
        for _ in 0..30 {
            quiet.fill(0.005);
            assert!(processor.process(&mut quiet, &controls));
        }
        assert!(quiet[0] > 0.018, "quiet speech gets a bounded boost");
        let mut loud = [1.0; 480];
        assert!(processor.process(&mut loud, &controls));
        assert!(loud
            .iter()
            .all(|sample| sample.is_finite() && sample.abs() <= 0.95));
        controls.gain.store(1.5_f32.to_bits(), Ordering::Relaxed);
        let mut transient = [0.4, 0.6, 0.9, -0.9];
        assert!(processor.process(&mut transient, &controls));
        assert!(
            (transient[0] / transient[2] - 0.4 / 0.9).abs() < 0.0001,
            "the input boost must not clip the waveform before level control"
        );
        controls
            .transmission
            .store(TransmissionMode::PushToTalk as u32, Ordering::Relaxed);
        assert!(!processor.process(&mut [0.5; 480], &controls));
        assert_eq!(
            processor.automatic_gain, 1.0,
            "closed PTT resets its level history"
        );
    }
    #[test]
    fn automatic_level_keeps_syllable_dynamics_with_a_clearer_bounded_output() {
        let controls = AudioControls::new(&AudioSettings {
            microphone_gain: 1.0,
            noise_suppression: false,
            transmission: TransmissionMode::Continuous,
            ..Default::default()
        });
        let mut processor = MicrophoneProcessor::new();
        let mut levels = Vec::new();
        for frame in 0..250 {
            // A sustained voiced tone with alternating syllable strengths.
            // No device, recording or external reference clip is used.
            let amplitude = if frame % 20 < 10 { 0.02 } else { 0.04 };
            let mut input = std::array::from_fn::<_, 480, _>(|i| {
                amplitude
                    * ((frame * 480 + i) as f32 * std::f32::consts::TAU * 200.0 / 48000.0).sin()
            });
            assert!(processor.process(&mut input, &controls));
            assert!(input.iter().all(|s| s.is_finite() && s.abs() <= 0.95));
            if frame >= 200 {
                levels.push((
                    frame % 20 < 10,
                    (input.iter().map(|s| s * s).sum::<f32>() / 480.0).sqrt(),
                ));
            }
        }
        let average = |quiet: bool| {
            let values: Vec<_> = levels
                .iter()
                .filter(|(q, _)| *q == quiet)
                .map(|(_, rms)| *rms)
                .collect();
            values.iter().sum::<f32>() / values.len() as f32
        };
        let soft = average(true);
        let strong = average(false);
        assert!(
            soft > 0.035 && strong > 0.07,
            "quiet speech should be present without erasing dynamics: soft={soft}, strong={strong}"
        );
        assert!(
            strong > soft * 1.45,
            "normal syllable dynamics must survive level adjustment"
        );
        assert_eq!(AudioSettings::default().packet_ms, 10);
        assert_eq!(AudioSettings::default().jitter_ms, 10);
    }
    #[test]
    fn automatic_level_does_not_add_steps_at_continuous_pcm_block_edges() {
        let mut reports = Vec::new();
        for samples in [120, 240, 480, 960] {
            let controls = AudioControls::new(&AudioSettings {
                microphone_gain: 1.0,
                noise_suppression: false,
                transmission: TransmissionMode::Continuous,
                ..Default::default()
            });
            let mut processor = MicrophoneProcessor::new();
            let mut previous_gain: Option<f32> = None;
            let mut maximum_added_step = 0.0_f32;
            for frame in 0..100 {
                // Continuous PCM, deliberately not periodic at the packet edge.
                // Any jump beyond the source waveform is introduced by AGC.
                let source: Vec<f32> = (0..samples)
                    .map(|i| {
                        0.02 * (0.71_f64
                            + (frame * samples + i) as f64 * std::f64::consts::TAU * 437.0
                                / 48000.0)
                            .sin() as f32
                    })
                    .collect();
                let mut output = source.clone();
                assert!(processor.process(&mut output, &controls));
                assert!(output.iter().all(|s| s.is_finite() && s.abs() <= 0.95));
                if let Some(gain) = previous_gain {
                    maximum_added_step =
                        maximum_added_step.max((output[0] - source[0] * gain).abs());
                }
                previous_gain = Some(output[samples - 1] / source[samples - 1]);
            }
            reports.push((samples, maximum_added_step));
        }
        println!("AGC added PCM boundary steps (samples, maximum): {reports:?}");
        assert!(
            reports.iter().all(|(_, step)| *step < 0.0001),
            "automatic level changes must not create block-edge discontinuities: {reports:?}"
        );
    }
    #[test]
    fn automatic_volume_does_not_pump_when_detection_drops_out_mid_word() {
        let controls = AudioControls::new(&AudioSettings {
            microphone_gain: 1.0,
            noise_suppression: false,
            transmission: TransmissionMode::Continuous,
            ..Default::default()
        });
        let mut processor = MicrophoneProcessor::new();
        for _ in 0..100 {
            processor.process(&mut [0.005; 480], &controls);
        }
        let learned = processor.speech_output_gain;
        assert!(learned > 4.0, "quiet speech is boosted: {learned}");
        // A weaker 10 ms inside the word falls under the speech detector but is
        // still speech-like: the gain must hold instead of dropping to unity.
        let mut dip = [0.0012; 480];
        processor.process(&mut dip, &controls);
        assert!((dip[479] / 0.0012 - learned).abs() < 0.01 * learned, "held: {}", dip[479] / 0.0012);
        let mut word = [0.005; 480];
        processor.process(&mut word, &controls);
        assert!((word[0] / 0.005 - learned).abs() < 0.01 * learned, "no jump back up");
        // A real pause is quiet: it returns to unity immediately.
        let mut pause = [0.0001; 480];
        processor.process(&mut pause, &controls);
        assert!(pause.iter().all(|s| (*s - 0.0001).abs() < 1e-7));
    }
    #[test]
    fn quiet_microphone_level_survives_a_normal_pause_without_boosting_background() {
        let mut reports = Vec::new();
        for samples in [120, 240, 480, 960] {
            let controls = AudioControls::new(&AudioSettings {
                microphone_gain: 1.0,
                noise_suppression: false,
                transmission: TransmissionMode::Continuous,
                ..Default::default()
            });
            let mut processor = MicrophoneProcessor::new();
            let mut frame = vec![0.005; samples];
            // Isolate level control from recognition using a reproducible PCM
            // level. Native speech/RNNoise/Opus has a separate owned fixture.
            for _ in 0..48_000 / samples {
                frame.fill(0.005);
                assert!(processor.process(&mut frame, &controls));
            }
            let before_pause = frame[0];
            for _ in 0..48_000 / samples {
                frame.fill(0.0001);
                assert!(processor.process(&mut frame, &controls));
                assert!(
                    frame.iter().all(|s| (*s - 0.0001).abs() < 0.000001),
                    "a held speech level must not amplify background during the pause"
                );
            }
            frame.fill(0.005);
            assert!(processor.process(&mut frame, &controls));
            let resumed = frame[0];
            reports.push((samples, before_pause, resumed, resumed / before_pause));

            // Explicit privacy boundaries must still reset learned gain.
            controls
                .transmission
                .store(TransmissionMode::PushToTalk as u32, Ordering::Relaxed);
            assert!(!processor.process(&mut frame, &controls));
            assert_eq!(processor.automatic_gain, 1.0);
            controls.ptt_pressed.store(true, Ordering::Relaxed);
            let mut fresh = MicrophoneProcessor::new();
            let mut expected = vec![0.005; samples];
            frame.fill(0.005);
            assert!(fresh.process(&mut expected, &controls));
            assert!(processor.process(&mut frame, &controls));
            assert_eq!(frame, expected, "PTT must not restore a pre-privacy level");
        }
        println!("Quiet speech level before/after a one-second pause (samples, before, after, ratio): {reports:?}");
        assert!(
            reports
                .iter()
                .all(|(_, _, _, ratio)| (0.9..=1.1).contains(ratio)),
            "ordinary pauses must not make the next speech frame much quieter: {reports:?}"
        );
    }

    pub(super) fn owned_synthetic_speech() -> Vec<f32> {
        let path =
            std::path::PathBuf::from(std::env::var_os("RELAY_SYNTHETIC_SPEECH_WAV").unwrap());
        assert!(path.is_absolute());
        assert!(
            matches!(path.components().next(), Some(std::path::Component::Prefix(p))
            if matches!(p.kind(), std::path::Prefix::Disk(d) if d.eq_ignore_ascii_case(&b'C') || d.eq_ignore_ascii_case(&b'D')))
        );
        for parent in path.ancestors() {
            let info = std::fs::symlink_metadata(parent).unwrap();
            assert!(!info.file_type().is_symlink());
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                assert_eq!(info.file_attributes() & 0x400, 0);
            }
        }
        let wave = std::fs::read(path).unwrap();
        assert!(wave.len() < 8 * 1024 * 1024 && &wave[..4] == b"RIFF" && &wave[8..12] == b"WAVE");
        let mut pcm = None;
        let mut format = false;
        let mut at = 12;
        while at + 8 <= wave.len() {
            let size = u32::from_le_bytes(wave[at + 4..at + 8].try_into().unwrap()) as usize;
            let data = &wave[at + 8..at + 8 + size];
            match &wave[at..at + 4] {
                b"fmt " => {
                    assert_eq!(u16::from_le_bytes(data[0..2].try_into().unwrap()), 1);
                    assert_eq!(u16::from_le_bytes(data[2..4].try_into().unwrap()), 1);
                    assert_eq!(u32::from_le_bytes(data[4..8].try_into().unwrap()), 48000);
                    assert_eq!(u16::from_le_bytes(data[14..16].try_into().unwrap()), 16);
                    format = true;
                }
                b"data" => pcm = Some(data),
                _ => {}
            }
            at += 8 + size + size % 2;
        }
        assert!(format);
        let samples: Vec<f32> = pcm
            .unwrap()
            .chunks_exact(2)
            .map(|p| i16::from_le_bytes([p[0], p[1]]) as f32 / 32768.0)
            .collect();
        let peak = samples.iter().fold(0.0_f32, |p, s| p.max(s.abs()));
        assert!(peak > 0.01 && samples.len() > 48000);
        samples
    }
    #[test]
    #[ignore = "codec comparison with owned generated speech; no hardware or network"]
    fn report_actual_opus_loss_recovery_on_synthetic_speech() {
        let speech = owned_synthetic_speech();
        println!("Linked codec: {}", opus::version());
        for samples in [480, 960] {
            for bitrate in [32, 64, 96] {
                let mut encoder =
                    Encoder::new(48000, opus::Channels::Mono, opus::Application::Voip).unwrap();
                encoder.set_bitrate(Bitrate::Bits(bitrate * 1000)).unwrap();
                encoder.set_inband_fec(true).unwrap();
                encoder.set_packet_loss_perc(10).unwrap();
                let packets: Vec<_> = speech
                    .chunks_exact(samples)
                    .map(|frame| encoder.encode_vec_float(frame, 1500).unwrap())
                    .collect();
                let mut reference = opus::Decoder::new(48000, opus::Channels::Mono).unwrap();
                let mut plc = opus::Decoder::new(48000, opus::Channels::Mono).unwrap();
                let mut fec = opus::Decoder::new(48000, opus::Channels::Mono).unwrap();
                let (mut plc_error, mut fec_error, mut lost) = (0.0_f64, 0.0_f64, 0);
                for (index, packet) in packets.iter().enumerate() {
                    let mut clean = vec![0.0; samples];
                    let mut concealed = vec![0.0; samples];
                    let mut recovered = vec![0.0; samples];
                    reference.decode_float(packet, &mut clean, false).unwrap();
                    if index > 0 && index % 11 == 0 && index + 1 < packets.len() {
                        plc.decode_float(&[], &mut concealed, false).unwrap();
                        fec.decode_float(&packets[index + 1], &mut recovered, true)
                            .unwrap();
                        lost += 1;
                        for ((a, b), c) in clean.iter().zip(concealed).zip(recovered) {
                            plc_error += f64::from((*a - b).powi(2));
                            fec_error += f64::from((*a - c).powi(2));
                        }
                    } else {
                        plc.decode_float(packet, &mut concealed, false).unwrap();
                        fec.decode_float(packet, &mut recovered, false).unwrap();
                    }
                }
                println!("Opus VoIP {bitrate} kbps {} ms: {lost} losses, PLC error {plc_error:.6}, FEC error {fec_error:.6}, ratio {:.4}", samples as f32 / 48.0, fec_error / plc_error.max(1.0e-12));
                assert!(lost > 10 && plc_error.is_finite() && fec_error.is_finite());
            }
        }
    }

    #[test]
    #[ignore = "owned synthetic 48 kHz mono PCM WAV; no microphone or playback"]
    fn generated_speech_survives_quiet_microphone_processing_and_actual_opus() {
        let samples = owned_synthetic_speech();
        let peak = samples.iter().fold(0.0_f32, |p, s| p.max(s.abs()));
        let mut reports = Vec::new();
        for (label, automatic) in [("former factory gate", false), ("automatic speech", true)] {
            let settings = AudioSettings {
                automatic_sensitivity: Some(automatic),
                automatic_microphone_volume: automatic,
                microphone_gain: if automatic { 1.5 } else { 1.0 },
                bitrate_kbps: if automatic { 96 } else { 64 },
                ..Default::default()
            };
            let controls = AudioControls::new(&settings);
            let mut processor = MicrophoneProcessor::new();
            let mut encoder =
                crate::audio_settings::make_voice_encoder(settings.bitrate_kbps).unwrap();
            let mut decoder = opus::Decoder::new(48000, opus::Channels::Mono).unwrap();
            let (mut active, mut sent, mut energy, mut maximum) = (0, 0, 0.0_f64, 0.0_f32);
            for frame in samples.chunks_exact(480) {
                let mut input: Vec<f32> = frame.iter().map(|s| s * (0.035 / peak)).collect();
                if input.iter().map(|s| s * s).sum::<f32>() / 480.0 > 0.000004 {
                    active += 1;
                }
                if processor.process(&mut input, &controls) {
                    sent += 1;
                    let mut bytes = [0; 1500];
                    let n = encoder.encode_float(&input, &mut bytes).unwrap();
                    let mut output = [0.0_f32; 480];
                    let count = decoder
                        .decode_float(&bytes[..n], &mut output, false)
                        .unwrap();
                    assert_eq!(count, 480);
                    for sample in output {
                        assert!(sample.is_finite());
                        energy += f64::from(sample * sample);
                        maximum = maximum.max(sample.abs());
                    }
                }
            }
            assert!(active > 100);
            reports.push((label, active, sent, energy, maximum));
        }
        println!("synthetic quiet speech (RNNoise + actual Opus): {reports:?}");
        assert!(
            reports[1].2 > reports[0].2 && reports[1].2 > reports[1].1 / 2,
            "automatic activity must recover most quiet speech missed by the former gate"
        );
        assert!(reports[1].3 > 0.1 && reports[1].4 < 1.0);
    }
    #[test]
    #[cfg(feature = "native-rtc")]
    #[ignore = "owned synthetic speech, several input levels, real RNNoise/VAD/Opus; no hardware"]
    fn generated_speech_remains_audible_at_very_low_levels_with_background_noise() {
        let samples = owned_synthetic_speech();
        let peak = samples.iter().fold(0.0_f32, |p, s| p.max(s.abs()));
        let mut reports = Vec::new();
        for (amplitude, noise) in [
            (0.8, 0.0),
            (0.2, 0.0),
            (0.035, 0.0),
            (0.01, 0.0),
            (0.005, 0.0),
            (0.002, 0.0),
            (0.01, 0.001),
            (0.005, 0.001),
        ] {
            let settings = AudioSettings::default();
            assert_eq!((settings.packet_ms, settings.jitter_ms), (10, 10));
            let controls = AudioControls::new(&settings);
            let mut processor = MicrophoneProcessor::new();
            let mut encoder =
                crate::audio_settings::make_voice_encoder(settings.bitrate_kbps).unwrap();
            let mut decoder = opus::Decoder::new(48000, opus::Channels::Mono).unwrap();
            let mut seed = 0x513ab97du32;
            let (mut reference, mut delivered, mut sent, mut energy, mut maximum) =
                (0, 0, 0, 0.0_f64, 0.0_f32);
            let (mut receive_indications, mut old_indications, mut maximum_rms) = (0, 0, 0.0_f32);
            for frame in samples.chunks_exact(480) {
                // Define voiced reference frames before attenuation or added noise.
                let voiced = frame
                    .iter()
                    .map(|s| (s * 0.035 / peak).powi(2))
                    .sum::<f32>()
                    / 480.0
                    > 0.000004;
                reference += usize::from(voiced);
                let mut input: Vec<f32> = frame
                    .iter()
                    .map(|s| {
                        seed ^= seed << 13;
                        seed ^= seed >> 17;
                        seed ^= seed << 5;
                        s * amplitude / peak
                            + noise * (((seed as f64 / u32::MAX as f64) * 2.0 - 1.0) as f32)
                    })
                    .collect();
                if processor.process(&mut input, &controls) {
                    delivered += usize::from(voiced);
                    sent += 1;
                    assert!(input.iter().all(|s| s.is_finite() && s.abs() <= 0.95));
                    let mut packet = [0; 1500];
                    let n = encoder.encode_float(&input, &mut packet).unwrap();
                    let mut decoded = [0.0; 480];
                    assert_eq!(
                        decoder
                            .decode_float(&packet[..n], &mut decoded, false)
                            .unwrap(),
                        480
                    );
                    let power = decoded.iter().map(|s| s * s).sum::<f32>() / 480.0;
                    receive_indications += usize::from(received_audio_activity(power));
                    old_indications += usize::from(power > 0.0001);
                    maximum_rms = maximum_rms.max(power.sqrt());
                    for sample in decoded {
                        assert!(sample.is_finite());
                        energy += f64::from(sample * sample);
                        maximum = maximum.max(sample.abs());
                    }
                }
            }
            reports.push((
                amplitude,
                noise,
                reference,
                delivered,
                sent,
                energy,
                maximum,
                receive_indications,
                old_indications,
                maximum_rms,
            ));
            assert!(
                reference > 100 && delivered * 100 >= reference * 95,
                "most reference speech frames must be transmitted across levels: {reports:?}"
            );
            assert!(
                energy > 0.1 && maximum < 1.0,
                "real codec output must carry a nonzero bounded speech signal: {reports:?}"
            );
            assert!(
                receive_indications > reference / 2,
                "quiet decoded speech must register as received audio: {reports:?}"
            );
        }
        println!("quiet speech RNNoise/VAD/Opus levels: {reports:?}");
    }
    #[test]
    #[ignore = "owned synthetic speech, RNNoise/VAD/Opus across a normal pause; no devices"]
    fn generated_speech_keeps_learned_volume_across_silence_without_opening_the_gate() {
        let samples = owned_synthetic_speech();
        let phrase = samples
            .chunks_exact(4800)
            .max_by(|a, b| {
                let power = |v: &[f32]| v.iter().map(|s| s * s).sum::<f32>();
                power(a).total_cmp(&power(b))
            })
            .unwrap();
        let peak = phrase.iter().fold(0.0_f32, |p, s| p.max(s.abs()));
        let phrase: Vec<f32> = phrase.iter().map(|s| s * (0.02 / peak)).collect();
        let controls = AudioControls::new(&AudioSettings::default());
        let mut processor = MicrophoneProcessor::new();
        for frame in 0..200 {
            let at = frame % 10 * 480;
            processor.process(&mut phrase[at..at + 480].to_vec(), &controls);
        }
        let learned = processor.automatic_gain;
        assert!(
            learned > 1.5,
            "synthetic quiet speech must first learn a meaningful level"
        );
        let mut late_silence_sent = 0;
        for frame in 0..200 {
            let mut input = [0.0; 480];
            let sent = processor.process(&mut input, &controls);
            if frame >= 100 {
                late_silence_sent += usize::from(sent);
                assert!(input.iter().all(|s| s.abs() < 0.0001));
            }
        }
        let held = processor.automatic_gain;
        assert_eq!(
            late_silence_sent, 0,
            "a held level must not keep the voice gate open"
        );
        let mut encoder = crate::audio_settings::make_voice_encoder(96).unwrap();
        let mut decoder = Decoder::new(48_000, Channels::Mono).unwrap();
        let mut decoded_blocks = 0;
        let mut decoded_energy = 0_f64;
        let mut decoded_peak = 0_f32;
        for frame in 0..30 {
            let at = frame % 10 * 480;
            let mut input = phrase[at..at + 480].to_vec();
            if processor.process(&mut input, &controls) {
                assert!(input.iter().all(|s| s.is_finite() && s.abs() <= 0.95));
                let mut bytes = [0; 1500];
                let n = encoder.encode_float(&input, &mut bytes).unwrap();
                let mut output = [0.0; 480];
                assert_eq!(
                    decoder
                        .decode_float(&bytes[..n], &mut output, false)
                        .unwrap(),
                    480
                );
                for sample in output {
                    assert!(sample.is_finite());
                    decoded_energy += f64::from(sample * sample);
                    decoded_peak = decoded_peak.max(sample.abs());
                }
                decoded_blocks += 1;
            }
        }
        let rms = (decoded_energy / (decoded_blocks.max(1) * 480) as f64).sqrt();
        println!("Native synthetic pause: learned={learned}, held={held}, late_silence_sent={late_silence_sent}, resumed_blocks={decoded_blocks}/30, resumed_decoded_rms={rms}, peak={decoded_peak}");
        assert!(
            held > 1.5,
            "a normal silent interval discarded the learned microphone level"
        );
        assert!(
            decoded_blocks >= 15 && rms > 0.001 && decoded_peak <= 1.0,
            "resumed real Opus must carry finite, audible synthetic speech"
        );
    }

    #[test]
    fn received_audio_activity_includes_quiet_signal_without_claiming_silence() {
        assert!(received_audio_activity(0.002_f32.powi(2)));
        for power in [0.0, 0.00000001, f32::NAN, f32::INFINITY] {
            assert!(!received_audio_activity(power));
        }
    }
    #[test]
    #[cfg(feature = "native-rtc")]
    fn automatic_voice_activity_rejects_settled_fan_noise_and_respects_manual_gate() {
        for amplitude in [0.001, 0.005, 0.02] {
            let controls = AudioControls::new(&AudioSettings::default());
            let mut processor = MicrophoneProcessor::new();
            let mut seed = 0x513ab97du32;
            let mut settled_sent = 0;
            for frame in 0..1000 {
                let mut input = [0.0; 480];
                for (i, sample) in input.iter_mut().enumerate() {
                    seed ^= seed << 13;
                    seed ^= seed >> 17;
                    seed ^= seed << 5;
                    let white = ((seed as f64 / u32::MAX as f64) * 2.0 - 1.0) as f32;
                    *sample = amplitude
                        * (white * 0.7
                            + ((frame * 480 + i) as f32 * std::f32::consts::TAU * 60.0 / 48000.0)
                                .sin()
                                * 0.3);
                }
                if processor.process(&mut input, &controls) && frame >= 500 {
                    settled_sent += 1;
                }
            }
            println!("settled synthetic fan/hum amplitude={amplitude}: {settled_sent}/500 transmitted frames");
            assert!(
                settled_sent <= 60,
                "stationary noise must not keep the microphone open"
            );
            controls
                .transmission
                .store(TransmissionMode::PushToTalk as u32, Ordering::Relaxed);
            assert!(!processor.process(&mut [0.1; 480], &controls));
            assert!(
                processor.voice_detector.is_none() && processor.denoiser.is_none(),
                "closed PTT must discard both recognition and denoising history"
            );
            assert_eq!(processor.automatic_gain, 1.0);
        }
        let controls = AudioControls::new(&AudioSettings {
            automatic_sensitivity: Some(false),
            activation_threshold: 0.5,
            ..Default::default()
        });
        let mut processor = MicrophoneProcessor::new();
        for frame in 0..100 {
            let mut input = std::array::from_fn::<_, 480, _>(|i| {
                0.1 * ((frame * 480 + i) as f32 * std::f32::consts::TAU * 220.0 / 48000.0).sin()
            });
            assert!(
                !processor.process(&mut input, &controls),
                "manual sensitivity must override VAD"
            );
        }
    }
    #[test]
    fn real_rnnoise_reduces_stationary_noise_without_nonfinite_output() {
        let controls = test_audio_controls();
        controls.suppression.store(true, Ordering::Relaxed);
        let mut processor = MicrophoneProcessor::new();
        let mut random = 0x513ab97du32;
        let mut input_energy = 0.0f64;
        let mut output_energy = 0.0f64;
        for frame in 0..40 {
            let mut input = [0.0; AUDIO_FRAME_SAMPLES];
            for sample in &mut input {
                random ^= random << 13;
                random ^= random >> 17;
                random ^= random << 5;
                *sample = ((random as f64 / u32::MAX as f64) * 2.0 - 1.0) as f32 * 0.05;
                if frame >= 20 {
                    input_energy += (*sample as f64).powi(2);
                }
            }
            assert!(processor.process(&mut input, &controls));
            assert!(input
                .iter()
                .all(|sample| sample.is_finite() && sample.abs() <= 1.0));
            if frame == 0 {
                assert!(input[..DenoiseState::FRAME_SIZE]
                    .iter()
                    .all(|sample| *sample == 0.0));
            }
            if frame >= 20 {
                output_energy += input
                    .iter()
                    .map(|sample| (*sample as f64).powi(2))
                    .sum::<f64>();
            }
        }
        assert!(
            output_energy < input_energy * 0.6,
            "RNNoise must reduce the noise signal, ratio={}",
            output_energy / input_energy
        );
    }
    #[test]
    fn jitter_target_changes_priming_delay_and_packet_storage_stays_bounded() {
        let now = Instant::now();
        let mut speaker = Speaker::new(now).unwrap();
        let mut encoder =
            Encoder::new(AUDIO_SAMPLE_RATE, Channels::Mono, Application::LowDelay).unwrap();
        let mut encoded = [0; 400];
        let size = encoder
            .encode_float(&[0.1; AUDIO_FRAME_SAMPLES], &mut encoded)
            .unwrap();
        speaker.push(1, encoded[..size].to_vec(), now);
        let mut output = [0.0; AUDIO_FRAME_SAMPLES];
        assert!(!speaker.render(now, &mut output, 120));
        assert!(!speaker.render(now + Duration::from_millis(100), &mut output, 120));
        assert!(speaker.render(now + Duration::from_millis(120), &mut output, 120));
        for sequence in 2..=50 {
            speaker.push(sequence, encoded[..size].to_vec(), now);
        }
        assert_eq!(speaker.packets.len(), 8);
        assert!(speaker.render(now + Duration::from_millis(140), &mut output, 20));
    }
    fn playout_packet() -> Vec<u8> {
        let mut encoder =
            Encoder::new(AUDIO_SAMPLE_RATE, Channels::Mono, Application::LowDelay).unwrap();
        let input: Vec<f32> = (0..AUDIO_FRAME_SAMPLES)
            .map(|n| (n as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.2)
            .collect();
        let mut encoded = [0; 400];
        let size = encoder.encode_float(&input, &mut encoded).unwrap();
        encoded[..size].to_vec()
    }
    #[test]
    fn shared_playout_starts_at_packet_readiness_then_mixes_peers_on_one_timeline() {
        let origin = Instant::now();
        // Arrival is between microphone frame boundaries. The receive timeline
        // starts here, rather than waiting for origin + 20 ms.
        let arrival = origin + Duration::from_millis(7);
        let mut speakers = HashMap::new();
        for id in [11, 22] {
            let mut speaker = Speaker::new(origin).unwrap();
            speaker.push_timed(1, 0, true, playout_packet(), arrival);
            speakers.insert(id, speaker);
        }
        let mut schedule = PlayoutSchedule::default();
        assert!(schedule.due(arrival, &speakers, 20));
        let mut mixed = [0.0; AUDIO_FRAME_SAMPLES];
        let mut rendered = 0;
        for speaker in speakers.values_mut() {
            let mut decoded = [0.0; AUDIO_FRAME_SAMPLES];
            if speaker.render(arrival, &mut decoded, 20) {
                rendered += 1;
                for (out, input) in mixed.iter_mut().zip(decoded) {
                    *out += input;
                }
            }
        }
        assert_eq!(rendered, 2);
        assert!(mixed.iter().any(|sample| sample.abs() > 0.01));
        schedule.complete(arrival, true);
        assert_eq!(schedule.next, Some(arrival + FRAME_TIME));
        assert!(!schedule.due(arrival, &speakers, 20));

        // A third source does not enqueue another 20 ms device block between
        // the shared ticks; it is mixed with the other sources at the next tick.
        let third_arrival = arrival + Duration::from_millis(5);
        let mut third = Speaker::new(third_arrival).unwrap();
        third.push_timed(1, 0, true, playout_packet(), third_arrival);
        speakers.insert(33, third);
        assert!(!schedule.due(third_arrival, &speakers, 20));
        let next = arrival + FRAME_TIME;
        assert!(schedule.due(next, &speakers, 20));
        let rendered = speakers
            .values_mut()
            .map(|speaker| usize::from(speaker.render(next, &mut [0.0; AUDIO_FRAME_SAMPLES], 20)))
            .sum::<usize>();
        assert_eq!(rendered, 3);
        schedule.complete(next, true);
        assert_eq!(schedule.next, Some(next + FRAME_TIME));
    }
    #[test]
    fn short_mixer_stalls_preserve_every_decoded_sample_without_more_jitter() {
        let mut encoder = crate::audio_settings::make_voice_encoder(96).unwrap();
        let mut decoder = Decoder::new(48_000, Channels::Mono).unwrap();
        let mut packets = Vec::new();
        let mut reference = Vec::new();
        for frame in 0..500 {
            let pcm: Vec<f32> = (0..480)
                .map(|i| {
                    ((frame * 480 + i) as f32 * std::f32::consts::TAU * 440.0 / 48_000.0).sin()
                        * 0.2
                })
                .collect();
            let mut bytes = [0; 1500];
            let n = encoder.encode_float(&pcm, &mut bytes).unwrap();
            let mut decoded = [0.0; 480];
            assert_eq!(
                decoder
                    .decode_float(&bytes[..n], &mut decoded, false)
                    .unwrap(),
                480
            );
            reference.extend(decoded);
            packets.push(bytes[..n].to_vec());
        }
        for (offset, stall_us, burst_limit) in [
            (None, 0, 1),
            (Some(0), 4000, 2),
            (Some(7500), 4000, 2),
            (Some(0), 8000, 4),
            (Some(7500), 8000, 4),
        ] {
            let origin = Instant::now();
            let mut speakers = HashMap::from([(0, Speaker::new(origin).unwrap())]);
            let mut schedule = PlayoutSchedule::default();
            let mut queue = VecDeque::new();
            let mut actual = Vec::new();
            let mut underflows = 0;
            let mut max_burst = 0;
            for tick in 0..10200_u64 {
                let us = tick * 500;
                let now = origin + Duration::from_micros(us);
                if us % 10_000 == 0 && us / 10_000 < packets.len() as u64 {
                    let frame = (us / 10_000) as usize;
                    speakers.get_mut(&0).unwrap().push_timed(
                        frame as u64 + 1,
                        frame as u64 * 480,
                        frame == 0,
                        packets[frame].clone(),
                        now,
                    );
                }
                let stalled = offset.is_some_and(|offset| {
                    us >= 100_000 + offset && (us - 100_000 - offset) % 100_000 < stall_us
                });
                if !stalled {
                    let mut burst = 0;
                    while schedule.due(now, &speakers, 10) {
                        let mut pcm = [0.0; 120];
                        let active = speakers
                            .get_mut(&0)
                            .unwrap()
                            .render_quantum(now, &mut pcm, 10);
                        if active {
                            assert!(
                                !append_playback_frame(&mut queue, &pcm, MAX_PLAYBACK_SAMPLES),
                                "bounded queue overflowed"
                            );
                        }
                        schedule.complete_interval(now, active, Duration::from_micros(2500));
                        burst += 1;
                        assert!(
                            burst <= burst_limit,
                            "catch-up exceeded the sub-packet stall bound"
                        );
                    }
                    max_burst = max_burst.max(burst);
                    assert!(queue.len() <= MAX_PLAYBACK_SAMPLES);
                }
                // Same independent virtual output clock/10ms headroom in every case.
                // This is not an added production buffer or physical device timing.
                if (10_000..5_010_000).contains(&us) && us % 2500 == 0 {
                    for _ in 0..120 {
                        actual.push(queue.pop_front().unwrap_or_else(|| {
                            underflows += 1;
                            0.0
                        }));
                    }
                }
            }
            println!("Mixer stall {stall_us}us, offset {offset:?}: underflow samples={underflows}, max burst={max_burst}, output samples={}",actual.len());
            assert_eq!(
                underflows, 0,
                "short worker stalls must not create output starvation"
            );
            assert_eq!(
                actual, reference,
                "no decoded speech samples may be cut or repeated"
            );
            assert_eq!(speakers[&0].stats.late, 0, "all packets arrived on time");
        }
    }

    fn output_block_frames(case: usize, block: u64) -> usize {
        match case {
            0 => 240,
            1 => 480,
            2 => 960,
            3 => 1920,
            4 => 2400,
            5 => 4800,
            6 => [240, 4800, 240, 480][block as usize % 4],
            _ => {
                if block == 0 {
                    4800
                } else {
                    240
                }
            }
        }
    }

    fn output_block_frames_at_rate(case: usize, block: u64, rate: u32) -> usize {
        (output_block_frames(case, block) as u128 * u128::from(rate) / 48_000) as usize
    }

    #[cfg(feature = "native-rtc")]
    #[test]
    fn user_volume_also_controls_application_audio_in_native_render() {
        let media = MediaEngine::new().unwrap();
        let sink = media.application_audio_sink();
        let mut render = application_audio_test_renderer(sink.clone(), 2);
        let stop = AtomicBool::new(false);
        let source: Vec<_> = [8192_i16, -4096].into_iter().cycle().take(960).collect();
        let mut data = [0.0_f32; 480];
        for volume in [1.0, 0.0, 0.5, 2.0, f32::NAN] {
            media.set_user_volume("friend", volume);
            for _ in 0..12 {
                sink.push_peer_guarded(
                    "connection-42:friend:application-audio",
                    "friend",
                    &source,
                    &stop,
                );
                render(&mut data);
            }
            let expected = if volume.is_finite() { volume } else { 1.0 };
            for frame in data.chunks_exact(2) {
                assert!(
                    (frame[0] - 0.25 * expected).abs() < 0.001,
                    "User volume {volume} did not control shared left channel: {frame:?}"
                );
                assert!(
                    (frame[1] + 0.125 * expected).abs() < 0.001,
                    "User volume {volume} did not control shared right channel: {frame:?}"
                );
            }
        }
    }

    #[cfg(feature = "native-rtc")]
    #[test]
    fn stereo_application_privacy_erases_both_channels() {
        for rate in [44_100_u32, 48_000, 96_000] {
            let controls = Arc::new(AudioControls::new(&AudioSettings {
                playback_volume: 1.0,
                ..Default::default()
            }));
            let stop = Arc::new(AtomicBool::new(false));
            let deafened = Arc::new(AtomicBool::new(false));
            let mut render = output_callback::<f32>(
                &cpal::StreamConfig {
                    channels: 2,
                    sample_rate: cpal::SampleRate(rate),
                    buffer_size: cpal::BufferSize::Default,
                },
                OutputState {
                    playback_limit: Arc::new(AtomicUsize::new(MAX_PLAYBACK_SAMPLES)),
                    counters: Arc::new(Counters::default()),
                    queue: sample_queue(VecDeque::new()),
                    deafened: deafened.clone(),
                    errors: Arc::new(Mutex::new(None)),
                    controls: controls.clone(),
                    tones: Arc::new(Mutex::new(VecDeque::new())),
                    stop: stop.clone(),
                },
            );
            let source: Vec<_> = [8192_i16, -4096].into_iter().cycle().take(1920).collect();
            let mut data = vec![0.0_f32; rate as usize / 400 * 2];
            for gate in 0..4 {
                controls
                    .application_audio
                    .push_guarded("private-stereo", &source, &stop);
                for _ in 0..4 {
                    render(&mut data);
                }
                assert!(data.chunks_exact(2).any(|f| f[0] > 0.1 && f[1] < -0.05));
                match gate {
                    0 => {
                        deafened.store(true, Ordering::Release);
                        render(&mut data);
                        deafened.store(false, Ordering::Release);
                    }
                    1 => {
                        controls.application_audio.push_guarded(
                            "retained-silence",
                            &[0; 1920],
                            &stop,
                        );
                        controls.application_audio.clear_stream("private-stereo");
                        render(&mut data);
                    }
                    2 => {
                        controls.application_audio.clear();
                        let busy = controls.application_audio.try_queues().unwrap();
                        render(&mut data);
                        drop(busy);
                    }
                    _ => {
                        stop.store(true, Ordering::Release);
                        render(&mut data);
                    }
                }
                assert!(
                    data.iter().all(|v| *v == 0.0),
                    "privacy gate {gate} rate {rate}"
                );
                for _ in 0..8 {
                    render(&mut data);
                    assert!(data.iter().all(|v| *v == 0.0));
                }
            }
        }
    }

    #[cfg(feature = "native-rtc")]
    #[test]
    fn shared_application_stereo_preserves_channels_and_mono_fallback() {
        for rate in [44_100_u32, 48_000, 96_000] {
            for channels in [1_u16, 2, 6] {
                for pair in [[8192_i16, 0], [0, 8192], [8192, -8192], [8192, 8192]] {
                    let controls = Arc::new(AudioControls::new(&AudioSettings {
                        playback_volume: 1.0,
                        ..Default::default()
                    }));
                    let stop = Arc::new(AtomicBool::new(false));
                    let mut output = output_callback::<f32>(
                        &cpal::StreamConfig {
                            channels,
                            sample_rate: cpal::SampleRate(rate),
                            buffer_size: cpal::BufferSize::Default,
                        },
                        OutputState {
                            playback_limit: Arc::new(AtomicUsize::new(MAX_PLAYBACK_SAMPLES)),
                            counters: Arc::new(Counters::default()),
                            queue: sample_queue(VecDeque::new()),
                            deafened: Arc::new(AtomicBool::new(false)),
                            errors: Arc::new(Mutex::new(None)),
                            controls: controls.clone(),
                            tones: Arc::new(Mutex::new(VecDeque::new())),
                            stop: stop.clone(),
                        },
                    );
                    let source: Vec<_> = pair.into_iter().cycle().take(960).collect();
                    let mut pcm = vec![0.0_f32; rate as usize / 100 * channels as usize];
                    for _ in 0..20 {
                        controls
                            .application_audio
                            .push_guarded("owned-stereo", &source, &stop);
                        output(&mut pcm);
                    }
                    let mono = (f32::from(pair[0]) + f32::from(pair[1])) / 65536.0;
                    for frame in pcm.chunks_exact(channels as usize) {
                        for (channel, value) in frame.iter().enumerate() {
                            let expected = if channels == 2 {
                                f32::from(pair[channel]) / 32768.0
                            } else {
                                mono
                            };
                            assert!((value - expected).abs() < 0.00001,
                                "rate={rate} channels={channels} input={pair:?} channel={channel} actual={value} expected={expected}");
                        }
                    }
                    println!(
                        "Owned stereo render rate={rate} channels={channels} input={pair:?} pass"
                    );
                }
            }
        }
    }

    #[cfg(feature = "native-rtc")]
    #[test]
    fn decoded_application_short_tail_and_privacy_gates_remain_bounded() {
        for rate in [44_100, 48_000, 96_000] {
            let controls = Arc::new(AudioControls::new(&AudioSettings::default()));
            controls
                .playback_volume
                .store(1.0_f32.to_bits(), Ordering::Relaxed);
            let deafened = Arc::new(AtomicBool::new(false));
            let stop = Arc::new(AtomicBool::new(false));
            let config = cpal::StreamConfig {
                channels: 1,
                sample_rate: cpal::SampleRate(rate),
                buffer_size: cpal::BufferSize::Default,
            };
            let mut output = output_callback::<f32>(
                &config,
                OutputState {
                    queue: sample_queue(VecDeque::new()),
                    playback_limit: Arc::new(AtomicUsize::new(MAX_PLAYBACK_SAMPLES)),
                    counters: Arc::new(Counters::default()),
                    deafened: deafened.clone(),
                    stop: stop.clone(),
                    errors: Arc::new(Mutex::new(None)),
                    controls: controls.clone(),
                    tones: Arc::new(Mutex::new(VecDeque::new())),
                },
            );
            controls
                .application_audio
                .push_guarded("short", &[8192; 240], &stop);
            let frames = rate as usize / 400;
            let (mut energy, mut first) = (0.0_f32, None);
            for block in 0..10 {
                let mut pcm = vec![0.0; frames];
                output(&mut pcm);
                if pcm.iter().any(|v| *v > 0.1) {
                    first.get_or_insert(block);
                }
                energy += pcm.iter().map(|v| v * v).sum::<f32>();
            }
            assert!(
                first.is_some_and(|n| n <= 3),
                "A short final sound cannot wait forever"
            );
            assert!(energy > frames as f32 * 0.04);
            let mut silence = vec![0.0; frames];
            output(&mut silence);
            assert!(silence.iter().all(|v| *v == 0.0));
            controls
                .application_audio
                .push_guarded("long", &[8192; 1920], &stop);
            // Two2.5ms callbacks supply the bounded5ms startup wait; the
            // third must play. Keep the existing short-tail deadline above.
            output(&mut silence);
            assert!(silence.iter().all(|v| *v == 0.0));
            output(&mut silence);
            assert!(silence.iter().all(|v| *v == 0.0));
            output(&mut silence);
            assert!(silence.iter().any(|v| *v > 0.1));
            deafened.store(true, Ordering::Relaxed);
            output(&mut silence);
            assert!(silence.iter().all(|v| *v == 0.0));
            deafened.store(false, Ordering::Relaxed);
            output(&mut silence);
            assert!(
                silence.iter().all(|v| *v == 0.0),
                "Un-deafen must not replay old program audio"
            );
            controls
                .application_audio
                .push_guarded("retired", &[8192; 1920], &stop);
            stop.store(true, Ordering::Release);
            output(&mut silence);
            assert!(silence.iter().all(|v| *v == 0.0));
        }
    }

    #[cfg(feature = "native-rtc")]
    #[test]
    #[ignore = "Owned decoded application audio/device block trace; no physical or SDK transport claim"]
    fn output_device_block_sizes_preserve_decoded_application_audio() {
        let mut results = Vec::new();
        for rate in [48_000_u32, 44_100, 96_000] {
            for decoded_ms in [10_u64, 20, 40] {
                for case in 0..8 {
                    let controls = Arc::new(AudioControls::new(&AudioSettings::default()));
                    controls
                        .playback_volume
                        .store(1.0_f32.to_bits(), Ordering::Relaxed);
                    let stop = Arc::new(AtomicBool::new(false));
                    let state = OutputState {
                        playback_limit: Arc::new(AtomicUsize::new(MAX_PLAYBACK_SAMPLES)),
                        counters: Arc::new(Counters::default()),
                        queue: sample_queue(VecDeque::new()),
                        deafened: Arc::new(AtomicBool::new(false)),
                        errors: Arc::new(Mutex::new(None)),
                        controls: controls.clone(),
                        tones: Arc::new(Mutex::new(VecDeque::new())),
                        stop: stop.clone(),
                    };
                    let config = cpal::StreamConfig {
                        channels: 1,
                        sample_rate: cpal::SampleRate(rate),
                        buffer_size: cpal::BufferSize::Default,
                    };
                    let mut output = output_callback::<f32>(&config, state);
                    let (mut source_ns, mut output_ns, mut block) = (0_u64, 3_750_000_u64, 0_u64);
                    let (mut missing, mut rendered) = (0_u64, 0_u64);
                    loop {
                        let ns = source_ns.min(output_ns);
                        if ns >= 5_000_000_000 {
                            break;
                        }
                        if ns == source_ns {
                            controls.application_audio.push_guarded(
                                "owned-screen",
                                &vec![8192; decoded_ms as usize * 48 * 2],
                                &stop,
                            );
                            source_ns += decoded_ms * 1_000_000;
                        }
                        if ns == output_ns {
                            let mut pcm =
                                vec![0.0_f32; output_block_frames_at_rate(case, block, rate)];
                            output(&mut pcm);
                            assert!(pcm.iter().all(|v| v.is_finite() && *v >= 0.0 && *v <= 0.25));
                            if ns >= 1_000_000_000 {
                                missing += pcm.iter().filter(|v| **v < 0.249).count() as u64;
                                rendered += pcm.len() as u64;
                            }
                            block += 1;
                            output_ns += ((output_block_frames_at_rate(case, block, rate) as u128
                                * 1_000_000_000)
                                / u128::from(rate)) as u64;
                        }
                    }
                    println!(
                        "Owned application block trace {}",
                        serde_json::json!({"case":case,"native_rate":rate,"decoded_frame_ms":decoded_ms,"virtual_seconds":5,"warmup_seconds":1,"steady_missing_samples":missing,"steady_rendered_samples":rendered})
                    );
                    results.push((case, missing));
                }
            }
        }
        assert_eq!(results[0], (0, 0));
        assert!(
            results.iter().all(|(_, missing)| *missing == 0),
            "Device-sized callbacks cut decoded application audio: {results:?}"
        );
    }

    #[cfg(feature = "native-rtc")]
    #[test]
    #[ignore = "Owned independent application sink/output clocks; no devices, NetEQ transport or endurance claim"]
    fn slight_output_clock_drift_does_not_interrupt_application_audio() {
        let mut results = Vec::new();
        for rate in [48_000_u32, 44_100, 96_000] {
            for period in [5_u32, 50, 100] {
                for ppm in [0_i64, 100, -100] {
                    let controls = Arc::new(AudioControls::new(&AudioSettings::default()));
                    controls
                        .playback_volume
                        .store(1.0_f32.to_bits(), Ordering::Relaxed);
                    let stop = Arc::new(AtomicBool::new(false));
                    let state = OutputState {
                        playback_limit: Arc::new(AtomicUsize::new(MAX_PLAYBACK_SAMPLES)),
                        counters: Arc::new(Counters::default()),
                        queue: sample_queue(VecDeque::new()),
                        deafened: Arc::new(AtomicBool::new(false)),
                        errors: Arc::new(Mutex::new(None)),
                        controls: controls.clone(),
                        tones: Arc::new(Mutex::new(VecDeque::new())),
                        stop: stop.clone(),
                    };
                    let config = cpal::StreamConfig {
                        channels: 1,
                        sample_rate: cpal::SampleRate(rate),
                        buffer_size: cpal::BufferSize::Default,
                    };
                    let mut output = output_callback::<f32>(&config, state);
                    let (mut source_frame, mut block) = (0_u64, 0_u64);
                    let (mut missing, mut rendered) = (0_u64, 0_u64);
                    let pcm = [8192_i16; 960];
                    let frames = rate as usize * period as usize / 1000;
                    let mut data = vec![0.0_f32; frames];
                    let seconds = if rate == 48_000 { 600_u64 } else { 120_u64 };
                    loop {
                        let source_ns = source_frame * 10_000_000;
                        let output_ns = 3_750_000
                            + (u128::from(block) * frames as u128 * 1_000_000_000_000_000
                                / u128::from(rate)
                                / (1_000_000 + ppm) as u128) as u64;
                        if source_ns.min(output_ns) >= seconds * 1_000_000_000 {
                            break;
                        }
                        if source_ns <= output_ns {
                            controls.application_audio.push_guarded(
                                "owned-clock-screen",
                                &pcm,
                                &stop,
                            );
                            source_frame += 1;
                        } else {
                            output(&mut data);
                            assert!(data
                                .iter()
                                .all(|v| v.is_finite() && *v >= -0.00001 && *v <= 0.251));
                            if output_ns >= 1_000_000_000 {
                                missing += data.iter().filter(|v| **v < 0.249).count() as u64;
                                rendered += data.len() as u64;
                            }
                            block += 1;
                        }
                    }
                    println!(
                        "Owned application output clock trace {}",
                        serde_json::json!({"native_rate":rate,"period_ms":period,"ppm":ppm,"virtual_seconds":seconds,"warmup_seconds":1,"steady_missing_samples":missing,"steady_rendered_samples":rendered,"dropped_samples":controls.application_audio.try_queues().unwrap().dropped_samples()})
                    );
                    results.push((
                        ppm,
                        missing,
                        controls
                            .application_audio
                            .try_queues()
                            .unwrap()
                            .dropped_samples(),
                    ));
                }
            }
        }
        assert!(
            results
                .iter()
                .all(|(_, missing, dropped)| *missing == 0 && *dropped == 0),
            "Independent output clocks interrupted decoded application audio: {results:?}"
        );
    }

    #[cfg(feature = "native-rtc")]
    #[test]
    fn retiring_an_application_track_erases_its_filter_history() {
        let controls = Arc::new(AudioControls::new(&AudioSettings::default()));
        controls
            .playback_volume
            .store(1.0_f32.to_bits(), Ordering::Relaxed);
        let stop = Arc::new(AtomicBool::new(false));
        let state = OutputState {
            playback_limit: Arc::new(AtomicUsize::new(MAX_PLAYBACK_SAMPLES)),
            counters: Arc::new(Counters::default()),
            queue: sample_queue(VecDeque::new()),
            deafened: Arc::new(AtomicBool::new(false)),
            errors: Arc::new(Mutex::new(None)),
            controls: controls.clone(),
            tones: Arc::new(Mutex::new(VecDeque::new())),
            stop: stop.clone(),
        };
        let mut output = output_callback::<f32>(
            &cpal::StreamConfig {
                channels: 1,
                sample_rate: cpal::SampleRate(48_000),
                buffer_size: cpal::BufferSize::Default,
            },
            state,
        );
        controls
            .application_audio
            .push_guarded("retired", &[8192; 1920], &stop);
        controls
            .application_audio
            .push_guarded("retained-silence", &[0; 1920], &stop);
        let mut data = [0.0_f32; 120];
        for _ in 0..4 {
            output(&mut data);
        }
        assert!(data.iter().any(|v| *v > 0.1));
        controls.application_audio.clear_stream("retired");
        for _ in 0..8 {
            output(&mut data);
            assert!(
                data.iter().all(|v| *v == 0.0),
                "A removed track survived in the resampler history"
            );
        }
    }

    #[test]
    #[ignore = "Owned virtual device block sizes; no physical audio or hardware timing claim"]
    fn output_device_block_sizes_preserve_continuous_voice_after_startup() {
        let mut encoder = crate::audio_settings::make_voice_encoder(96).unwrap();
        let mut packets = Vec::new();
        for frame in 0..100 {
            let pcm: Vec<_> = (0..480)
                .map(|n| {
                    ((frame * 480 + n) as f32 * std::f32::consts::TAU * 440.0 / 48_000.0).sin()
                        * 0.2
                })
                .collect();
            let mut bytes = [0; 1500];
            let n = encoder.encode_float(&pcm, &mut bytes).unwrap();
            packets.push(bytes[..n].to_vec());
        }
        let mut results = Vec::new();
        for rate in [48_000_u32, 44_100, 96_000] {
            for case in 0..8 {
                let origin = Instant::now();
                let mut speakers = HashMap::from([(0, Speaker::new(origin).unwrap())]);
                let mut schedule = PlayoutSchedule::default();
                let queue = sample_queue(VecDeque::with_capacity(MAX_PLAYBACK_SAMPLES));
                let playback_limit = Arc::new(AtomicUsize::new(MAX_PLAYBACK_SAMPLES));
                let counters = Arc::new(Counters::default());
                let state = OutputState {
                    playback_limit: playback_limit.clone(),
                    counters: counters.clone(),
                    queue: queue.clone(),
                    deafened: Arc::new(AtomicBool::new(false)),
                    errors: Arc::new(Mutex::new(None)),
                    controls: Arc::new(AudioControls::new(&AudioSettings::default())),
                    tones: Arc::new(Mutex::new(VecDeque::new())),
                    stop: Arc::new(AtomicBool::new(false)),
                };
                let config = cpal::StreamConfig {
                    channels: 1,
                    sample_rate: cpal::SampleRate(rate),
                    buffer_size: cpal::BufferSize::Default,
                };
                let missing = Arc::new(AtomicU64::new(0));
                let priming = Arc::new(AtomicU64::new(0));
                let (m, p) = (missing.clone(), priming.clone());
                let mut output = output_callback_observed::<f32>(&config, state, move |a, b| {
                    m.fetch_add(a as u64, Ordering::Relaxed);
                    p.fetch_add(b as u64, Ordering::Relaxed);
                });
                let (mut source_frame, mut output_block) = (0_u64, 0_u64);
                let mut output_ns = 3_750_000_u64;
                let (
                    mut steady_missing,
                    mut steady_priming,
                    mut steady_dropped,
                    mut rendered_samples,
                ) = (0_u64, 0_u64, 0_u64, 0_u64);
                let mut first_ready = None;
                let mut peak = 0;
                loop {
                    let source_ns = source_frame * 10_000_000;
                    let mixer_ns = schedule
                        .next
                        .map(|at| at.duration_since(origin).as_nanos() as u64)
                        .unwrap_or(source_ns);
                    let ns = source_ns.min(mixer_ns).min(output_ns);
                    if ns >= 5_000_000_000 {
                        break;
                    }
                    let now = origin + Duration::from_nanos(ns);
                    if ns == source_ns {
                        speakers.get_mut(&0).unwrap().push_timed(
                            source_frame + 1,
                            source_frame * 480,
                            source_frame == 0,
                            packets[source_frame as usize % 100].clone(),
                            now,
                        );
                        source_frame += 1;
                    }
                    if schedule.due(now, &speakers, 10) {
                        let mut pcm = [0.0; 120];
                        let active = speakers
                            .get_mut(&0)
                            .unwrap()
                            .render_quantum(now, &mut pcm, 10);
                        assert!(active);
                        let mut queued = queue.lock().unwrap();
                        let limit = playback_limit.load(Ordering::Relaxed);
                        let excess = (queued.len() + pcm.len()).saturating_sub(limit);
                        assert_eq!(append_playback_frame(&mut queued, &pcm, limit), excess > 0);
                        if ns >= 1_000_000_000 {
                            steady_dropped += excess as u64;
                        }
                        peak = peak.max(queued.len());
                        schedule.complete_interval(now, true, Duration::from_micros(2500));
                    }
                    if ns == output_ns {
                        let frames = output_block_frames_at_rate(case, output_block, rate);
                        let mut pcm = vec![0.0; frames];
                        let (before_m, before_p) = (
                            missing.load(Ordering::Relaxed),
                            priming.load(Ordering::Relaxed),
                        );
                        output(&mut pcm);
                        assert!(pcm.iter().all(|v| v.is_finite()));
                        let (after_m, after_p) = (
                            missing.load(Ordering::Relaxed),
                            priming.load(Ordering::Relaxed),
                        );
                        if first_ready.is_none() && after_p == before_p {
                            first_ready = Some(ns);
                        }
                        if ns >= 1_000_000_000 {
                            steady_missing += after_m - before_m;
                            steady_priming += after_p - before_p;
                            rendered_samples += frames as u64;
                        }
                        output_block += 1;
                        output_ns += ((output_block_frames_at_rate(case, output_block, rate)
                            as u128
                            * 1_000_000_000)
                            / u128::from(rate)) as u64;
                    }
                }
                assert!(
                    first_ready.is_some_and(|n| n <= 210_000_000),
                    "Device priming cannot hide gaps indefinitely"
                );
                assert_eq!(
                    counters.playback_drop.load(Ordering::Relaxed),
                    0,
                    "No render-side trim may conceal a gap"
                );
                assert!(peak <= MAX_DEVICE_PLAYBACK_SAMPLES);
                assert_eq!(speakers[&0].stats.late, 0);
                assert_eq!(speakers[&0].stats.concealed, 0);
                println!(
                    "Owned output block trace {}",
                    serde_json::json!({"case":case,"native_rate":rate,"first_block_frames":output_block_frames_at_rate(case,0,rate),"virtual_seconds":5,"warmup_seconds":1,"steady_missing_samples":steady_missing,"steady_priming_samples":steady_priming,"steady_dropped_samples":steady_dropped,"steady_rendered_samples":rendered_samples,"first_ready_ns":first_ready,"peak_queue_samples":peak,"queue_limit_samples":playback_limit.load(Ordering::Relaxed)})
                );
                results.push((case, steady_missing + steady_priming, steady_dropped));
            }
        }
        assert_eq!(
            results[0],
            (0, 0, 0),
            "Normal5ms callbacks must remain continuous"
        );
        assert!(
            results
                .iter()
                .all(|(_, missing, dropped)| *missing == 0 && *dropped == 0),
            "Device-sized callbacks cut steady decoded speech: {results:?}"
        );
    }

    #[test]
    #[ignore = "Owned virtual ten-minute clock-drift traces; no audio devices or wall-clock endurance"]
    fn slight_output_clock_drift_does_not_cut_continuous_voice() {
        let mut encoder = crate::audio_settings::make_voice_encoder(96).unwrap();
        let mut packets = Vec::new();
        for frame in 0..100 {
            let pcm: Vec<f32> = (0..480)
                .map(|i| {
                    ((frame * 480 + i) as f32 * std::f32::consts::TAU * 440.0 / 48_000.0).sin()
                        * 0.2
                })
                .collect();
            let mut bytes = [0; 1500];
            let n = encoder.encode_float(&pcm, &mut bytes).unwrap();
            packets.push(bytes[..n].to_vec());
        }
        let mut results = Vec::new();
        for ppm in [0_i64, 100, -100] {
            let origin = Instant::now();
            let mut speakers = HashMap::from([(0, Speaker::new(origin).unwrap())]);
            let mut schedule = PlayoutSchedule::default();
            let queue = sample_queue(VecDeque::with_capacity(MAX_PLAYBACK_SAMPLES));
            let state = OutputState {
                playback_limit: Arc::new(AtomicUsize::new(MAX_PLAYBACK_SAMPLES)),
                counters: Arc::new(Counters::default()),
                queue: queue.clone(),
                deafened: Arc::new(AtomicBool::new(false)),
                errors: Arc::new(Mutex::new(None)),
                controls: Arc::new(AudioControls::new(&AudioSettings::default())),
                tones: Arc::new(Mutex::new(VecDeque::new())),
                stop: Arc::new(AtomicBool::new(false)),
            };
            let config = cpal::StreamConfig {
                channels: 1,
                sample_rate: cpal::SampleRate(AUDIO_SAMPLE_RATE),
                buffer_size: cpal::BufferSize::Default,
            };
            let missing_counter = Arc::new(AtomicU64::new(0));
            let priming_counter = Arc::new(AtomicU64::new(0));
            let observed_missing = missing_counter.clone();
            let observed_priming = priming_counter.clone();
            let mut output =
                output_callback_observed::<f32>(&config, state, move |missing, priming| {
                    observed_missing.fetch_add(missing as u64, Ordering::Relaxed);
                    observed_priming.fetch_add(priming as u64, Ordering::Relaxed);
                });
            let mut first_audio_ns = None;
            let mut source_frame = 0_u64;
            let mut output_block = 0_u64;
            let mut dropped_samples = 0_u64;
            let mut produced_samples = 0_u64;
            let mut consumed_samples = 0_u64;
            let mut peak_queue = 0;
            const END_NS: u64 = 600_000_000_000;
            loop {
                let source_ns = source_frame * 10_000_000;
                // Only the synthetic device clock changes: 240 frames per
                // callback, initially 3.75 ms after the first mixer quantum.
                let output_ns =
                    3_750_000 + output_block * 5_000_000_000_000 / (1_000_000 + ppm) as u64;
                let mixer_ns = schedule
                    .next
                    .map(|at| at.duration_since(origin).as_nanos() as u64)
                    .unwrap_or(source_ns);
                let ns = source_ns.min(output_ns).min(mixer_ns);
                if ns >= END_NS {
                    break;
                }
                let now = origin + Duration::from_nanos(ns);
                if ns == source_ns {
                    speakers.get_mut(&0).unwrap().push_timed(
                        source_frame + 1,
                        source_frame * 480,
                        source_frame == 0,
                        packets[source_frame as usize % packets.len()].clone(),
                        now,
                    );
                    source_frame += 1;
                }
                if schedule.due(now, &speakers, 10) {
                    let mut pcm = [0.0; 120];
                    let active = speakers
                        .get_mut(&0)
                        .unwrap()
                        .render_quantum(now, &mut pcm, 10);
                    assert!(
                        active,
                        "Paced source must always have a ready mixer quantum"
                    );
                    let mut queued = queue.lock().unwrap();
                    let excess = (queued.len() + pcm.len()).saturating_sub(MAX_PLAYBACK_SAMPLES);
                    dropped_samples += excess as u64;
                    assert_eq!(
                        append_playback_frame(&mut queued, &pcm, MAX_PLAYBACK_SAMPLES),
                        excess > 0
                    );
                    produced_samples += pcm.len() as u64;
                    peak_queue = peak_queue.max(queued.len());
                    schedule.complete_interval(now, active, Duration::from_micros(2500));
                }
                if ns == output_ns {
                    let available = queue.lock().unwrap().len();
                    let mut rendered = [0.0; 240];
                    let was_priming = priming_counter.load(Ordering::Relaxed);
                    output(&mut rendered);
                    if first_audio_ns.is_none()
                        && was_priming == priming_counter.load(Ordering::Relaxed)
                    {
                        first_audio_ns = Some(ns);
                    }
                    assert!(rendered.iter().all(|sample| sample.is_finite()));
                    let remaining = queue.lock().unwrap().len();
                    assert!(remaining <= available);
                    consumed_samples += (available - remaining) as u64;
                    output_block += 1;
                }
            }
            let missing_samples = missing_counter.load(Ordering::Relaxed);
            assert!(
                first_audio_ns.is_some_and(|ns| ns <= 10_000_000),
                "Priming must not hide gaps through indefinite silence"
            );
            assert_eq!(
                produced_samples,
                consumed_samples + dropped_samples + queue.lock().unwrap().len() as u64
            );
            assert_eq!(speakers[&0].stats.late, 0);
            assert_eq!(speakers[&0].stats.concealed, 0);
            assert!(peak_queue <= MAX_PLAYBACK_SAMPLES);
            let result = serde_json::json!({
                "output_ppm":ppm,"virtual_seconds":600,"source_frames":source_frame,"first_audio_ns":first_audio_ns,"initial_priming_samples":priming_counter.load(Ordering::Relaxed),
                "output_callbacks":output_block,"produced_samples":produced_samples,
                "consumed_samples":consumed_samples,"missing_samples":missing_samples,
                "dropped_samples":dropped_samples,"peak_queue_samples":peak_queue,
                "late_packets":speakers[&0].stats.late,"concealed_packets":speakers[&0].stats.concealed
            });
            println!("Owned output clock trace {result}");
            results.push((ppm, missing_samples, dropped_samples));
        }
        assert_eq!(results[0], (0, 0, 0), "Equal clocks must remain continuous");
        assert!(
            results
                .iter()
                .all(|(_, missing, dropped)| *missing == 0 && *dropped == 0),
            "Small independent output-clock offsets must not cut decoded speech: {results:?}"
        );
    }

    #[test]
    fn shared_playout_stall_has_one_frame_then_a_future_deadline_without_burst() {
        for late in [FRAME_TIME, Duration::from_millis(100)] {
            let now = Instant::now();
            let mut speaker = Speaker::new(now).unwrap();
            for sequence in 1..=6 {
                speaker.push_timed(
                    sequence,
                    (sequence - 1) * AUDIO_FRAME_SAMPLES as u64,
                    false,
                    playout_packet(),
                    now,
                );
            }
            let mut speakers = HashMap::from([(11, speaker)]);
            let mut schedule = PlayoutSchedule::default();
            let mut decoded = [0.0; AUDIO_FRAME_SAMPLES];
            assert!(schedule.due(now, &speakers, 20));
            assert!(speakers.get_mut(&11).unwrap().render(now, &mut decoded, 20));
            schedule.complete(now, true);
            let stalled = now + FRAME_TIME + late;
            assert!(schedule.due(stalled, &speakers, 20));
            assert!(speakers
                .get_mut(&11)
                .unwrap()
                .render(stalled, &mut decoded, 20));
            schedule.complete(stalled, true);
            assert_eq!(schedule.next, Some(stalled + FRAME_TIME));
            assert!(!schedule.due(stalled, &speakers, 20));
            assert!(!schedule.due(
                stalled + FRAME_TIME - Duration::from_millis(1),
                &speakers,
                20
            ));
            assert!(schedule.due(stalled + FRAME_TIME, &speakers, 20));
        }
    }
    #[test]
    fn idle_playout_restarts_from_new_source_readiness_instead_of_old_phase() {
        let now = Instant::now();
        let mut speaker = Speaker::new(now).unwrap();
        speaker.push_timed(1, 0, true, playout_packet(), now);
        let mut speakers = HashMap::from([(11, speaker)]);
        let mut schedule = PlayoutSchedule::default();
        assert!(schedule.due(now, &speakers, 20));
        let mut decoded = [0.0; AUDIO_FRAME_SAMPLES];
        assert!(speakers.get_mut(&11).unwrap().render(now, &mut decoded, 20));
        schedule.complete(now, true);
        for frame in 1..=4 {
            let at = now + FRAME_TIME * frame;
            assert!(schedule.due(at, &speakers, 20));
            let emitted = speakers.get_mut(&11).unwrap().render(at, &mut decoded, 20);
            assert_eq!(emitted, frame <= 3);
            schedule.complete(at, emitted);
        }
        assert!(!schedule.running && schedule.next.is_none());
        let resumed = now + Duration::from_millis(207);
        speakers
            .get_mut(&11)
            .unwrap()
            .push_timed(2, 9600, true, playout_packet(), resumed);
        assert!(schedule.due(resumed, &speakers, 20));
        assert!(speakers
            .get_mut(&11)
            .unwrap()
            .render(resumed, &mut decoded, 20));
        schedule.complete(resumed, true);
        assert_eq!(schedule.next, Some(resumed + FRAME_TIME));
    }
    #[test]
    fn supplied_cues_are_distinct_bounded_and_disabled_settings_open_no_devices() {
        let muted = tone_samples(AudioCue::Mute, 0.25);
        let unmuted = tone_samples(AudioCue::Unmute, 0.25);
        assert_ne!(muted, unmuted);
        assert!(muted.len() < MAX_TONE_SAMPLES);
        assert!(muted
            .iter()
            .all(|sample| sample.is_finite() && sample.abs() <= 0.25));
        assert_eq!(muted[0], 0.0);
        assert_eq!(muted.last().copied(), Some(0.0));
        let mut engine = MediaEngine::new().unwrap();
        engine.configure(AudioSettings {
            sounds_enabled: false,
            ..Default::default()
        });
        engine.play_tone(AudioCue::Join).unwrap();
        assert!(engine.owner.is_none());
        assert!(!engine.is_microphone_test_active() && !engine.is_active());
    }
    #[test]
    fn long_custom_events_and_repeated_appends_keep_a_bounded_cue_queue() {
        let mut queue = VecDeque::new();
        // Longer than the former one-second cap: previously drain(..excess)
        // indexed beyond an empty queue and panicked.
        append_tone(&mut queue, vec![0.25; AUDIO_SAMPLE_RATE as usize * 3]);
        assert_eq!(queue.len(), AUDIO_SAMPLE_RATE as usize * 3);
        append_tone(&mut queue, vec![0.5; AUDIO_SAMPLE_RATE as usize * 3]);
        assert_eq!(queue.len(), MAX_TONE_SAMPLES);
        assert_eq!(queue.front(), Some(&0.25));
        assert_eq!(queue.back(), Some(&0.5));
        append_tone(&mut queue, vec![0.75; MAX_TONE_SAMPLES + 1]);
        assert_eq!(queue.len(), MAX_TONE_SAMPLES);
        assert!(queue.iter().all(|sample| *sample == 0.75));
    }
    #[test]
    fn capture_resampling_keeps_a_fixed_queue_under_a_slow_consumer() {
        let mut resampler = InputResampler::new(8_000);
        let mut queue = VecDeque::with_capacity(CAPTURE_SAMPLES);
        let original_capacity = queue.capacity();
        let mut dropped = 0;
        for _ in 0..8_000 {
            dropped += resampler.feed_bounded(0.2, &mut queue, CAPTURE_SAMPLES);
        }
        assert_eq!(queue.len(), CAPTURE_SAMPLES);
        assert_eq!(queue.capacity(), original_capacity);
        assert!(dropped > 0);
    }
    #[test]
    fn authenticated_packet_requires_direction_session_and_untampered_header() {
        let cipher = ChaCha20Poly1305::new_from_slice(&[17; 32]).unwrap();
        let packet = seal_packet(&cipher, 41, 7, 0, &[1, 2, 3]).unwrap();
        assert_eq!(
            open_packet(&cipher, 41, 0, &packet),
            Some((7, vec![1, 2, 3]))
        );
        assert!(open_packet(&cipher, 42, 0, &packet).is_none());
        assert!(open_packet(&cipher, 41, 1, &packet).is_none());
        let mut tampered = packet;
        tampered[8] ^= 1;
        assert!(open_packet(&cipher, 41, 0, &tampered).is_none());
    }
    #[test]
    fn replay_window_allows_reordering_rejects_duplicates_and_old_packets() {
        let mut replay = ReplayWindow::default();
        assert!(replay.accept(10));
        assert!(replay.accept(8));
        assert!(!replay.accept(10));
        assert!(!replay.accept(8));
        assert!(replay.accept(200));
        assert!(!replay.accept(10));
        assert!(!replay.accept(0));
        assert!(replay.accept(199));
    }
    #[test]
    fn opus_frame_roundtrip_preserves_nonzero_voice() {
        let mut encoder = Encoder::new(48_000, Channels::Mono, Application::LowDelay).unwrap();
        encoder.set_bitrate(Bitrate::Bits(64_000)).unwrap();
        let mut decoder = Decoder::new(48_000, Channels::Mono).unwrap();
        let mut encoded = [0; 400];
        let mut decoded = [0.0; AUDIO_FRAME_SAMPLES];
        let input: Vec<f32> = (0..AUDIO_FRAME_SAMPLES)
            .map(|n| (n as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.2)
            .collect();
        let n = encoder.encode_float(&input, &mut encoded).unwrap();
        assert_eq!(
            decoder
                .decode_float(&encoded[..n], &mut decoded, false)
                .unwrap(),
            AUDIO_FRAME_SAMPLES
        );
        assert!(decoded.iter().any(|v| v.abs() > 0.05));
    }
    #[test]
    fn native_linked_opus_reports_low_delay_for_every_supported_packet_size() {
        let mut encoder = Encoder::new(48_000, Channels::Mono, Application::LowDelay).unwrap();
        encoder.set_bitrate(Bitrate::Bits(64_000)).unwrap();
        assert_eq!(encoder.get_application().unwrap(), Application::LowDelay);
        let lookahead = encoder.get_lookahead().unwrap();
        assert!((0..=120).contains(&lookahead));
        let mut encoded = [0; 400];
        let mut decoder = Decoder::new(48_000, Channels::Mono).unwrap();
        for samples in [120, 240, 480, 960] {
            let input: Vec<_> = (0..samples)
                .map(|n| (n as f32 * 0.07).sin() * 0.2)
                .collect();
            let count = encoder.encode_float(&input, &mut encoded).unwrap();
            let mut output = vec![0.; samples];
            assert_eq!(
                decoder
                    .decode_float(&encoded[..count], &mut output, false)
                    .unwrap(),
                samples
            );
            assert_eq!(encoder.get_lookahead().unwrap(), lookahead);
            assert!(output.iter().all(|sample| sample.is_finite()));
        }
        println!("Native linked Opus LowDelay: lookahead={lookahead} samples ({:.3} ms) at 48 kHz; 120/240/480/960-sample packets encoded and decoded", lookahead as f32 / 48.0);
    }

    #[test]
    fn shared_capture_callback_clears_muted_audio_before_fresh_capture() {
        let config = cpal::StreamConfig {
            channels: 1,
            sample_rate: cpal::SampleRate(48_000),
            buffer_size: cpal::BufferSize::Default,
        };
        let queue = sample_queue(VecDeque::new());
        let muted = Arc::new(AtomicBool::new(false));
        let state = InputState {
            queue: queue.clone(),
            level: Arc::new(AtomicU32::new(0)),
            muted: muted.clone(),
            deafened: Arc::new(AtomicBool::new(false)),
            counters: Arc::new(Counters::default()),
            errors: Arc::new(Mutex::new(None)),
            controls: Arc::new(AudioControls::new(&AudioSettings::default())),
            stop: Arc::new(AtomicBool::new(false)),
        };
        let mut callback = input_callback::<f32>(&config, state);
        callback(&[0.75; 480]);
        assert!(!queue.lock().unwrap().is_empty());
        muted.store(true, Ordering::Release);
        callback(&[0.9; 480]);
        assert!(queue.lock().unwrap().is_empty());
        muted.store(false, Ordering::Release);
        callback(&[0.25; 480]);
        assert!(queue.lock().unwrap().iter().all(|sample| *sample == 0.25));
    }
    #[test]
    fn resetting_discarded_opus_history_prevents_private_tail_in_the_next_packet() {
        for recovery in [false, true] {
            let mut encoder =
                crate::audio_settings::make_voice_encoder_with_recovery(64, recovery).unwrap();
            let mut decoder = Decoder::new(48_000, Channels::Mono).unwrap();
            encoder.set_bitrate(Bitrate::Bits(64_000)).unwrap();
            let mut encoded = [0; 400];
            let mut decoded = [0.0; AUDIO_FRAME_SAMPLES];
            let private: Vec<f32> = (0..AUDIO_FRAME_SAMPLES)
                .map(|n| (n as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.5)
                .collect();
            encoder.encode_float(&private, &mut encoded).unwrap(); // This frame never reaches a recipient.
            let size = encoder
                .encode_float(&[0.0; AUDIO_FRAME_SAMPLES], &mut encoded)
                .unwrap();
            decoder
                .decode_float(&encoded[..size], &mut decoded, false)
                .unwrap();
            let stale_peak = decoded
                .iter()
                .map(|sample| sample.abs())
                .fold(0.0f32, f32::max);
            assert!(
                stale_peak > 0.05,
                "fixture must expose the real low-delay codec lookahead"
            );
            encoder.encode_float(&private, &mut encoded).unwrap();
            let errors = Arc::new(Mutex::new(None));
            let stop = AtomicBool::new(false);
            discard_encoder_history(&mut encoder, &errors, &stop);
            assert!(!stop.load(Ordering::Acquire) && errors.lock().unwrap().is_none());
            let size = encoder
                .encode_float(&[0.0; AUDIO_FRAME_SAMPLES], &mut encoded)
                .unwrap();
            let mut fresh_decoder = Decoder::new(48_000, Channels::Mono).unwrap();
            fresh_decoder
                .decode_float(&encoded[..size], &mut decoded, false)
                .unwrap();
            let safe_peak = decoded
                .iter()
                .map(|sample| sample.abs())
                .fold(0.0f32, f32::max);
            assert!(
                safe_peak < 0.001,
                "discarded PCM must not survive reset: peak={safe_peak}"
            );
        }
    }
    #[test]
    fn actual_encrypted_worker_keeps_quiet_word_onset_before_manual_gate_opens() {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let (worker, stop, _) = probe_fixture(server.local_addr().unwrap());
        worker.muted.store(false, Ordering::Release);
        worker.deafened.store(false, Ordering::Release);
        worker
            .controls
            .transmission
            .store(TransmissionMode::VoiceActivation as u32, Ordering::Relaxed);
        worker.controls.suppression.store(false, Ordering::Relaxed);
        worker
            .controls
            .automatic_sensitivity
            .store(false, Ordering::Relaxed);
        worker
            .controls
            .automatic_microphone_volume
            .store(false, Ordering::Relaxed);
        worker
            .controls
            .threshold
            .store(0.1_f32.to_bits(), Ordering::Relaxed);
        worker.controls.packet_samples.store(480, Ordering::Relaxed);
        let capture = worker.capture.clone();
        let level = worker.level.clone();
        let errors = worker.errors.clone();
        let handle = thread::spawn(move || voice_worker(worker));
        let cipher = ChaCha20Poly1305::new_from_slice(&[23; 32]).unwrap();
        let mut inspector = CallCipher::new(test_call_keys("remote")).unwrap();
        let mut bytes = [0; UDP_MAX_PACKET];
        server.recv_from(&mut bytes).unwrap(); // initial authenticated presence
        capture
            .lock()
            .unwrap()
            .extend((0..480).map(|i| (i as f32 * 0.07).sin() * 0.03));
        let deadline = Instant::now() + Duration::from_secs(2);
        while level.load(Ordering::Acquire) == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        assert!(
            level.load(Ordering::Acquire) != 0,
            "quiet onset was not processed"
        );
        capture
            .lock()
            .unwrap()
            .extend((480..960).map(|i| (i as f32 * 0.07).sin() * 0.3));
        let mut timestamps = Vec::new();
        let mut decoder = Decoder::new(48000, Channels::Mono).unwrap();
        let mut onset = [0.0; 480];
        while timestamps.len() < 2 {
            let (n, _) = server.recv_from(&mut bytes).unwrap();
            let (_, plain) = open_packet(&cipher, 7, 0, &bytes[..n]).unwrap();
            if plain[0] != AUDIO_PACKET_V3 {
                continue;
            }
            let frame = inspector.open(&plain).unwrap();
            timestamps.push(frame.timestamp);
            if timestamps.len() == 1 {
                decoder
                    .decode_float(&frame.opus, &mut onset, false)
                    .unwrap();
            }
        }
        stop.store(true, Ordering::Release);
        handle.join().unwrap();
        assert!(errors.lock().unwrap().is_none());
        assert_eq!(timestamps, vec![0, 480]);
        assert!(
            onset.iter().map(|v| v * v).sum::<f32>() > 0.001,
            "quiet onset was replaced by silence"
        );
    }

    #[test]
    fn packet_duration_changes_keep_protected_clock_and_sequences_monotonic() {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let (worker, stop, _) = probe_fixture(server.local_addr().unwrap());
        worker.muted.store(false, Ordering::Release);
        worker.deafened.store(false, Ordering::Release);
        let controls = worker.controls.clone();
        controls
            .transmission
            .store(TransmissionMode::Continuous as u32, Ordering::Relaxed);
        controls.suppression.store(false, Ordering::Relaxed);
        let capture = worker.capture.clone();
        let errors = worker.errors.clone();
        let handle = thread::spawn(move || voice_worker(worker));
        let cipher = ChaCha20Poly1305::new_from_slice(&[23; 32]).unwrap();
        let mut inspector = CallCipher::new(test_call_keys("remote")).unwrap();
        let mut expected_clock = 0;
        let mut previous_sequence = 0;
        for (samples, recovery) in [
            (960, false),
            (120, true),
            (240, true),
            (480, true),
            (960, true),
            (480, false),
        ] {
            controls
                .recover_packet_loss
                .store(recovery, Ordering::Relaxed);
            controls.packet_samples.store(samples, Ordering::Relaxed);
            // One full packet; no hardware or personal audio is opened.
            capture.lock().unwrap().extend(vec![0.2; samples as usize]);
            let frame = loop {
                let mut bytes = [0; UDP_MAX_PACKET];
                let (n, _) = server.recv_from(&mut bytes).unwrap();
                let (_, plain) = open_packet(&cipher, 7, 0, &bytes[..n]).unwrap();
                if plain[0] == AUDIO_PACKET_V3 {
                    break inspector.open(&plain).unwrap();
                }
            };
            assert_eq!(
                opus::packet::get_nb_samples(&frame.opus, 48_000).unwrap(),
                samples as usize
            );
            assert_eq!(frame.timestamp, expected_clock);
            assert_eq!(frame.sequence, previous_sequence + 1);
            assert!(frame.restart);
            expected_clock += u64::from(samples);
            previous_sequence = frame.sequence;
        }
        stop.store(true, Ordering::Release);
        handle.join().unwrap();
        assert!(errors.lock().unwrap().is_none());
    }

    #[test]
    fn short_capture_preserves_dsp_blocks_and_voice_activity_hold_duration() {
        for samples in [120, 240, 480, 960] {
            let controls = AudioControls::new(&AudioSettings {
                noise_suppression: false,
                activation_hold_ms: 100,
                activation_release_ms: 0,
                ..Default::default()
            });
            assert_eq!(capture_processing_samples(samples, &controls), samples);
            let mut processor = MicrophoneProcessor::new();
            assert!(processor.process(&mut vec![0.2; samples], &controls));
            let mut elapsed = 0;
            while processor.process(&mut vec![0.; samples], &controls) {
                elapsed += samples;
            }
            assert_eq!(elapsed, 4800);
            controls.suppression.store(true, Ordering::Relaxed);
            assert_eq!(
                capture_processing_samples(samples, &controls),
                samples.max(480)
            );
        }
    }

    #[test]
    fn settings_delay_budget_matches_real_capture_batch_sizes() {
        for packet_samples in [120, 240, 480, 960] {
            for noise_suppression in [false, true] {
                for echo_cancellation in [false, true] {
                    let mut settings = AudioSettings {
                        noise_suppression,
                        echo_cancellation,
                        ..Default::default()
                    };
                    settings.set_packet_samples(packet_samples);
                    let controls = AudioControls::new(&settings);
                    assert_eq!(
                        (settings.voice_delay_budget().capture_ms * 48.0) as usize,
                        capture_processing_samples(packet_samples, &controls)
                    );
                }
            }
        }
    }

    #[test]
    fn encrypted_udp_worker_sends_and_plays_voice_without_devices() {
        for packet_ms in [5, 10, 20] {
            let samples = packet_ms as usize * 48;
            let server = UdpSocket::bind("127.0.0.1:0").unwrap();
            server
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let remote = server.local_addr().unwrap();
            let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
            socket.connect(remote).unwrap();
            socket.set_nonblocking(true).unwrap();
            let cipher = ChaCha20Poly1305::new_from_slice(&[23; 32]).unwrap();
            let input: Vec<f32> = (0..AUDIO_FRAME_SAMPLES)
                .map(|n| (n as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.2)
                .collect();
            let capture = sample_queue(input.iter().copied().collect());
            let playback = sample_queue(VecDeque::new());
            let stop = Arc::new(AtomicBool::new(false));
            let counters = Arc::new(Counters::default());
            let worker = VoiceWorker {
                gateway: None,
                playback_limit: Arc::new(AtomicUsize::new(MAX_PLAYBACK_SAMPLES)),
                socket,
                remote,
                session_id: 7,
                wire_version: 3,
                cipher: cipher.clone(),
                call: CallCipher::new(test_call_keys("local")).unwrap(),
                encoder: Encoder::new(48_000, Channels::Mono, Application::LowDelay).unwrap(),
                capture,
                playback: playback.clone(),
                stop: stop.clone(),
                muted: Arc::new(AtomicBool::new(false)),
                deafened: Arc::new(AtomicBool::new(false)),
                counters: counters.clone(),
                errors: Arc::new(Mutex::new(None)),
                controls: Arc::new(AudioControls::new(&AudioSettings {
                    packet_ms,
                    transmission: TransmissionMode::Continuous,
                    noise_suppression: false,
                    ..Default::default()
                })),
                level: Arc::new(AtomicU32::new(0)),
            };
            let handle = thread::spawn(move || voice_worker(worker));
            let mut bytes = [0; UDP_MAX_PACKET];
            let mut inspector = CallCipher::new(test_call_keys("remote")).unwrap();
            let client = loop {
                let (n, client) = server.recv_from(&mut bytes).unwrap();
                let (_, plain) = open_packet(&cipher, 7, 0, &bytes[..n]).unwrap();
                if plain[0] == AUDIO_PACKET_V3 {
                    let frame = inspector.open(&plain).unwrap();
                    assert_eq!(
                        opus::packet::get_nb_samples(&frame.opus, 48_000).unwrap(),
                        samples
                    );
                    assert_eq!(frame.timestamp, 0);
                    break client;
                }
            };
            let mut encoder = Encoder::new(48_000, Channels::Mono, Application::LowDelay).unwrap();
            let mut encoded = [0; 400];
            let remote_call = CallCipher::new(test_call_keys("remote")).unwrap();
            for seq in 1..=4 {
                let len = encoder
                    .encode_float(&input[..samples], &mut encoded)
                    .unwrap();
                let plain = remote_call
                    .seal(
                        99,
                        seq,
                        (seq - 1) * samples as u64,
                        seq == 1,
                        &encoded[..len],
                    )
                    .unwrap();
                let packet = seal_packet(&cipher, 7, seq, 1, &plain).unwrap();
                server.send_to(&packet, client).unwrap();
                if seq == 2 {
                    server.send_to(&packet, client).unwrap();
                }
            }
            // Wrong nonce direction must fail authentication without advancing replay state.
            server
                .send_to(&seal_packet(&cipher, 7, 5, 0, &[0]).unwrap(), client)
                .unwrap();
            let until = Instant::now() + Duration::from_secs(1);
            while Instant::now() < until {
                if playback.lock().unwrap().iter().any(|x| x.abs() > 0.01)
                    && counters.rejected.load(Ordering::Relaxed) >= 2
                {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
            stop.store(true, Ordering::Release);
            handle.join().unwrap();
            assert!(playback.lock().unwrap().iter().any(|x| x.abs() > 0.01));
            assert_eq!(counters.received.load(Ordering::Relaxed), 4);
            assert_eq!(counters.rejected.load(Ordering::Relaxed), 2);
        }
    }
    pub(super) fn probe_fixture(
        remote: SocketAddr,
    ) -> (VoiceWorker, Arc<AtomicBool>, Arc<Counters>) {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.connect(remote).unwrap();
        socket.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let counters = Arc::new(Counters::default());
        (
            VoiceWorker {
                gateway: None,
                playback_limit: Arc::new(AtomicUsize::new(MAX_PLAYBACK_SAMPLES)),
                socket,
                remote,
                session_id: 7,
                wire_version: 3,
                cipher: ChaCha20Poly1305::new_from_slice(&[23; 32]).unwrap(),
                call: CallCipher::new(test_call_keys("local")).unwrap(),
                encoder: Encoder::new(48_000, Channels::Mono, Application::LowDelay).unwrap(),
                capture: sample_queue(VecDeque::new()),
                playback: sample_queue(VecDeque::new()),
                stop: stop.clone(),
                muted: Arc::new(AtomicBool::new(true)),
                deafened: Arc::new(AtomicBool::new(true)),
                counters: counters.clone(),
                errors: Arc::new(Mutex::new(None)),
                controls: Arc::new(test_audio_controls()),
                level: Arc::new(AtomicU32::new(0)),
            },
            stop,
            counters,
        )
    }

    #[test]
    fn actual_voice_workers_use_direct_encrypted_opus_and_return_to_relay_with_privacy_gates() {
        use relay_protocol::direct_voice::{self, Peer, Roster};
        struct Running(Vec<(Arc<AtomicBool>, JoinHandle<()>)>);
        impl Drop for Running {
            fn drop(&mut self) {
                for (stop, _) in &self.0 {
                    stop.store(true, Ordering::Release);
                }
                for (_, handle) in self.0.drain(..) {
                    handle.join().unwrap();
                }
            }
        }
        let relay = UdpSocket::bind("127.0.0.1:0").unwrap();
        relay
            .set_read_timeout(Some(Duration::from_millis(10)))
            .unwrap();
        let address = relay.local_addr().unwrap();
        let relayed_audio = Arc::new(AtomicU64::new(0));
        let server_stop = Arc::new(AtomicBool::new(false));
        let server_end = server_stop.clone();
        let server_audio = relayed_audio.clone();
        let mut running = Running(Vec::new());
        running.0.push((
            server_stop,
            thread::spawn(move || {
                let cipher = ChaCha20Poly1305::new_from_slice(&[23; 32]).unwrap();
                let mut members: HashMap<u64, (SocketAddr, bool)> = HashMap::new();
                let mut sequence = 1;
                let mut bytes = [0; UDP_MAX_PACKET];
                while !server_end.load(Ordering::Acquire) {
                    let Ok((n, from)) = relay.recv_from(&mut bytes) else {
                        continue;
                    };
                    if n < 16 {
                        continue;
                    }
                    let id = u64::from_le_bytes(bytes[..8].try_into().unwrap());
                    let Some((_, clear)) = open_packet(&cipher, id, 0, &bytes[..n]) else {
                        continue;
                    };
                    let mut replies = Vec::new();
                    if clear == [0] {
                        members.entry(id).or_insert((from, false));
                    }
                    if clear.first() == Some(&UDP_PING) {
                        // A local fixture models a slower relay route without external servers.
                        thread::sleep(Duration::from_millis(6));
                        replies.push((id, from, clear.clone()));
                    }
                    if let Some((request, enabled, _)) = direct_voice::parse_request(&clear) {
                        members.entry(id).or_insert((from, false)).1 = enabled;
                        let other = if id == 7 { 8 } else { 7 };
                        let peer = members.get(&other).filter(|(_, opt)| enabled && *opt).map(
                            |(address, _)| Peer {
                                session: other,
                                user: if other == 7 { "local" } else { "remote" }.into(),
                                address: *address,
                                key: [77; 32],
                                relay_rtt_us: 6000,
                                send_allowed: true,
                                receive_allowed: true,
                            },
                        );
                        let mut response = vec![direct_voice::ROSTER];
                        response.extend(serde_json::to_vec(&Roster { request, peer }).unwrap());
                        replies.push((id, from, response));
                    }
                    if clear.first() == Some(&AUDIO_PACKET_V3) {
                        server_audio.fetch_add(1, Ordering::Relaxed);
                        for (&other, &(address, _)) in &members {
                            if other != id {
                                replies.push((other, address, clear.clone()));
                            }
                        }
                    }
                    for (id, address, clear) in replies {
                        let packet = seal_packet(&cipher, id, sequence, 1, &clear).unwrap();
                        sequence += 1;
                        relay.send_to(&packet, address).unwrap();
                    }
                }
            }),
        ));
        let (mut a, stop_a, counters_a) = probe_fixture(address);
        let (mut b, stop_b, counters_b) = probe_fixture(address);
        for worker in [&mut a, &mut b] {
            worker.socket = UdpSocket::bind("127.0.0.1:0").unwrap();
            worker.socket.set_nonblocking(true).unwrap();
            worker.wire_version = 4;
            worker.muted.store(false, Ordering::Release);
            worker.deafened.store(false, Ordering::Release);
            worker
                .controls
                .direct_enabled
                .store(true, Ordering::Release);
            worker
                .controls
                .transmission
                .store(TransmissionMode::Continuous as u32, Ordering::Relaxed);
            worker.controls.suppression.store(false, Ordering::Relaxed);
            worker.controls.packet_samples.store(120, Ordering::Relaxed);
        }
        b.session_id = 8;
        b.call = CallCipher::new(test_call_keys("remote")).unwrap();
        let capture = a.capture.clone();
        let playback = b.playback.clone();
        let muted = a.muted.clone();
        let controls_a = a.controls.clone();
        let controls_b = b.controls.clone();
        let errors_a = a.errors.clone();
        let errors_b = b.errors.clone();
        running
            .0
            .push((stop_a, thread::spawn(move || voice_worker(a))));
        running
            .0
            .push((stop_b, thread::spawn(move || voice_worker(b))));
        let wait = |label: &str, condition: &dyn Fn() -> bool| {
            let until = Instant::now() + Duration::from_secs(3);
            while !condition() && Instant::now() < until {
                thread::sleep(Duration::from_millis(2));
            }
            assert!(
                condition(),
                "{label}: worker condition timed out; A={:?}, B={:?}, relayed={}, errors={:?}/{:?}",
                counters_a.snapshot(),
                counters_b.snapshot(),
                relayed_audio.load(Ordering::Relaxed),
                errors_a.lock().unwrap(),
                errors_b.lock().unwrap()
            );
        };
        wait("direct selected", &|| {
            counters_a.direct_active.load(Ordering::Acquire)
                && counters_b.direct_active.load(Ordering::Acquire)
        });
        capture.lock().unwrap().extend(vec![0.2; 2400]);
        controls_a.wake.notify();
        wait("decoded audio", &|| {
            playback
                .lock()
                .unwrap()
                .iter()
                .any(|sample| sample.abs() > 0.01)
        });
        assert_eq!(
            relayed_audio.load(Ordering::Relaxed),
            0,
            "proven direct audio bypasses relay forwarding"
        );
        muted.store(true, Ordering::Release);
        controls_a.privacy_generation.fetch_add(1, Ordering::AcqRel);
        thread::sleep(Duration::from_millis(15));
        let received = counters_b.received.load(Ordering::Relaxed);
        capture.lock().unwrap().extend(vec![0.9; 2400]);
        thread::sleep(Duration::from_millis(30));
        assert_eq!(
            counters_b.received.load(Ordering::Relaxed),
            received,
            "mute blocks direct audio too"
        );
        controls_b.direct_enabled.store(false, Ordering::Release);
        wait("fallback selected", &|| {
            !counters_a.direct_active.load(Ordering::Acquire)
        });
        playback.lock().unwrap().clear();
        capture.lock().unwrap().push_back(0.0); // Partial stale block must be flushed on the privacy transition.
        muted.store(false, Ordering::Release);
        controls_a.privacy_generation.fetch_add(1, Ordering::AcqRel);
        controls_a.wake.notify();
        wait("privacy flush", &|| capture.lock().unwrap().is_empty());
        capture.lock().unwrap().extend(vec![0.2; 2400]);
        controls_a.wake.notify();
        wait("audio relayed", &|| {
            relayed_audio.load(Ordering::Relaxed) > 0
        });
        wait("decoded audio", &|| {
            playback
                .lock()
                .unwrap()
                .iter()
                .any(|sample| sample.abs() > 0.01)
        });
        assert!(errors_a.lock().unwrap().is_none() && errors_b.lock().unwrap().is_none());
        println!("Actual encrypted 2.5 ms Opus workers: direct route, mute gate and server fallback verified on isolated localhost sockets; no physical audio devices.");
    }

    #[test]
    fn actual_device_free_tcp_fallback_returns_to_udp_without_counter_reset() {
        exercise_tcp_fallback(false);
    }

    #[test]
    fn actual_device_free_iroh_voice_recovers_without_enabling_the_tcp_setting() {
        exercise_tcp_fallback(true);
    }

    #[test]
    fn automatic_fallback_survives_settings_changes_and_does_not_follow_a_normal_call() {
        let settings = AudioSettings::default();
        assert!(!settings.tcp_fallback_enabled);
        let controls = AudioControls::new(&settings);
        let address = format!("http://{}.iroh:4580", "ab".repeat(32));
        controls.set_voice_connection(&address, true);
        controls.apply(&settings);
        assert!(controls.tcp_allowed());
        assert!(!controls.direct_enabled.load(Ordering::Acquire));
        controls.set_voice_connection("http://127.0.0.1:4580", false);
        controls.apply(&settings);
        assert!(!controls.tcp_allowed());
    }

    fn exercise_tcp_fallback(account_free: bool) {
        use std::net::TcpListener;
        let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
        udp.set_nonblocking(true).unwrap();
        let tcp = TcpListener::bind("127.0.0.1:0").unwrap();
        let tcp_address = tcp.local_addr().unwrap();
        let (worker, stop, counters) = probe_fixture(udp.local_addr().unwrap());
        worker.muted.store(false, Ordering::Release);
        worker.deafened.store(false, Ordering::Release);
        worker
            .controls
            .tcp_enabled
            .store(!account_free, Ordering::Release);
        if account_free {
            worker
                .controls
                .set_voice_connection(&format!("http://{}.iroh:4580", "ab".repeat(32)), true);
        }
        *worker.controls.tunnel_config.lock().unwrap() = Some(crate::voice_tunnel::Config {
            url: format!("ws://{tcp_address}/owned"),
            address: tcp_address,
            session: 7,
            key: STANDARD.encode([23; 32]),
        });
        let server_stop = Arc::new(AtomicBool::new(false));
        let udp_enabled = Arc::new(AtomicBool::new(false));
        let key = worker.cipher.clone();
        let output_sequence = Arc::new(AtomicU64::new(1));
        let replay = Arc::new(Mutex::new(ReplayWindow::default()));
        let udp_stop = server_stop.clone();
        let enabled = udp_enabled.clone();
        let udp_key = key.clone();
        let udp_out = output_sequence.clone();
        let udp_replay = replay.clone();
        let udp_owner = thread::spawn(move || {
            let mut bytes = [0u8; UDP_MAX_PACKET];
            while !udp_stop.load(Ordering::Acquire) {
                if let Ok((size, source)) = udp.recv_from(&mut bytes) {
                    if enabled.load(Ordering::Acquire) {
                        let clear = {
                            let mut replay = udp_replay.lock().unwrap();
                            open_packet(&udp_key, 7, 0, &bytes[..size]).and_then(
                                |(sequence, clear)| replay.accept(sequence).then_some(clear),
                            )
                        };
                        if let Some(clear) = clear.filter(|clear| clear.first() == Some(&UDP_PING))
                        {
                            let sequence = udp_out.fetch_add(1, Ordering::AcqRel);
                            udp.send_to(
                                &seal_packet(&udp_key, 7, sequence, 1, &clear).unwrap(),
                                source,
                            )
                            .unwrap();
                        }
                    }
                }
                thread::sleep(Duration::from_millis(2));
            }
        });
        let tcp_stop = server_stop.clone();
        let tcp_owner = thread::spawn(move || {
            tcp.set_nonblocking(true).unwrap();
            let mut stream = None;
            let until = Instant::now() + Duration::from_secs(12);
            while !tcp_stop.load(Ordering::Acquire) && Instant::now() < until {
                if let Ok((connection, _)) = tcp.accept() {
                    stream = Some(connection);
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
            let Some(stream) = stream else {
                return;
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            socket
                .get_mut()
                .set_read_timeout(Some(Duration::from_millis(10)))
                .unwrap();
            // The real server ignores a probe until a keepalive has pinned the
            // ticket's transport. An unconditional echo hid this startup bug.
            let mut registered = false;
            while !tcp_stop.load(Ordering::Acquire) {
                match socket.read() {
                    Ok(tungstenite::Message::Binary(packet)) => {
                        let clear = {
                            let mut replay = replay.lock().unwrap();
                            open_packet(&key, 7, 0, &packet).and_then(|(sequence, clear)| {
                                replay.accept(sequence).then_some(clear)
                            })
                        };
                        if clear.as_deref() == Some(&[0]) {
                            registered = true;
                        }
                        if let Some(clear) =
                            clear.filter(|clear| registered && clear.first() == Some(&UDP_PING))
                        {
                            let sequence = output_sequence.fetch_add(1, Ordering::AcqRel);
                            if socket
                                .send(tungstenite::Message::Binary(
                                    seal_packet(&key, 7, sequence, 1, &clear).unwrap(),
                                ))
                                .is_err()
                            {
                                break;
                            }
                        }
                    }
                    Err(tungstenite::Error::Io(error))
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                        ) => {}
                    _ => break,
                }
            }
        });
        let controls = worker.controls.clone();
        let voice = thread::spawn(move || voice_worker(worker));
        let until = Instant::now() + Duration::from_secs(10);
        while !counters.snapshot().tcp_active && Instant::now() < until {
            thread::sleep(Duration::from_millis(10));
        }
        let fallback = counters.snapshot();
        udp_enabled.store(true, Ordering::Release);
        let until = Instant::now() + Duration::from_secs(5);
        while (!counters.snapshot().udp_reachable || counters.snapshot().tcp_active)
            && Instant::now() < until
        {
            thread::sleep(Duration::from_millis(10));
        }
        let recovered = counters.snapshot();
        stop.store(true, Ordering::Release);
        voice.join().unwrap();
        controls.tunnel.lock().unwrap().take();
        server_stop.store(true, Ordering::Release);
        udp_owner.join().unwrap();
        tcp_owner.join().unwrap();
        assert!(
            fallback.tcp_active && !fallback.udp_reachable,
            "fallback: {fallback:?}"
        );
        assert!(
            recovered.udp_reachable && !recovered.tcp_active,
            "recovered: {recovered:?}"
        );
        assert!(recovered.received_packets > fallback.received_packets);
    }
    #[test]
    fn one_encrypted_packet_plays_while_capture_is_locked_and_has_no_complete_frame() {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let (worker, stop, counters) = probe_fixture(server.local_addr().unwrap());
        worker.deafened.store(false, Ordering::Release);
        worker.controls.jitter_ms.store(20, Ordering::Relaxed);
        let cipher = worker.cipher.clone();
        let playback = worker.playback.clone();
        let capture = worker.capture.clone();
        // A delayed input callback must not hold receive playout hostage.
        // No physical capture or playback stream is constructed by this fixture.
        let capture_guard = capture.lock().unwrap();
        let handle = thread::spawn(move || voice_worker(worker));
        let mut bytes = [0; UDP_MAX_PACKET];
        let (_, client) = server.recv_from(&mut bytes).unwrap();
        // Even a relay that knows the outer transport key cannot downgrade a
        // protected call to either historical plaintext Opus format.
        let mut legacy = vec![1];
        legacy.extend_from_slice(&99_u64.to_le_bytes());
        legacy.extend_from_slice(&playout_packet());
        server
            .send_to(&seal_packet(&cipher, 7, 1, 1, &legacy).unwrap(), client)
            .unwrap();
        let mut legacy_v2 = vec![AUDIO_PACKET_V2];
        legacy_v2.extend_from_slice(&99_u64.to_le_bytes());
        legacy_v2.extend_from_slice(&1_u64.to_le_bytes());
        legacy_v2.extend_from_slice(&0_u64.to_le_bytes());
        legacy_v2.push(1);
        legacy_v2.extend_from_slice(&playout_packet());
        server
            .send_to(&seal_packet(&cipher, 7, 2, 1, &legacy_v2).unwrap(), client)
            .unwrap();
        let remote_call = CallCipher::new(test_call_keys("remote")).unwrap();
        let plain = remote_call.seal(99, 1, 0, true, &playout_packet()).unwrap();
        server
            .send_to(&seal_packet(&cipher, 7, 3, 1, &plain).unwrap(), client)
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut played = false;
        while Instant::now() < deadline {
            played = playback
                .lock()
                .unwrap()
                .iter()
                .any(|sample| sample.abs() > 0.01);
            if played {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        stop.store(true, Ordering::Release);
        drop(capture_guard);
        handle.join().unwrap();
        assert!(
            played,
            "one source packet must play without microphone readiness"
        );
        assert_eq!(counters.received.load(Ordering::Relaxed), 3);
        assert_eq!(counters.rejected.load(Ordering::Relaxed), 2);
    }
    #[test]
    fn encrypted_transport_probe_measures_authenticated_rtt_even_when_muted_without_devices() {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let (worker, stop, counters) = probe_fixture(server.local_addr().unwrap());
        let cipher = worker.cipher.clone();
        let handle = thread::spawn(move || voice_worker(worker));
        let mut packet = [0; UDP_MAX_PACKET];
        let (client, probe) = loop {
            let (length, client) = server.recv_from(&mut packet).unwrap();
            let (_, plain) = open_packet(&cipher, 7, 0, &packet[..length]).unwrap();
            if plain.first() == Some(&UDP_PING) {
                break (client, plain);
            }
        };
        // Authentication failure cannot establish reachability or burn the echo nonce.
        server
            .send_to(&seal_packet(&cipher, 7, 1, 0, &probe).unwrap(), client)
            .unwrap();
        server
            .send_to(&seal_packet(&cipher, 7, 1, 1, &probe).unwrap(), client)
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while !counters.snapshot().udp_reachable {
            assert!(
                Instant::now() < deadline,
                "Authenticated ping reply was not accepted"
            );
            thread::sleep(Duration::from_millis(2));
        }
        stop.store(true, Ordering::Release);
        handle.join().unwrap();
        let stats = counters.snapshot();
        assert!(stats
            .udp_round_trip_ms
            .is_some_and(|rtt| rtt > 0.0 && rtt < 1000.0));
        assert!(!stats.udp_probe_timed_out);
        assert_eq!(stats.rejected_packets, 1);
    }
    #[test]
    fn delayed_authenticated_probe_replies_do_not_look_like_an_offline_call() {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        server.set_nonblocking(true).unwrap();
        let (worker, stop, counters) = probe_fixture(server.local_addr().unwrap());
        let cipher = worker.cipher.clone();
        let errors = worker.errors.clone();
        let handle = thread::spawn(move || voice_worker(worker));
        let started = Instant::now();
        let result = (|| -> Result<_, String> {
            let mut queued = VecDeque::new();
            let mut bytes = [0; UDP_MAX_PACKET];
            let mut echoed = 0_u64;
            let mut probes_queued = 0_u64;
            let mut first_echo = None;
            loop {
                for _ in 0..32 {
                    match server.recv_from(&mut bytes) {
                        Ok((n, client)) => {
                            let (_, payload) = open_packet(&cipher, 7, 0, &bytes[..n])
                                .ok_or("Owned outgoing probe authentication failed")?;
                            if payload.len() == 9 && payload[0] == UDP_PING {
                                probes_queued += 1;
                                let delay = if probes_queued == 1 { 2300 } else { 2600 };
                                queued.push_back((
                                    Instant::now() + Duration::from_millis(delay),
                                    client,
                                    payload,
                                ));
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(e) => return Err(format!("Owned UDP receive failed: {e}")),
                    }
                }
                while queued
                    .front()
                    .is_some_and(|(at, _, _)| Instant::now() >= *at)
                {
                    let (_, client, payload) = queued.pop_front().unwrap();
                    echoed += 1;
                    let packet = seal_packet(&cipher, 7, echoed, 1, &payload)
                        .ok_or("Owned delayed probe could not be sealed")?;
                    server.send_to(&packet, client).map_err(|e| e.to_string())?;
                    first_echo.get_or_insert_with(Instant::now);
                }
                let stats = counters.snapshot();
                if echoed >= 2
                    && stats.udp_reachable
                    && stats.udp_round_trip_ms.is_some_and(|rtt| rtt >= 2500.0)
                {
                    if !stats
                        .udp_round_trip_ms
                        .is_some_and(|rtt| (2200.0..4900.0).contains(&rtt))
                    {
                        return Err(format!(
                            "Delayed probe RTT did not match injected delay: {:?}",
                            stats.udp_round_trip_ms
                        ));
                    }
                    return Ok((echoed, stats.udp_round_trip_ms, stats.udp_probe_timed_out));
                }
                if first_echo.is_some_and(|at| at.elapsed() > Duration::from_millis(500))
                    && !stats.udp_reachable
                {
                    return Err("A valid authenticated 2.3-second reply was ignored after a newer probe was sent".into());
                }
                if started.elapsed() > Duration::from_secs(7) {
                    return Err("Owned delayed probe check exceeded seven seconds".into());
                }
                thread::sleep(Duration::from_millis(2));
            }
        })();
        stop.store(true, Ordering::Release);
        handle.join().unwrap();
        assert!(errors.lock().unwrap().is_none());
        let (echoes, rtt, timed_out) = result.unwrap();
        assert!(!timed_out);
        println!("Owned delayed UDP probes: {echoes} authenticated replies accepted, RTT={rtt:?} ms, no devices or WAN");
    }
    #[test]
    fn udp_link_snapshot_distinguishes_checking_connected_recovery_and_expiry() {
        use crate::voice_recovery::LinkState;
        let counters = Counters::default();
        for (state, expected) in [
            (LinkState::Checking, (false, false, None)),
            (LinkState::Connected, (true, false, None)),
            (
                LinkState::Reconnecting {
                    remaining: Duration::from_secs(30),
                },
                (false, true, Some(30_000)),
            ),
            (
                LinkState::Reconnecting {
                    remaining: Duration::ZERO,
                },
                (false, true, Some(1)),
            ),
            (
                LinkState::Reconnecting {
                    remaining: Duration::MAX,
                },
                (false, true, Some(u32::MAX)),
            ),
            (LinkState::Expired, (false, true, None)),
        ] {
            counters.publish_udp_link(state);
            let stats = counters.snapshot();
            assert_eq!(
                (
                    stats.udp_reachable,
                    stats.udp_probe_timed_out,
                    stats.udp_recovery_remaining_ms,
                ),
                expected
            );
            assert_eq!(counters.udp_reachable(), expected.0);
        }
    }
    #[test]
    fn udp_link_snapshot_remains_coherent_during_concurrent_transition_publication() {
        use crate::voice_recovery::LinkState;
        let counters = Arc::new(Counters::default());
        counters.publish_udp_link(LinkState::Connected);
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let writer_counters = counters.clone();
        let writer_barrier = barrier.clone();
        let writer = thread::spawn(move || {
            writer_barrier.wait();
            for _ in 0..100_000 {
                writer_counters.publish_udp_link(LinkState::Reconnecting {
                    remaining: Duration::from_secs(30),
                });
                writer_counters.publish_udp_link(LinkState::Connected);
            }
        });
        barrier.wait();
        for _ in 0..100_000 {
            let stats = counters.snapshot();
            assert!(matches!(
                (
                    stats.udp_reachable,
                    stats.udp_probe_timed_out,
                    stats.udp_recovery_remaining_ms,
                ),
                (true, false, None) | (false, true, Some(30_000))
            ));
        }
        writer.join().unwrap();
    }
    #[test]
    fn stopping_udp_link_clears_live_state_and_preserves_timeout_diagnosis() {
        use crate::voice_recovery::LinkState;
        let counters = Counters::default();
        for state in [
            LinkState::Checking,
            LinkState::Connected,
            LinkState::Reconnecting {
                remaining: Duration::from_secs(30),
            },
            LinkState::Expired,
        ] {
            counters.publish_udp_link(state);
            let timed_out = counters.snapshot().udp_probe_timed_out;
            counters.stop_udp_link();
            let stats = counters.snapshot();
            assert!(!stats.udp_reachable);
            assert!(stats.udp_recovery_remaining_ms.is_none());
            assert_eq!(stats.udp_probe_timed_out, timed_out);
        }
    }
    #[test]
    fn transport_blackhole_is_reported_without_opening_audio_devices() {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        let (worker, stop, counters) = probe_fixture(server.local_addr().unwrap());
        let errors = worker.errors.clone();
        let handle = thread::spawn(move || voice_worker(worker));
        let deadline = Instant::now() + Duration::from_millis(5700);
        while !counters.snapshot().udp_probe_timed_out {
            assert!(Instant::now() < deadline, "UDP blackhole was not diagnosed");
            thread::sleep(Duration::from_millis(10));
        }
        assert!(counters.snapshot().udp_recovery_remaining_ms.is_some());
        stop.store(true, Ordering::Release);
        handle.join().unwrap();
        let stats = counters.snapshot();
        assert!(!stats.udp_reachable && stats.udp_round_trip_ms.is_none());
        assert!(stats.sent_packets > 0 && errors.lock().unwrap().is_none());
        assert!(stats.udp_recovery_remaining_ms.is_none());
    }
    #[test]
    fn cancelled_voice_worker_never_sends_its_initial_keepalive_or_captured_speech() {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        server
            .set_read_timeout(Some(Duration::from_millis(80)))
            .unwrap();
        let remote = server.local_addr().unwrap();
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.connect(remote).unwrap();
        socket.set_nonblocking(true).unwrap();
        let counters = Arc::new(Counters::default());
        let worker = VoiceWorker {
            gateway: None,
            playback_limit: Arc::new(AtomicUsize::new(MAX_PLAYBACK_SAMPLES)),
            socket,
            remote,
            session_id: 7,
            wire_version: 3,
            cipher: ChaCha20Poly1305::new_from_slice(&[23; 32]).unwrap(),
            call: CallCipher::new(test_call_keys("local")).unwrap(),
            encoder: Encoder::new(48_000, Channels::Mono, Application::LowDelay).unwrap(),
            capture: sample_queue([0.1; AUDIO_FRAME_SAMPLES].into_iter().collect()),
            playback: sample_queue(VecDeque::new()),
            stop: Arc::new(AtomicBool::new(true)),
            muted: Arc::new(AtomicBool::new(false)),
            deafened: Arc::new(AtomicBool::new(false)),
            counters: counters.clone(),
            errors: Arc::new(Mutex::new(None)),
            controls: Arc::new(test_audio_controls()),
            level: Arc::new(AtomicU32::new(0)),
        };
        thread::spawn(move || voice_worker(worker)).join().unwrap();
        let mut packet = [0; UDP_MAX_PACKET];
        assert!(server.recv_from(&mut packet).is_err());
        assert_eq!(counters.sent.load(Ordering::Relaxed), 0);
    }
    #[test]
    fn already_encoded_speech_obeys_current_mute_deafen_and_ptt_but_keeps_presence() {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        server
            .set_read_timeout(Some(Duration::from_millis(25)))
            .unwrap();
        let remote = server.local_addr().unwrap();
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.connect(remote).unwrap();
        socket.set_nonblocking(true).unwrap();
        let controls = Arc::new(test_audio_controls());
        let counters = Arc::new(Counters::default());
        let cipher = ChaCha20Poly1305::new_from_slice(&[29; 32]).unwrap();
        let mut encoder = Encoder::new(48_000, Channels::Mono, Application::LowDelay).unwrap();
        let mut opus = [0; 400];
        let size = encoder
            .encode_float(&[0.1; AUDIO_FRAME_SAMPLES], &mut opus)
            .unwrap();
        let mut audio = vec![1];
        audio.extend_from_slice(&7u64.to_le_bytes());
        audio.extend_from_slice(&opus[..size]);
        let worker = VoiceWorker {
            gateway: None,
            playback_limit: Arc::new(AtomicUsize::new(MAX_PLAYBACK_SAMPLES)),
            socket,
            remote,
            session_id: 7,
            wire_version: 3,
            cipher: cipher.clone(),
            call: CallCipher::new(test_call_keys("local")).unwrap(),
            encoder,
            capture: sample_queue(VecDeque::new()),
            playback: sample_queue(VecDeque::new()),
            stop: Arc::new(AtomicBool::new(false)),
            muted: Arc::new(AtomicBool::new(false)),
            deafened: Arc::new(AtomicBool::new(false)),
            counters: counters.clone(),
            errors: Arc::new(Mutex::new(None)),
            controls: controls.clone(),
            level: Arc::new(AtomicU32::new(0)),
        };
        let mut sequence = 1;
        let mut packet = [0; UDP_MAX_PACKET];
        for blocked in 0..3 {
            worker.muted.store(blocked == 0, Ordering::Release);
            worker.deafened.store(blocked == 1, Ordering::Release);
            controls.transmission.store(
                if blocked == 2 {
                    TransmissionMode::PushToTalk as u32
                } else {
                    TransmissionMode::Continuous as u32
                },
                Ordering::Release,
            );
            controls.ptt_pressed.store(false, Ordering::Release);
            send_packet(&worker, &mut sequence, &audio);
            assert!(
                server.recv_from(&mut packet).is_err(),
                "an already encoded frame stays private after permissions change"
            );
            send_packet(&worker, &mut sequence, &[0]);
            let (size, _) = server.recv_from(&mut packet).unwrap();
            assert_eq!(
                open_packet(&cipher, 7, 0, &packet[..size]),
                Some(((blocked + 1) as u64, vec![0]))
            );
        }
        controls.ptt_pressed.store(true, Ordering::Release);
        send_packet(&worker, &mut sequence, &audio);
        let (size, _) = server.recv_from(&mut packet).unwrap();
        assert_eq!(
            open_packet(&cipher, 7, 0, &packet[..size]),
            Some((4, audio))
        );
        assert_eq!(sequence, 5);
        assert_eq!(counters.sent.load(Ordering::Relaxed), 4);
    }
    #[test]
    #[ignore = "Generated PCM formats through actual capture callback; no audio devices"]
    fn capture_sample_rate_conversion_preserves_passband_and_rejects_aliases() {
        let mut failed = Vec::new();
        for rate in [44_100_u32, 48_000, 96_000] {
            for hz in [400_u32, 6000, 12_000, 16_000, 20_000]
                .into_iter()
                .chain((rate == 96_000).then_some(30_000))
            {
                let queue = sample_queue(VecDeque::new());
                let counters = Arc::new(Counters::default());
                let state = InputState {
                    queue: queue.clone(),
                    level: Arc::new(AtomicU32::new(0)),
                    muted: Arc::new(AtomicBool::new(false)),
                    deafened: Arc::new(AtomicBool::new(false)),
                    counters: counters.clone(),
                    errors: Arc::new(Mutex::new(None)),
                    controls: Arc::new(AudioControls::new(&AudioSettings::default())),
                    stop: Arc::new(AtomicBool::new(false)),
                };
                let config = cpal::StreamConfig {
                    channels: 1,
                    sample_rate: cpal::SampleRate(rate),
                    buffer_size: cpal::BufferSize::Default,
                };
                let mut capture = input_callback::<f32>(&config, state);
                let mut captured = Vec::with_capacity(96_000);
                // Drain the real bounded queue at each 10 ms device callback.
                // This measures format conversion, without manufacturing overflow.
                for block in 0..200 {
                    let pcm: Vec<_> = (0..rate / 100)
                        .map(|n| {
                            let source = block * (rate / 100) + n;
                            (f64::from(source) * f64::from(hz) * std::f64::consts::TAU
                                / f64::from(rate))
                            .sin() as f32
                                * 0.2
                        })
                        .collect();
                    capture(&pcm);
                    captured.extend(queue.lock().unwrap().drain(..));
                }
                assert_eq!(counters.capture_drop.load(Ordering::Relaxed), 0);
                assert!((captured.len() as i64 - 96_000).abs() < 4);
                assert!(captured.iter().all(|v| v.is_finite()));
                let steady = &captured[48_000..];
                let energy = steady.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>();
                let gain = (energy / steady.len() as f64).sqrt() / (0.2 / 2.0_f64.sqrt());
                let alias_hz = 18_000.0;
                let (mut sine, mut cosine) = (0.0_f64, 0.0_f64);
                for (n, value) in steady.iter().enumerate() {
                    let phase = n as f64 * alias_hz * std::f64::consts::TAU / 48_000.0;
                    sine += f64::from(*value) * phase.sin();
                    cosine += f64::from(*value) * phase.cos();
                }
                let alias_gain = 2.0 * sine.hypot(cosine) / steady.len() as f64 / 0.2;
                println!(
                    "Owned capture format {}",
                    serde_json::json!({
                        "source_rate":rate,"source_hz":hz,"output_samples":captured.len(),
                        "rms_gain":gain,"alias_18khz_gain":alias_gain
                    })
                );
                if hz <= 20_000 {
                    if !(0.98..=1.02).contains(&gain) {
                        failed.push(format!("{rate}Hz input, {hz}Hz tone: gain {gain}"));
                    }
                } else if gain >= 0.01 || alias_gain >= 0.01 {
                    failed.push(format!(
                        "{rate}Hz input, {hz}Hz out-of-band tone: gain {gain}, alias {alias_gain}"
                    ));
                }
            }
        }
        assert!(
            failed.is_empty(),
            "Capture conversion damaged PCM: {failed:?}"
        );
    }

    #[test]
    fn native_48khz_capture_preserves_each_pcm_sample_exactly() {
        let input: Vec<_> = (0..2048).map(|n| (n as f32 * 0.13).sin() * 0.8).collect();
        let mut converted = VecDeque::new();
        let mut resampler = InputResampler::new(48_000);
        for sample in &input {
            resampler.feed(*sample, &mut converted);
        }
        assert_eq!(converted.into_iter().collect::<Vec<_>>(), input);
    }

    #[test]
    fn capture_format_gates_clear_all_resampling_history_before_fresh_audio() {
        for rate in [44_100_u32, 48_000, 96_000] {
            for gate in 0..3 {
                let make_state = || InputState {
                    queue: sample_queue(VecDeque::new()),
                    level: Arc::new(AtomicU32::new(0)),
                    muted: Arc::new(AtomicBool::new(false)),
                    deafened: Arc::new(AtomicBool::new(false)),
                    counters: Arc::new(Counters::default()),
                    errors: Arc::new(Mutex::new(None)),
                    controls: Arc::new(AudioControls::new(&AudioSettings::default())),
                    stop: Arc::new(AtomicBool::new(false)),
                };
                let state = make_state();
                let config = cpal::StreamConfig {
                    channels: 1,
                    sample_rate: cpal::SampleRate(rate),
                    buffer_size: cpal::BufferSize::Default,
                };
                let mut capture = input_callback::<f32>(&config, state.clone());
                let frames = rate as usize / 100;
                capture(&vec![0.75; frames]);
                assert!(state.queue.lock().unwrap().iter().any(|v| *v > 0.5));
                let blocked = match gate {
                    0 => &state.muted,
                    1 => &state.deafened,
                    _ => &state.stop,
                };
                blocked.store(true, Ordering::Release);
                capture(&vec![-0.6; frames]);
                assert!(state.queue.lock().unwrap().is_empty());
                assert_eq!(state.level.load(Ordering::Relaxed), 0);
                // Reopening here checks internal memory clearing only. A stopped
                // production call still retires its callback and cannot restart it.
                blocked.store(false, Ordering::Release);
                let fresh = make_state();
                let mut fresh_capture = input_callback::<f32>(&config, fresh.clone());
                let pcm = vec![0.125; frames];
                capture(&pcm);
                fresh_capture(&pcm);
                assert_eq!(
                    state
                        .queue
                        .lock()
                        .unwrap()
                        .iter()
                        .copied()
                        .collect::<Vec<_>>(),
                    fresh
                        .queue
                        .lock()
                        .unwrap()
                        .iter()
                        .copied()
                        .collect::<Vec<_>>(),
                    "Private history survived gate {gate} at {rate}Hz"
                );
                assert!(state.queue.lock().unwrap().iter().all(|v| v.abs() < 0.2));
            }
        }
    }

    #[test]
    fn resampler_supports_common_44100hz_devices() {
        let mut resampler = InputResampler::new(44_100);
        let mut out = VecDeque::new();
        for n in 0..44_101 {
            resampler.feed(n as f32 / 44_100.0, &mut out);
        }
        assert!((out.len() as isize - 48_000).abs() <= 1);
        // A causal band-limited ramp has a tiny start transient and 32 native
        // samples of filter delay. Keep count/range checks and verify the
        // time-aligned linear response after the whole filter has settled.
        assert!(out
            .iter()
            .all(|x| x.is_finite() && *x >= -0.00001 && *x <= 1.0));
        let warmup = (crate::audio_resample::TAPS * 48_000).div_ceil(44_100);
        for (n, value) in out.iter().enumerate().skip(warmup) {
            let expected =
                n as f64 / 48_000.0 - (crate::audio_resample::TAPS / 2) as f64 / 44_100.0;
            assert!(*value >= 0.0 && (f64::from(*value) - expected).abs() < 0.00001);
        }
    }
    /// Runs against the actual release server while keeping every audio device closed.
    /// Set RELAY_MEDIA_TEST_SERVER to the built relay-server executable and invoke
    /// `cargo test -p relay-client actual_server_opus_room_and_authorization -- --ignored`.
    #[test]
    #[ignore = "requires RELAY_MEDIA_TEST_SERVER pointing to a built server executable"]
    fn actual_server_opus_room_and_authorization() {
        use serde_json::{json, Value};
        use std::{
            fs,
            io::{Read, Write},
            net::{TcpListener, TcpStream},
            path::PathBuf,
            process::{Child, Command, Stdio},
            time::SystemTime,
        };

        struct Server {
            child: Child,
            port: u16,
        }
        impl Drop for Server {
            fn drop(&mut self) {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
        impl Server {
            fn http(
                &self,
                method: &str,
                path: &str,
                token: Option<&str>,
                body: Value,
            ) -> (u16, Value) {
                let mut socket = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                socket
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let bytes = serde_json::to_vec(&body).unwrap();
                let auth = token
                    .map(|t| format!("Authorization: Bearer {t}\r\n"))
                    .unwrap_or_default();
                let header = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\n\r\n", bytes.len());
                socket.write_all(header.as_bytes()).unwrap();
                socket.write_all(&bytes).unwrap();
                let mut response = Vec::new();
                socket
                    .take(2 * 1024 * 1024)
                    .read_to_end(&mut response)
                    .unwrap();
                let split = response
                    .windows(4)
                    .position(|s| s == b"\r\n\r\n")
                    .expect("HTTP response header");
                let header = String::from_utf8_lossy(&response[..split]);
                let status = header
                    .lines()
                    .next()
                    .unwrap()
                    .split_whitespace()
                    .nth(1)
                    .unwrap()
                    .parse()
                    .unwrap();
                let body = &response[split + 4..];
                let value = if body.is_empty() {
                    Value::Null
                } else {
                    serde_json::from_slice(body).expect("HTTP JSON response")
                };
                (status, value)
            }
            fn post(&self, path: &str, token: Option<&str>, body: Value) -> Value {
                let (status, result) = self.http("POST", path, token, body);
                assert!(
                    (200..300).contains(&status),
                    "{path} returned {status}: {result}"
                );
                result
            }
        }
        struct Peer {
            socket: UdpSocket,
            ticket: VoiceTicket,
            cipher: ChaCha20Poly1305,
            sequence: u64,
        }
        impl Peer {
            fn new(server: &Server, token: &str, channel: &str) -> Self {
                let ticket: VoiceTicket = serde_json::from_value(server.post(
                    "/api/voice/join",
                    Some(token),
                    json!({"channel_id": channel}),
                ))
                .unwrap();
                let key = STANDARD.decode(&ticket.key).unwrap();
                let cipher = ChaCha20Poly1305::new_from_slice(&key).unwrap();
                let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
                socket.connect(("127.0.0.1", ticket.udp_port)).unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_millis(120)))
                    .unwrap();
                let mut peer = Self {
                    socket,
                    ticket,
                    cipher,
                    sequence: 1,
                };
                peer.send(&[0]);
                peer
            }
            fn send(&mut self, plain: &[u8]) -> Vec<u8> {
                let packet = seal_packet(
                    &self.cipher,
                    self.ticket.session_id,
                    self.sequence,
                    0,
                    plain,
                )
                .unwrap();
                self.sequence += 1;
                self.socket.send(&packet).unwrap();
                packet
            }
            fn heard(&self, speaker: u64, opus: &[u8]) {
                let mut bytes = [0; UDP_MAX_PACKET];
                let n = self
                    .socket
                    .recv(&mut bytes)
                    .expect("authorized room listener receives voice");
                let (_, plain) =
                    open_packet(&self.cipher, self.ticket.session_id, 1, &bytes[..n]).unwrap();
                assert_eq!(plain[0], 1);
                assert_eq!(
                    u64::from_le_bytes(plain[1..9].try_into().unwrap()),
                    speaker,
                    "relay must replace spoofed speaker identity"
                );
                assert_eq!(
                    &plain[9..],
                    opus,
                    "relay preserves Opus bytes without transcoding"
                );
                let mut decoded = [0.0; AUDIO_FRAME_SAMPLES];
                assert_eq!(
                    Decoder::new(48_000, Channels::Mono)
                        .unwrap()
                        .decode_float(&plain[9..], &mut decoded, false)
                        .unwrap(),
                    AUDIO_FRAME_SAMPLES
                );
                assert!(decoded.iter().any(|n| n.abs() > 0.01));
            }
            fn silent(&self) {
                let mut bytes = [0; UDP_MAX_PACKET];
                let error = self
                    .socket
                    .recv(&mut bytes)
                    .expect_err("unauthorized recipient must not receive voice");
                assert!(matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ));
            }
            fn protected(&self) -> Vec<u8> {
                let mut bytes = [0; UDP_MAX_PACKET];
                let n = self
                    .socket
                    .recv(&mut bytes)
                    .expect("protected frame forwarded");
                let (_, plain) =
                    open_packet(&self.cipher, self.ticket.session_id, 1, &bytes[..n]).unwrap();
                assert_eq!(plain[0], AUDIO_PACKET_V3);
                plain
            }
        }

        let executable = PathBuf::from(
            std::env::var_os("RELAY_MEDIA_TEST_SERVER")
                .expect("RELAY_MEDIA_TEST_SERVER is required"),
        );
        let stamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::var_os("RELAY_TEST_WORK_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("relay-tests"));
        let folder = base.join(format!("media-server-smoke-{}-{stamp}", std::process::id()));
        fs::create_dir_all(&folder).unwrap();
        let server_copy = folder.join(if cfg!(windows) {
            "relay-server.exe"
        } else {
            "relay-server"
        });
        fs::copy(executable, &server_copy).unwrap();
        let reservation = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = reservation.local_addr().unwrap().port();
        drop(reservation);
        let mut command = Command::new(server_copy);
        command
            .args([
                "--bind",
                "127.0.0.1",
                "--port",
                &port.to_string(),
                "--udp-port",
                "0",
                "--data",
            ])
            .arg(folder.join("data"))
            .arg("--name")
            .arg("Media test")
            .stdout(Stdio::from(
                fs::File::create(folder.join("server.log")).unwrap(),
            ))
            .stderr(Stdio::from(
                fs::File::create(folder.join("server-errors.log")).unwrap(),
            ));
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let server = Server {
            child: command.spawn().unwrap(),
            port,
        };
        let started = Instant::now();
        while TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "server startup timeout"
            );
            thread::sleep(Duration::from_millis(25));
        }
        let admin = server.post(
            "/api/register",
            None,
            json!({"username":"mediaalice", "password":"Media_Test_1234!", "invite":""}),
        );
        let a_token = admin["token"].as_str().unwrap();
        let invite = server.post("/api/invites", Some(a_token), json!({}));
        let bob = server.post(
            "/api/register",
            None,
            json!({"username":"mediabob", "password":"Media_Test_1234!", "invite":invite["code"]}),
        );
        let invite = server.post("/api/invites", Some(a_token), json!({}));
        let carol = server.post("/api/register", None, json!({"username":"mediacarol", "password":"Media_Test_1234!", "invite":invite["code"]}));
        let b_token = bob["token"].as_str().unwrap();
        let c_token = carol["token"].as_str().unwrap();
        let (status, bootstrap) = server.http("GET", "/api/bootstrap", Some(a_token), Value::Null);
        assert_eq!(status, 200);
        let room = bootstrap["channels"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["kind"] == "voice")
            .unwrap()["id"]
            .as_str()
            .unwrap();
        let other = server.post(
            "/api/channels",
            Some(a_token),
            json!({"name":"Other voice", "kind":"voice", "topic":""}),
        );
        let other_room = other["id"].as_str().unwrap();
        let (status, _) = server.http("POST", "/api/voice/join", None, json!({"channel_id":room}));
        assert_eq!(status, 401);
        let mut a = Peer::new(&server, a_token, room);
        let mut b = Peer::new(&server, b_token, room);
        let mut c = Peer::new(&server, c_token, other_room);
        thread::sleep(Duration::from_millis(50));
        let input: Vec<f32> = (0..AUDIO_FRAME_SAMPLES)
            .map(|n| (n as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.2)
            .collect();
        let mut encoder = Encoder::new(48_000, Channels::Mono, Application::LowDelay).unwrap();
        encoder.set_bitrate(Bitrate::Bits(64_000)).unwrap();
        let mut encoded = [0; 400];
        let len = encoder.encode_float(&input, &mut encoded).unwrap();
        let opus = &encoded[..len];
        let mut plain = vec![1];
        plain.extend_from_slice(&999u64.to_le_bytes());
        plain.extend_from_slice(opus);
        let packet = a.send(&plain);
        b.heard(a.ticket.session_id, opus);
        a.silent();
        c.silent();
        a.socket.send(&packet).unwrap();
        b.silent(); // exact replay
        let mut corrupted =
            seal_packet(&a.cipher, a.ticket.session_id, a.sequence, 0, &plain).unwrap();
        *corrupted.last_mut().unwrap() ^= 1;
        a.socket.send(&corrupted).unwrap();
        b.silent();
        a.send(&plain);
        b.heard(a.ticket.session_id, opus); // invalid authentication doesn't consume sequence
        let unknown = seal_packet(&a.cipher, 0, a.sequence, 0, &plain).unwrap();
        a.socket.send(&unknown).unwrap();
        b.silent();
        let wrong_cipher = ChaCha20Poly1305::new_from_slice(&[0; 32]).unwrap();
        a.socket
            .send(&seal_packet(&wrong_cipher, a.ticket.session_id, a.sequence, 0, &plain).unwrap())
            .unwrap();
        b.silent();
        a.send(&plain);
        b.heard(a.ticket.session_id, opus);
        a.socket
            .send(&seal_packet(&a.cipher, a.ticket.session_id, a.sequence, 1, &plain).unwrap())
            .unwrap();
        b.silent();
        a.send(&plain);
        b.heard(a.ticket.session_id, opus);
        let impostor = UdpSocket::bind("127.0.0.1:0").unwrap();
        impostor
            .send_to(
                &seal_packet(&a.cipher, a.ticket.session_id, a.sequence, 0, &plain).unwrap(),
                ("127.0.0.1", a.ticket.udp_port),
            )
            .unwrap();
        b.silent();
        a.send(&plain);
        b.heard(a.ticket.session_id, opus); // authenticated voice requires pinned socket
        let voice_state = |muted, deafened| json!({"muted":muted, "deafened":deafened, "sharing":false, "camera":false});
        server.post("/api/voice/state", Some(a_token), voice_state(true, false));
        a.send(&plain);
        b.silent();
        server.post("/api/voice/state", Some(a_token), voice_state(false, false));
        a.send(&plain);
        b.heard(a.ticket.session_id, opus);
        server.post("/api/voice/state", Some(b_token), voice_state(false, true));
        a.send(&plain);
        b.silent();
        server.post("/api/voice/state", Some(b_token), voice_state(false, false));
        a.send(&plain);
        b.heard(a.ticket.session_id, opus);
        let b_other = Peer::new(&server, b_token, other_room);
        thread::sleep(Duration::from_millis(50));
        a.send(&plain);
        b.silent();
        b_other.silent();
        c.send(&plain);
        b_other.heard(c.ticket.session_id, opus);
        a.silent();
        server.post("/api/voice/leave", Some(c_token), json!({}));
        c.send(&plain);
        b_other.silent();
        b = Peer::new(&server, b_token, room);
        thread::sleep(Duration::from_millis(50));
        a.send(&plain);
        b.heard(a.ticket.session_id, opus);
        assert!(a.ticket.wire_version >= 3);
        let call_keys = |local: &Value| {
            let local_id = local["user"]["id"].as_str().unwrap();
            let sender_keys = [(&admin, 101_u8), (&bob, 113_u8), (&carol, 127_u8)]
                .into_iter()
                .map(|(session, byte)| {
                    let id = session["user"]["id"].as_str().unwrap();
                    (
                        id.to_owned(),
                        crate::e2ee::SenderKeys {
                            device_id: format!("owned-device-{id}"),
                            opus: zeroize::Zeroizing::new([byte; 32]),
                            rtc: zeroize::Zeroizing::new([byte + 1; 32]),
                        },
                    )
                })
                .collect();
            Arc::new(CallKeys {
                group_id: "owned-server-voice-fixture".into(),
                scope: room.into(),
                epoch: 5,
                local_user_id: local_id.into(),
                local_device_id: format!("owned-device-{local_id}"),
                sender_keys,
                voice_replay: Mutex::default(),
            })
        };
        let sender = CallCipher::new(call_keys(&admin)).unwrap();
        let mut listener = CallCipher::new(call_keys(&bob)).unwrap();
        let protected = sender.seal(999, 1, 0, true, opus).unwrap();
        a.send(&protected);
        let forwarded = b.protected();
        assert_eq!(
            &forwarded[9..],
            &protected[9..],
            "host must preserve opaque authenticated audio"
        );
        assert_eq!(
            u64::from_le_bytes(forwarded[1..9].try_into().unwrap()),
            a.ticket.session_id
        );
        let frame = listener.open(&forwarded).unwrap();
        assert_eq!(frame.user_id, admin["user"]["id"].as_str().unwrap());
        assert_eq!(frame.opus, opus);
        c.silent();
        a.send(&protected); // New outer nonce does not make inner replay acceptable.
        assert!(listener.open(&b.protected()).is_none());
        let mut tampered_inner = sender.seal(999, 2, 960, false, opus).unwrap();
        tampered_inner[17] ^= 1;
        a.send(&tampered_inner);
        assert!(listener.open(&b.protected()).is_none());
        a.send(&sender.seal(999, 2, 960, false, opus).unwrap());
        assert!(
            listener.open(&b.protected()).is_some(),
            "bad inner MAC cannot consume replay state"
        );
        let mut forged_user = protected.clone();
        let bob_id = bob["user"]["id"].as_str().unwrap().as_bytes();
        assert_eq!(usize::from(forged_user[34]), bob_id.len());
        forged_user[60..60 + bob_id.len()].copy_from_slice(bob_id);
        a.send(&forged_user);
        b.silent(); // Outer authenticated account is not allowed to claim Bob.
        fs::write(folder.join("protected-voice-evidence.json"),serde_json::to_vec_pretty(&json!({"fixture":"actual-Relay-opaque-XChaCha-Opus","wire_version":3,"inner_user_authenticated":true,"routing_id_rewritten_only":true,"ciphertext_preserved":true,"fresh_outer_nonce_inner_replay_rejected":true,"inner_tamper_rejected_without_replay_poison":true,"outer_user_claim_enforced":true,"other_room_silent":true,"physical_devices_opened":false})).unwrap()).unwrap();
        server.post("/api/logout", Some(a_token), json!({}));
        a.send(&plain);
        b.silent();
        let (status, _) = server.http(
            "POST",
            "/api/voice/join",
            Some(a_token),
            json!({"channel_id":room}),
        );
        assert_eq!(status, 401);
        println!("actual-server UDP Opus roundtrip passed: 3 users, 2 rooms, identity rewrite, no self-echo, replay/tamper/unknown-session/wrong-key/wrong-direction/wrong-source rejection, mute/deafen, channel move, leave and logout revocation; every hardware device stayed closed");
    }
}

#[cfg(all(test, feature = "native-rtc"))]
#[path = "media_synthetic_voice.rs"]
mod synthetic_voice;
