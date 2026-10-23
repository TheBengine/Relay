//! Device-free native SDK probe using real Friends grants and production key policy.
use super::*;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use livekit::webrtc::{
    native::frame_cryptor::EncryptionState,
    peer_connection_factory::IceTransportsType,
    stats::{IceCandidatePairState, IceCandidateType, RtcStats},
};
use serde_json::{json, Value};
use std::collections::HashSet;

#[path = "rtc_people_quality.rs"]
mod quality;
#[cfg(windows)]
#[path = "rtc_people_loopback_tests.rs"]
mod loopback_tests;

const TURN: &str = "turns:relay-accounts.tail787aee.ts.net:10000";

#[test]
fn ice_failure_counts_do_not_disclose_candidate_or_transport_secrets() {
    let publisher_stats: Vec<RtcStats> = serde_json::from_value(json!([
        {"type":"local-candidate","candidateType":"relay","address":"private-address",
         "url":"turns:private-host?secret","usernameFragment":"private-username"},
        {"type":"remote-candidate","address":"192.168.10.20","port":7882,"protocol":"udp"},
        {"type":"candidate-pair","state":"succeeded","requestsSent":3,"responsesReceived":2,
         "bytesSent":12,"bytesReceived":24,"localCandidateId":"private-id"},
        {"type":"transport","iceLocalUsernameFragment":"private-transport"},
        {"type":"certificate","base64Certificate":"private-certificate"}
    ])).unwrap();
    let stats = livekit::rtc_engine::SessionStats { publisher_stats, subscriber_stats: vec![] };
    assert_eq!(stats.redacted_ice_summary(),
        "publisher: local=1, relay=1, remote=1, pairs=1, succeeded=1, requests=3, responses=2, bytes=36, remote_udp_ports={7882}, remote_loopback=0, remote_private=1; subscriber: local=0, relay=0, remote=0, pairs=0, succeeded=0, requests=0, responses=0, bytes=0, remote_udp_ports={}, remote_loopback=0, remote_private=0");
    let failure = livekit::rtc_engine::EngineError::IceTimeout(Box::new(livekit::rtc_engine::IceConnectionFailure {
        summary: "address-free counters".into(),
        remote_candidates: vec![livekit::rtc_engine::IceRemoteCandidate {
            address: "192.168.10.20".parse().unwrap(), port: 7882, protocol: "udp",
        }],
    }));
    assert_eq!(format!("{failure}"), "connection error: address-free counters");
    assert!(!format!("{failure:?}").contains("192.168.10.20"));
}

pub(crate) struct PeopleMediaGrant {
    pub ticket: RtcTicket,
    pub keys: Arc<CallKeys>,
}

struct VideoPublication {
    source: NativeVideoSource,
    _track: LocalVideoTrack,
    publication: LocalTrackPublication,
    width: u32,
    height: u32,
}
struct VideoReader {
    source: TrackSource,
    stream: NativeVideoStream,
    last: i64,
}
struct QualityTrack {
    source: TrackSource,
    participant: String,
    track: RemoteTrack,
}
struct Peer {
    room: Room,
    events: tokio::sync::mpsc::UnboundedReceiver<RoomEvent>,
    keys: Arc<CallKeys>,
    video: Vec<VideoPublication>,
    audio: Option<(NativeAudioSource, LocalAudioTrack, LocalTrackPublication)>,
    readers: HashMap<String, VideoReader>,
    audio_readers: HashMap<String, NativeAudioStream>,
    quality_tracks: HashMap<String, QualityTrack>,
    frames: [u64; 2],
    colored: [bool; 2],
    audio_samples: u64,
    audio_energy: f64,
    reconnecting: bool,
    reconnected: bool,
    disconnect_reason_code: Option<i32>,
    refreshed_issuer_matches_initial: Option<bool>,
    unexpected_video_dimensions: Option<Value>,
    stop_flush_frames: [u64; 2],
}

fn claims(token: &str) -> Result<Value, String> {
    let parts: Vec<_> = token.split('.').collect();
    if parts.len() != 3 || token.len() > 16384 {
        return Err("Unexpected owned RTC grant shape".into());
    }
    serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(parts[1])
            .map_err(|_| "Decode owned RTC grant")?,
    )
    .map_err(|_| "Read owned RTC grant binding".into())
}

fn owned_probe_error_classification(error: &str) -> &'static str {
    match error {
        "Generated Friends video stopped within a sustained window" => "video_stalled",
        "Generated Friends audio stopped within a sustained window" => "audio_stalled",
        "Owned generated audio source deadline" => "audio_source_deadline",
        "Owned generated audio source failed" | "Generated audio source missing" => "audio_source_failed",
        "Owned generated video publication deadline" | "Owned generated audio publication deadline" => "publication_deadline",
        "Owned generated video publication failed" | "Owned generated audio publication failed" => "publication_failed",
        "Friends video publication was not protected" | "Friends program audio was not protected"
        | "Native Friends frame authentication failed" | "Unverified Friends native track was subscribed" => "media_authentication_failed",
        "Owned Friends native room disconnected unexpectedly" => "room_disconnected",
        "Owned Friends media probe cancelled" => "cancelled",
        "Native refreshed grant did not preserve the real Friends admission binding" => "refreshed_grant_binding_failed",
        "Native refreshed grant changed its verified issuer" => "refreshed_issuer_changed",
        "Owned signal reconnect deadline" | "Owned signal reconnect failed" => "signal_reconnect_failed",
        "Friends media did not resume after the refreshed signal reconnect" | "Refreshed signal recovery was not accepted" => "media_recovery_failed",
        "Generated bidirectional Friends camera/screen/audio did not decode sufficiently" => "insufficient_decoded_media",
        "Owned decoded Friends dimensions changed" | "Owned Friends audio format changed" => "decoded_media_format_changed",
        "Owned public ICE statistics deadline" | "Owned public ICE statistics unavailable" => "selected_path_statistics_unavailable",
        "Selected native local ICE candidate missing" | "Native media did not use the exact public TLS/TURN relay route"
        | "Both native publisher and subscriber require actual selected public relay evidence" => "selected_path_not_accepted",
        "Owned video stop deadline" | "Owned video stop failed" | "Owned audio stop deadline" | "Owned audio stop failed"
        | "Stopped Friends publications retained native readers" => "publication_cleanup_failed",
        _ => "other_owned_probe_error",
    }
}

fn finish_owned_probe(result: Result<Value, String>, cleanup_complete: bool) -> Result<Value, String> {
    match result {
        Err(primary) => Err(primary),
        Ok(value) if cleanup_complete => Ok(value),
        Ok(_) => Err("Owned native room cleanup did not complete".into()),
    }
}

fn owned_probe_diagnostic(
    stage: &'static str,
    seconds: u64,
    elapsed: f64,
    directions: &[Value; 2],
    primary: &Result<Value, String>,
    cleanup: [&'static str; 2],
) -> Value {
    json!({"schema":1,"evidence_kind":"partial_diagnostic_not_acceptance",
        "stage":stage,"requested_seconds":seconds,"elapsed_since_publication_attempt_seconds":elapsed,
        "primary_status":if primary.is_ok(){"passed"}else{"failed"},
        "primary_error_classification":primary.as_ref().err().map(|error|owned_probe_error_classification(error)),
        "directions":directions,"room_cleanup":cleanup})
}

fn retain_owned_probe_diagnostic(after_cleanup: bool, diagnostic: &Value) {
    // The harness creates a protected, unique fixture directory for each run.
    // Retain a pre-close snapshot as well, so a native close hang cannot hide it.
    let Some(fixture) = std::env::var_os("RELAY_LIVE_FRIENDS_MEDIA") else { return };
    let fixture = std::path::PathBuf::from(fixture);
    let Some(root) = fixture.parent() else { return };
    let Ok(encoded) = serde_json::to_vec(diagnostic) else { return };
    if encoded.len() > 16_384 { return; }
    let name = if after_cleanup { "native-probe-cleanup.private.json" } else { "native-probe-before-cleanup.private.json" };
    if let Ok(mut output) = std::fs::OpenOptions::new().write(true).create_new(true).open(root.join(name)) {
        use std::io::Write;
        let _ = output.write_all(&encoded);
    }
}

#[test]
fn owned_probe_preserves_primary_failure_and_redacts_partial_diagnostic() {
    let first = "Generated Friends audio stopped within a sustained window".to_string();
    for cleaned in [false, true] {
        assert_eq!(finish_owned_probe(Err(first.clone()), cleaned), Err(first.clone()));
    }
    assert_eq!(finish_owned_probe(Ok(json!({"probe":"passed"})), false),
        Err("Owned native room cleanup did not complete".into()));
    assert_eq!(finish_owned_probe(Ok(json!({"probe":"passed"})), true), Ok(json!({"probe":"passed"})));
    let secret = "private-token=do-not-serialize; sdp=private; https://private.invalid";
    let diagnostic = owned_probe_diagnostic("sustained-media-check", 100, 12.0,
        &[json!({"audio_samples":4800}), json!({"camera_frames":10})],
        &Err(secret.into()), ["deadline", "complete"]);
    assert_eq!(diagnostic["primary_error_classification"], "other_owned_probe_error");
    assert_eq!(diagnostic["primary_status"], "failed");
    assert_eq!(diagnostic["room_cleanup"], json!(["deadline", "complete"]));
    assert!(!diagnostic.to_string().contains("private"));
    assert!(diagnostic.to_string().len() < 16_384);
}

impl Peer {
    async fn connect(grant: PeopleMediaGrant) -> Result<Self, String> {
        crate::tls_crypto::initialize();
        let mut options = protected_room_options(&grant.keys)?;
        options.join_retries = 0;
        options.rtc_config.ice_transport_type = IceTransportsType::Relay;
        // Use the authenticated SFU's ICE servers. Do not mint TURN passwords or
        // override TLS with a local proxy, certificate bypass or host candidate.
        let (room, events) = tokio::time::timeout(
            Duration::from_secs(20),
            Room::connect(&grant.ticket.url, &grant.ticket.token, options),
        )
        .await
        .map_err(|_| "Owned Friends native join deadline")?
        .map_err(|error| {
            // Keep SDK details only in this run's already protected fixture.
            // Public failures must never contain ticket URLs or bearer values.
            let category = match &error {
                livekit::RoomError::Engine(livekit::rtc_engine::EngineError::Signal(_)) => "signaling",
                livekit::RoomError::Engine(livekit::rtc_engine::EngineError::Connection(_))
                | livekit::RoomError::Engine(livekit::rtc_engine::EngineError::IceTimeout(_)) => "connectivity",
                livekit::RoomError::Engine(livekit::rtc_engine::EngineError::Rtc(_))
                | livekit::RoomError::Rtc(_) => "native RTC",
                _ => "other SDK error",
            };
            if let Some(fixture) = std::env::var_os("RELAY_LIVE_FRIENDS_MEDIA") {
                let fixture = std::path::PathBuf::from(fixture);
                if let Some(root) = fixture.parent() {
                    if let livekit::RoomError::Engine(livekit::rtc_engine::EngineError::IceTimeout(failure)) = &error {
                        let peers: Vec<_> = failure.remote_candidates.iter().map(|candidate|
                            json!({"address":candidate.address.to_string(),"port":candidate.port,"protocol":candidate.protocol})
                        ).collect();
                        let path = root.join("native-ice-peers.private.json");
                        if let Ok(mut output) = std::fs::OpenOptions::new().write(true).create_new(true).open(path) {
                            use std::io::Write;
                            let _ = output.write_all(json!(peers).to_string().as_bytes());
                        }
                    }
                    let path = root.join("native-join-error.private.txt");
                    if let Ok(mut output) = std::fs::OpenOptions::new().write(true).create_new(true).open(path) {
                        use std::io::Write;
                        let detail = error.to_string();
                        let _ = output.write_all(&detail.as_bytes()[..detail.len().min(16_384)]);
                    }
                }
            }
            format!("Owned Friends native join failed ({category})")
        })?;
        if room.local_participant().identity().to_string() != grant.keys.local_user_id {
            let _ = room.close().await;
            return Err("Native room identity differs from the verified Friends device".into());
        }
        Ok(Self::from_connected(room, events, grant.keys))
    }

    fn from_connected(
        room: Room,
        events: tokio::sync::mpsc::UnboundedReceiver<RoomEvent>,
        keys: Arc<CallKeys>,
    ) -> Self {
        Self {
            room,
            events,
            keys,
            video: Vec::new(),
            audio: None,
            readers: HashMap::new(),
            audio_readers: HashMap::new(),
            quality_tracks: HashMap::new(),
            frames: [0; 2],
            colored: [false; 2],
            audio_samples: 0,
            audio_energy: 0.0,
            reconnecting: false,
            reconnected: false,
            disconnect_reason_code: None,
            refreshed_issuer_matches_initial: None,
            unexpected_video_dimensions: None,
            stop_flush_frames: [0; 2],
        }
    }

    async fn publish_generated(&mut self) -> Result<(), String> {
        for (kind, width, height) in [
            (TrackSource::Camera, 640, 360),
            (TrackSource::Screenshare, 1280, 720),
        ] {
            let source = NativeVideoSource::new(VideoResolution { width, height }, true);
            let track = LocalVideoTrack::create_video_track(
                "owned-generated-friends-video",
                RtcVideoSource::Native(source.clone()),
            );
            let publication = tokio::time::timeout(
                Duration::from_secs(10),
                self.room.local_participant().publish_track(
                    LocalTrack::Video(track.clone()),
                    TrackPublishOptions {
                        source: kind,
                        video_codec: VideoCodec::VP8,
                        video_encoding: Some(VideoEncoding {
                            max_bitrate: 1_000_000,
                            max_framerate: 10.0,
                        }),
                        simulcast: false,
                        ..Default::default()
                    },
                ),
            )
            .await
            .map_err(|_| "Owned generated video publication deadline")?
            .map_err(|_| "Owned generated video publication failed")?;
            if publication.encryption_type() != EncryptionType::Gcm {
                return Err("Friends video publication was not protected".into());
            }
            self.video.push(VideoPublication {
                source,
                _track: track,
                publication,
                width,
                height,
            });
        }
        let source = NativeAudioSource::new(
            AudioSourceOptions {
                echo_cancellation: false,
                noise_suppression: false,
                auto_gain_control: false,
            },
            48_000,
            2,
            20,
        );
        let track = LocalAudioTrack::create_audio_track(
            "owned-generated-friends-program-audio",
            RtcAudioSource::Native(source.clone()),
        );
        let publication = tokio::time::timeout(
            Duration::from_secs(10),
            self.room.local_participant().publish_track(
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
        .map_err(|_| "Owned generated audio publication failed")?;
        if publication.encryption_type() != EncryptionType::Gcm {
            return Err("Friends program audio was not protected".into());
        }
        self.audio = Some((source, track, publication));
        Ok(())
    }

    fn poll(&mut self) -> Result<(), String> {
        self.poll_frames(false)
    }

    fn poll_after_unpublish(&mut self) -> Result<(), String> {
        if !self.video.is_empty() || self.audio.is_some() {
            return Err("Owned close-frame drain started before unpublish".into());
        }
        self.poll_frames(true)
    }

    fn poll_frames(&mut self, publications_stopped: bool) -> Result<(), String> {
        while let Ok(event) = self.events.try_recv() {
            match event {
                RoomEvent::TrackPublished {
                    publication,
                    participant,
                } => {
                    publication.set_subscribed(verified_publication(
                        &self.keys,
                        &participant.identity().to_string(),
                        &publication,
                    ));
                }
                RoomEvent::TrackSubscribed {
                    track,
                    publication,
                    participant,
                } => {
                    if !verified_publication(
                        &self.keys,
                        &participant.identity().to_string(),
                        &publication,
                    ) {
                        publication.set_subscribed(false);
                        return Err("Unverified Friends native track was subscribed".into());
                    }
                    let sid = publication.sid().to_string();
                    self.quality_tracks.insert(sid.clone(),QualityTrack {
                        source:publication.source(),participant:participant.identity().to_string(),track:track.clone(),
                    });
                    match track {
                        RemoteTrack::Video(track) => {
                            if self.readers.len() >= 2 && !self.readers.contains_key(&sid) {
                                return Err("Too many owned video readers".into());
                            }
                            self.readers.insert(
                                sid,
                                VideoReader {
                                    source: publication.source(),
                                    stream: NativeVideoStream::new(track.rtc_track()),
                                    last: -1,
                                },
                            );
                        }
                        RemoteTrack::Audio(track) => {
                            if !self.audio_readers.is_empty()
                                && !self.audio_readers.contains_key(&sid)
                            {
                                return Err("Too many owned program-audio readers".into());
                            }
                            self.audio_readers.insert(
                                sid,
                                NativeAudioStream::with_options(
                                    track.rtc_track(),
                                    48_000,
                                    2,
                                    NativeAudioStreamOptions {
                                        queue_size_frames: Some(2),
                                    },
                                ),
                            );
                        }
                    }
                }
                RoomEvent::TrackUnpublished { publication, .. }
                | RoomEvent::TrackUnsubscribed { publication, .. } => {
                    self.readers.remove(&publication.sid().to_string());
                    self.audio_readers.remove(&publication.sid().to_string());
                    self.quality_tracks.remove(&publication.sid().to_string());
                }
                RoomEvent::Reconnecting => self.reconnecting = true,
                RoomEvent::Reconnected => self.reconnected = true,
                RoomEvent::E2eeStateChanged {
                    state:
                        EncryptionState::EncryptionFailed
                        | EncryptionState::DecryptionFailed
                        | EncryptionState::MissingKey
                        | EncryptionState::InternalError,
                    ..
                } => return Err("Native Friends frame authentication failed".into()),
                RoomEvent::Disconnected { reason } => {
                    self.disconnect_reason_code = Some(reason as i32);
                    return Err("Owned Friends native room disconnected unexpectedly".into())
                }
                _ => {}
            }
        }
        for reader in self.readers.values_mut() {
            for _ in 0..3 {
                let Some(Some(frame)) = reader.stream.next().now_or_never() else {
                    break;
                };
                let index = if reader.source == TrackSource::Camera {
                    0
                } else {
                    1
                };
                let (width, height) = if index == 0 { (640, 360) } else { (1280, 720) };
                let observed = (frame.buffer.width(), frame.buffer.height());
                // The pinned SFU's CloseWithFlush/getVP8BlankFrame sends 8x8
                // frames to clear the old decoder image on track removal.
                // Only the explicit post-unpublish drain may accept them;
                // never count these control frames as generated media.
                if publications_stopped && observed == (8, 8) {
                    self.stop_flush_frames[index] += 1;
                    continue;
                }
                if observed != (width, height) {
                    self.unexpected_video_dimensions = Some(json!({
                        "source": if index == 0 { "camera" } else { "screen" },
                        "expected": [width, height], "observed": [observed.0, observed.1]
                    }));
                    return Err("Owned decoded Friends dimensions changed".into());
                }
                if frame.timestamp_us != reader.last {
                    self.frames[index] += 1;
                    reader.last = frame.timestamp_us;
                    let mut pixels = vec![0; (width * height * 4) as usize];
                    frame.buffer.to_argb(
                        VideoFormatType::ABGR,
                        &mut pixels,
                        width * 4,
                        width as i32,
                        height as i32,
                    );
                    self.colored[index] |= pixels
                        .chunks_exact(4)
                        .take(20)
                        .any(|p| p[0].abs_diff(p[2]) > 40);
                }
            }
        }
        for reader in self.audio_readers.values_mut() {
            for _ in 0..6 {
                let Some(Some(frame)) = reader.next().now_or_never() else {
                    break;
                };
                if frame.sample_rate != 48_000 || frame.num_channels != 2 {
                    return Err("Owned Friends audio format changed".into());
                }
                self.audio_samples += frame.data.len() as u64;
                self.audio_energy += frame
                    .data
                    .iter()
                    .map(|s| f64::from(*s).powi(2))
                    .sum::<f64>();
            }
        }
        Ok(())
    }

    async fn feed(&self, index: u64, video: bool) -> Result<(), String> {
        if video {
            for published in &self.video {
                let mut buffer = I420Buffer::new(published.width, published.height);
                let (y, u, v) = buffer.data_mut();
                y.fill(65 + (index % 100) as u8);
                u.fill(85);
                v.fill(230);
                published.source.capture_frame(&VideoFrame {
                    rotation: VideoRotation::VideoRotation0,
                    timestamp_us: (index * 10_000 + 1_000_000) as i64,
                    frame_metadata: None,
                    buffer,
                });
            }
        }
        let pcm = (0..480)
            .flat_map(|sample| {
                let phase =
                    (index * 480 + sample) as f32 * std::f32::consts::TAU * 1000.0 / 48_000.0;
                [
                    (phase.sin() * 8000.0) as i16,
                    ((phase * 2.0).sin() * 8000.0) as i16,
                ]
            })
            .collect::<Vec<_>>();
        tokio::time::timeout(
            Duration::from_secs(1),
            self.audio
                .as_ref()
                .ok_or("Generated audio source missing")?
                .0
                .capture_frame(&AudioFrame {
                    data: pcm.into(),
                    sample_rate: 48_000,
                    num_channels: 2,
                    samples_per_channel: 480,
                }),
        )
        .await
        .map_err(|_| "Owned generated audio source deadline")?
        .map_err(|_| "Owned generated audio source failed".into())
    }

    async fn unpublish(&mut self) -> Result<(), String> {
        for published in &self.video {
            tokio::time::timeout(
                Duration::from_secs(5),
                self.room
                    .local_participant()
                    .unpublish_track(&published.publication.sid()),
            )
            .await
            .map_err(|_| "Owned video stop deadline")?
            .map_err(|_| "Owned video stop failed")?;
        }
        if let Some((_, _, publication)) = &self.audio {
            tokio::time::timeout(
                Duration::from_secs(5),
                self.room
                    .local_participant()
                    .unpublish_track(&publication.sid()),
            )
            .await
            .map_err(|_| "Owned audio stop deadline")?
            .map_err(|_| "Owned audio stop failed")?;
        }
        self.video.clear();
        self.audio = None;
        Ok(())
    }

    fn counts(&self) -> Value {
        json!({"camera_frames":self.frames[0],"screen_frames":self.frames[1],
        "camera_chroma_survived":self.colored[0],"screen_chroma_survived":self.colored[1],
        "audio_samples":self.audio_samples,"audio_rms":(self.audio_energy/self.audio_samples.max(1) as f64).sqrt()})
    }

    fn diagnostic_counts(&self) -> Value {
        json!({"camera_frames":self.frames[0],"screen_frames":self.frames[1],
            "camera_chroma_survived":self.colored[0],"screen_chroma_survived":self.colored[1],
            "audio_samples":self.audio_samples,"audio_energy_sum_squares":self.audio_energy,
            "audio_rms":(self.audio_energy/self.audio_samples.max(1) as f64).sqrt(),
            "video_publications":self.video.len(),"audio_publication":self.audio.is_some(),
            "video_readers":self.readers.len(),"audio_readers":self.audio_readers.len(),
            "reconnecting_observed":self.reconnecting,"reconnected_observed":self.reconnected,
            "disconnect_reason_code":self.disconnect_reason_code,
            "refreshed_issuer_matches_initial":self.refreshed_issuer_matches_initial,
            "unexpected_video_dimensions":self.unexpected_video_dimensions,
            "stop_flush_frames_excluded_from_media_counts":self.stop_flush_frames})
    }
}

fn selected_media_transport_scopes(
    single_peer_connection_active: bool,
    stats: &livekit::rtc_engine::SessionStats,
) -> Result<Vec<(&'static str, &[RtcStats])>, String> {
    if stats.publisher_stats.is_empty() {
        return Err("Native publisher/shared transport statistics missing".into());
    }
    if single_peer_connection_active {
        if !stats.subscriber_stats.is_empty() {
            return Err("Native negotiated shared transport statistics inconsistent".into());
        }
        Ok(vec![("shared_media", stats.publisher_stats.as_slice())])
    } else {
        if stats.subscriber_stats.is_empty() {
            return Err("Native negotiated subscriber transport statistics missing".into());
        }
        Ok(vec![
            ("publisher", stats.publisher_stats.as_slice()),
            ("subscriber", stats.subscriber_stats.as_slice()),
        ])
    }
}

#[test]
fn selected_media_scopes_follow_actual_negotiation_and_fail_closed_on_missing_stats() {
    let row: RtcStats = serde_json::from_value(json!({"type":"transport"})).unwrap();
    let mut stats = livekit::rtc_engine::SessionStats {
        publisher_stats: vec![row.clone()], subscriber_stats: vec![],
    };
    assert_eq!(selected_media_transport_scopes(true, &stats).unwrap()[0].0, "shared_media");
    assert!(selected_media_transport_scopes(false, &stats).is_err());
    stats.subscriber_stats.push(row);
    assert!(selected_media_transport_scopes(true, &stats).is_err());
    assert_eq!(selected_media_transport_scopes(false, &stats).unwrap().len(), 2);
    stats.publisher_stats.clear();
    assert!(selected_media_transport_scopes(false, &stats).is_err());
    stats.subscriber_stats.clear();
    assert!(selected_media_transport_scopes(true, &stats).is_err());
}

async fn public_relay_paths(room: &Room) -> Result<Value, String> {
    let reports = tokio::time::timeout(Duration::from_secs(3), room.get_stats())
        .await
        .map_err(|_| "Owned public ICE statistics deadline")?
        .map_err(|_| "Owned public ICE statistics unavailable")?;
    let mut output = serde_json::Map::new();
    for (name, stats) in selected_media_transport_scopes(room.is_single_peer_connection_active(), &reports)? {
        let selected: HashSet<_> = stats
            .iter()
            .filter_map(|s| match s {
                RtcStats::Transport(t) if !t.transport.selected_candidate_pair_id.is_empty() => {
                    Some(t.transport.selected_candidate_pair_id.as_str())
                }
                _ => None,
            })
            .collect();
        for stat in stats {
            let RtcStats::CandidatePair(pair) = stat else {
                continue;
            };
            if !selected.contains(pair.rtc.id.as_str())
                || !pair.candidate_pair.nominated
                || pair.candidate_pair.state != Some(IceCandidatePairState::Succeeded)
            {
                continue;
            }
            let candidate = stats
                .iter()
                .find_map(|s| match s {
                    RtcStats::LocalCandidate(c)
                        if c.rtc.id == pair.candidate_pair.local_candidate_id =>
                    {
                        Some(&c.local_candidate)
                    }
                    _ => None,
                })
                .ok_or("Selected native local ICE candidate missing")?;
            let url_matches =
                candidate.url == TURN || candidate.url == format!("{TURN}?transport=tcp");
            let protocol = format!("{:?}", candidate.relay_protocol);
            if candidate.candidate_type != Some(IceCandidateType::Relay)
                || !url_matches
                || !matches!(protocol.as_str(), "Some(Tls)" | "Some(Tcp)")
                || pair.candidate_pair.bytes_sent + pair.candidate_pair.bytes_received == 0
            {
                return Err(
                    "Native media did not use the exact public TLS/TURN relay route".into(),
                );
            }
            output.insert(name.into(),json!({"selected_nominated_relay":true,"turn_url":TURN,"relay_protocol":protocol,
                "bytes_sent":pair.candidate_pair.bytes_sent,"bytes_received":pair.candidate_pair.bytes_received}));
        }
        if !output.contains_key(name) {
            return Err("Every negotiated native media transport requires actual selected public relay evidence".into());
        }
    }
    Ok(Value::Object(output))
}

pub(crate) async fn probe_live_people_media(
    grants: [PeopleMediaGrant; 2],
    seconds: u64,
    stop: Arc<AtomicBool>,
) -> Result<Value, String> {
    let initial = [
        claims(&grants[0].ticket.token)?,
        claims(&grants[1].ticket.token)?,
    ];
    let [a, b] = grants;
    let mut a = Peer::connect(a).await?;
    let mut b = match Peer::connect(b).await {
        Ok(b) => b,
        Err(e) => {
            let _ = tokio::time::timeout(Duration::from_secs(5), a.room.close()).await;
            return Err(e);
        }
    };
    let diagnostic_started = Instant::now();
    let mut quality = quality::QualityProbe::new(diagnostic_started);
    let mut stage = "publish-peer-a";
    let result: Result<Value, String>=async{
        a.publish_generated().await?;stage="publish-peer-b";b.publish_generated().await?;
        let started=Instant::now();let mut tick=controller_pump_interval(Duration::from_millis(10));
        let mut index=0_u64;let mut reconnect_at=None;let mut resumed=[false;2];
        let mut before=[[0_u64;2];2];let mut before_energy=[0_f64;2];
        let mut last_window=Instant::now();let mut window_counts=[[0_u64;2];2];
        let mut window_audio=[(0_u64,0_f64);2];
        while started.elapsed()<Duration::from_secs(seconds){
            if stop.load(Ordering::Acquire){return Err("Owned Friends media probe cancelled".into());}
            stage="poll-peer-a";a.poll()?;stage="poll-peer-b";b.poll()?;
            quality.tick(&a,&b);
            stage="feed-peer-a";a.feed(index,index.is_multiple_of(10)).await?;stage="feed-peer-b";b.feed(index,index.is_multiple_of(10)).await?;index+=1;
            if last_window.elapsed()>=Duration::from_secs(12){
                stage="sustained-media-check";
                for (index,peer) in [&a,&b].into_iter().enumerate() {
                    let previous=&mut window_counts[index];
                    if peer.frames.iter().zip(previous.iter()).any(|(now,old)|now<=old){return Err("Generated Friends video stopped within a sustained window".into());}
                    *previous=peer.frames;
                    let (samples,energy)=window_audio[index];
                    if peer.audio_samples<=samples || (peer.audio_energy-energy)/((peer.audio_samples-samples) as f64)<40_000.0{
                        return Err("Generated Friends audio stopped within a sustained window".into());
                    }
                    window_audio[index]=(peer.audio_samples,peer.audio_energy);
                }
                last_window=Instant::now();
            }
            if seconds>=90 && reconnect_at.is_none() && started.elapsed()>=Duration::from_secs(70){
                stage="refreshed-grant-binding";
                let now=std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|_|"Fixture clock")?.as_secs();
                for (peer,old) in [(&mut a,&initial[0]),(&mut b,&initial[1])] {
                    let current=claims(&peer.room.token())?;
                    let issuer_matches = old["iss"].as_str().is_some_and(|issuer| !issuer.is_empty()
                        && current["iss"].as_str() == Some(issuer));
                    peer.refreshed_issuer_matches_initial = Some(issuer_matches);
                    if !issuer_matches {
                        return Err("Native refreshed grant changed its verified issuer".into());
                    }
                    if old["exp"].as_u64().is_none_or(|exp|exp>now) || current["exp"].as_u64().is_none_or(|exp|exp<=now)
                        || current["sub"]!=old["sub"] || current["attributes"]!=old["attributes"] || current["video"]["room"]!=old["video"]["room"]{
                        return Err("Native refreshed grant did not preserve the real Friends admission binding".into());
                    }
                }
                before=[a.frames,b.frames];before_energy=[a.audio_energy,b.audio_energy];reconnect_at=Some(Instant::now());
                quality.recovery_started();
                stage="signal-reconnect";
                for peer in [&a,&b]{tokio::time::timeout(Duration::from_secs(3),peer.room.simulate_scenario(livekit::SimulateScenario::SignalReconnect)).await
                    .map_err(|_|"Owned signal reconnect deadline")?.map_err(|_|"Owned signal reconnect failed")?;}
            }
            if let Some(at)=reconnect_at{
                stage="media-recovery-check";
                resumed[0]=a.reconnecting&&a.reconnected&&a.frames.iter().zip(before[0]).all(|(now,old)|*now>old+3)&&a.audio_energy>before_energy[0]+1_000_000.0;
                resumed[1]=b.reconnecting&&b.reconnected&&b.frames.iter().zip(before[1]).all(|(now,old)|*now>old+3)&&b.audio_energy>before_energy[1]+1_000_000.0;
                if at.elapsed()>Duration::from_secs(15)&&resumed.contains(&false){return Err("Friends media did not resume after the refreshed signal reconnect".into());}
            }
            tick.tick().await;
        }
        let feeding_seconds=started.elapsed().as_secs_f64();
        stage="minimum-decoded-media-check";
        for peer in [&a,&b]{if peer.frames.iter().any(|n|*n<10)||peer.colored.contains(&false)||peer.audio_samples<4800||peer.audio_energy/(peer.audio_samples.max(1) as f64)<40_000.0{
            return Err("Generated bidirectional Friends camera/screen/audio did not decode sufficiently".into());}}
        if seconds>=90&&resumed.contains(&false){return Err("Refreshed signal recovery was not accepted".into());}
        stage="selected-relay-peer-a";
        let path_a=public_relay_paths(&a.room).await?;
        stage="selected-relay-peer-b";
        let paths=[path_a,public_relay_paths(&b.room).await?];
        let counts=[a.counts(),b.counts()];
        stage="unpublish-peer-a";a.unpublish().await?;stage="unpublish-peer-b";b.unpublish().await?;
        let until=Instant::now()+Duration::from_secs(4);
        stage="drain-remote-readers";
        while !a.readers.is_empty()||!b.readers.is_empty()||!a.audio_readers.is_empty()||!b.audio_readers.is_empty(){
            a.poll_after_unpublish()?;b.poll_after_unpublish()?;if Instant::now()>until{return Err("Stopped Friends publications retained native readers".into());}
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        stage="probe-complete";
        Ok(json!({"scope":"Actual public native SDK with production Friends key/grant exchange; generated sources only, one Windows PC",
            "requested_seconds":seconds,"measured_feeding_seconds":feeding_seconds,"directions":counts,"selected_paths":paths,"stop_cleared_native_readers":true,
            "stop_flush_frames_excluded_from_media_counts":[a.stop_flush_frames,b.stop_flush_frames],
            "expired_initial_grant_and_refreshed_signal_reconnect":seconds>=90,
            "media_sustained_over_300_seconds":feeding_seconds>300.0,
            "per_source_quality_observations":quality.summary(),
            "media_continuity_is_not_quality_acceptance":true,
            "limitations":"No physical camera/microphone/playback/screen, GUI call acceptance, two-PC/WAN latency or subjective quality claim. Full native source-owner recovery and denied-peer TURN probes remain separate."}))
    }.await;
    quality.retain_owned_diagnostic(result.is_ok());
    let partial = [a.diagnostic_counts(), b.diagnostic_counts()];
    retain_owned_probe_diagnostic(false, &owned_probe_diagnostic(stage, seconds,
        diagnostic_started.elapsed().as_secs_f64(), &partial, &result, ["pending", "pending"]));
    let ac = tokio::time::timeout(Duration::from_secs(5), a.room.close()).await;
    let bc = tokio::time::timeout(Duration::from_secs(5), b.room.close()).await;
    let cleanup = [&ac, &bc].map(|closed| match closed {
        Ok(Ok(())) => "complete", Ok(Err(_)) => "close-error", Err(_) => "deadline",
    });
    retain_owned_probe_diagnostic(true, &owned_probe_diagnostic(stage, seconds,
        diagnostic_started.elapsed().as_secs_f64(), &partial, &result, cleanup));
    finish_owned_probe(result, cleanup.iter().all(|status| *status == "complete"))
}
