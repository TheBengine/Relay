//! Actual local SFU regression with generated pixels only; no device enumeration.
use super::*;
use livekit::webrtc::stats::RtcStats;
use serde_json::{json, Value};
use std::collections::HashSet;
pub(super) static SFU_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[test]
fn retirement_only_accepts_an_already_removed_closed_sender_during_reconnect() {
    let closed = || {
        livekit::RoomError::Engine(livekit::rtc_engine::EngineError::Rtc(
            livekit::webrtc::RtcError {
                error_type: livekit::webrtc::RtcErrorType::InvalidState,
                message: "PeerConnection is closed.".into(),
            },
        ))
    };
    assert!(retired_closed_sender_during_reconnect(
        &closed(),
        true,
        false
    ));
    for (reconnecting, registered) in [(false, false), (false, true), (true, true)] {
        assert!(!retired_closed_sender_during_reconnect(
            &closed(),
            reconnecting,
            registered
        ));
    }
    for error in [
        livekit::RoomError::AlreadyClosed,
        livekit::RoomError::Internal("track not found".into()),
        livekit::RoomError::Engine(livekit::rtc_engine::EngineError::Internal(
            "unexpected RTC failure".into(),
        )),
        livekit::RoomError::Engine(livekit::rtc_engine::EngineError::Rtc(
            livekit::webrtc::RtcError {
                error_type: livekit::webrtc::RtcErrorType::InvalidState,
                message: "Another native failure".into(),
            },
        )),
        livekit::RoomError::Engine(livekit::rtc_engine::EngineError::Rtc(
            livekit::webrtc::RtcError {
                error_type: livekit::webrtc::RtcErrorType::Internal,
                message: "PeerConnection is closed.".into(),
            },
        )),
    ] {
        assert!(!retired_closed_sender_during_reconnect(&error, true, false));
    }
}

fn recovery_fixture_keys() -> Arc<CallKeys> {
    fixture_call_keys(
        &json!({"room_id":"owned-recovery", "other_id":"unused", "users":{
            "alice":{"session":{"user":{"id":"owned-recovery-alice"}}},
            "bob":{"session":{"user":{"id":"owned-recovery-bob"}}},
            "eve":{"session":{"user":{"id":"owned-recovery-eve"}}}
        }}),
        "alice",
    )
}

pub(super) fn recovery_fixture_state(call_keys: Arc<CallKeys>, generation: u64) -> WorkerState {
    WorkerState {
        stop: Arc::new(AtomicBool::new(false)),
        ready: Arc::new(AtomicBool::new(true)),
        reconnecting: Arc::new(AtomicBool::new(false)),
        video_visible: Arc::new(AtomicBool::new(false)),
        errors: Arc::new(Mutex::new(None)),
        frames: Arc::new(Mutex::new(LatestFrames::default())),
        video_conversion: Arc::new(Mutex::new(())),
        decoded_frames: Arc::new(AtomicU64::new(0)),
        epochs: [
            Arc::new(AtomicU64::new(generation)),
            Arc::new(AtomicU64::new(1)),
        ],
        source_states: [
            Arc::new(AtomicU64::new((generation << 2) | 3)),
            Arc::new(AtomicU64::new(0)),
        ],
        tracks: Arc::new(Mutex::new(HashMap::new())),
        audio_tracks: Arc::new(Mutex::new(HashMap::new())),
        audio_sink: Arc::new(Mutex::new(None)),
        application_audio_active: Arc::new(AtomicBool::new(false)),
        diagnostics: Arc::new(Mutex::new(HashMap::new())),
        wake: Arc::new(tokio::sync::Notify::new()),
        ctx: Context::default(),
        connection_id: 0,
        call_keys,
    }
}

pub(super) fn recovery_fixture_publication(
    source: NativeVideoSource,
    sid: livekit::id::TrackSid,
    native_id: String,
    options: CaptureOptions,
    generation: u64,
) -> Publication {
    let (sender, receiver) = mpsc::sync_channel(1);
    Publication {
        capture: CaptureSession::synthetic_raw(sender, generation, options).unwrap(),
        receiver,
        source,
        sid,
        native_id,
        generation,
        options,
        // Recovery retires an encoded track after releasing its failed transform.
        // The fixture needs no COM transform or actual hardware encoder.
        pipeline: VideoPipeline::Software,
        adaptation: AdaptiveVideo::new(options),
        stats_task: None,
        next_stats: Instant::now(),
        previous_sample: None,
        previous_sample_at: None,
        outbound_mbps: None,
        next_frame: None,
        rate_bitrate: None,
        adaptive_samples: 0,
        recovery_pause: None,
        #[cfg(target_os = "windows")]
        audio: None,
    }
}

#[test]
fn automatic_hardware_format_failure_requests_a_typed_software_transition() {
    let error = "Owned unsupported 1440p/24 fps format".to_owned();
    assert_eq!(
        hardware_reconfiguration::<()>(
            VideoCodecPreference::Auto,
            Err(error.clone()),
            (2560, 1440)
        ),
        Err(VideoPumpError::SoftwareFallback {
            error: error.clone(),
            dimensions: (2560, 1440)
        })
    );
    for preference in [
        VideoCodecPreference::Hardware,
        VideoCodecPreference::Software,
    ] {
        assert_eq!(
            hardware_reconfiguration::<()>(preference, Err(error.clone()), (2560, 1440)),
            Err(VideoPumpError::Fatal(error.clone()))
        );
    }
    assert_eq!(
        hardware_reconfiguration(VideoCodecPreference::Auto, Ok(71), (2560, 1440)),
        Ok(71)
    );
}

#[test]
fn software_recovery_requires_current_generation_account_and_verified_keys() {
    let mut state = recovery_fixture_state(recovery_fixture_keys(), 7);
    let identity = state.call_keys.local_user_id.clone();
    assert_eq!(
        recovery_context_current(&state, false, 7, &identity),
        Ok(true)
    );
    assert!(recovery_context_current(&state, false, 7, "another-account").is_err());
    state.epochs[0].store(8, Ordering::Release);
    assert_eq!(
        recovery_context_current(&state, false, 7, &identity),
        Ok(false)
    );
    assert_eq!(
        recovery_context_current(&state, false, 8, &identity),
        Ok(true)
    );
    state.stop.store(true, Ordering::Release);
    assert_eq!(
        recovery_context_current(&state, false, 8, &identity),
        Ok(false)
    );
    state.stop.store(false, Ordering::Release);
    Arc::get_mut(&mut state.call_keys).unwrap().local_device_id = "unverified-device".into();
    assert!(recovery_context_current(&state, false, 8, &identity).is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_sid_cleanup_keeps_the_replacement_receiver_and_its_cached_pixels() {
    struct Dropped(Arc<AtomicBool>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    let dropped = Arc::new(AtomicBool::new(false));
    let task_dropped = dropped.clone();
    let (ready, waiting) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let _guard = Dropped(task_dropped);
        let _ = ready.send(());
        std::future::pending::<()>().await;
    });
    tokio::time::timeout(Duration::from_secs(2), waiting)
        .await
        .unwrap()
        .unwrap();
    let frames = Arc::new(Mutex::new(LatestFrames::default()));
    let key = "owned-recovery-alice:screen";
    frames.lock().unwrap().insert(
        key.into(),
        RtcFrame {
            user_id: "owned-recovery-alice".into(),
            kind: "screen".into(),
            image: Arc::new(ColorImage::new([16, 16], eframe::egui::Color32::RED)),
            timestamp_us: 123,
        },
    );
    let replacement_sid: livekit::id::TrackSid =
        "TR_owned_replacement".to_owned().try_into().unwrap();
    let old_sid = "TR_owned_retired".to_owned().try_into().unwrap();
    let mut receivers = HashMap::from([(
        key.into(),
        RemoteReceiver {
            sid: replacement_sid.clone(),
            task,
        },
    )]);
    assert!(!remove_receiver(&mut receivers, &frames, key, &old_sid).await);
    assert!(!dropped.load(Ordering::Acquire));
    assert!(receivers.contains_key(key));
    assert!(frames.lock().unwrap().frames.contains_key(key));
    assert!(remove_receiver(&mut receivers, &frames, key, &replacement_sid).await);
    assert!(dropped.load(Ordering::Acquire));
    assert!(receivers.is_empty() && frames.lock().unwrap().frames.is_empty());
}

#[cfg(target_os = "windows")]
async fn recovery_encoded_fixture(
    room: &Room,
    state: &WorkerState,
    options: CaptureOptions,
    generation: u64,
) -> Publication {
    // PreEncoded creates no GPU/Media Foundation encoder. No H.264 payload is
    // submitted; the old track models the failed encoder's empty handoff phase.
    let source = NativeVideoSource::new_encoded(VideoResolution {
        width: 1280,
        height: 720,
    });
    let track = LocalVideoTrack::create_video_track(
        "owned-empty-encoded-recovery",
        RtcVideoSource::Native(source.clone()),
    );
    let old = tokio::time::timeout(
        Duration::from_secs(8),
        room.local_participant().publish_track(
            LocalTrack::Video(track.clone()),
            TrackPublishOptions {
                source: TrackSource::Screenshare,
                video_codec: VideoCodec::H264,
                video_encoder: VideoEncoderBackend::PreEncoded,
                video_encoding: Some(VideoEncoding {
                    max_bitrate: 2_000_000,
                    max_framerate: 30.0,
                }),
                simulcast: false,
                ..Default::default()
            },
        ),
    )
    .await
    .unwrap();
    let old = old.unwrap();
    assert_eq!(old.encryption_type(), EncryptionType::Gcm);
    let native_id = track.rtc_track().id();
    state.tracks.lock().unwrap().insert(false, track);
    recovery_fixture_publication(source, old.sid(), native_id, options, generation)
}

#[cfg(target_os = "windows")]
async fn recovery_fixture_transition(
    room: &Room,
    current: &mut Publication,
    state: &WorkerState,
    dimensions: (u32, u32),
) -> Result<bool, String> {
    let result = match tokio::time::timeout(
        Duration::from_secs(20),
        recover_software_publication(room, current, state, false, dimensions),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => {
            stop_protected_media(state, "Owned recovery fixture deadline");
            Err("Owned recovery fixture deadline".into())
        }
    };
    // Early retirement/publish failures never reach the hook. Drop its sender
    // so the concurrent cancellation probe also finishes on that failure path.
    drop(current.recovery_pause.take());
    result
}

#[cfg(target_os = "windows")]
async fn close_recovery_controller(session: &mut RtcSession) {
    session.cancel();
    tokio::time::timeout(Duration::from_secs(12), async {
        while session
            .owner
            .as_ref()
            .is_some_and(|owner| !owner.is_finished())
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("Owned recovery controller did not finish normal cancellation");
    // Join only a finished owner; a timed-out spawn_blocking join would still
    // block the test runtime's teardown while waiting for that blocking task.
    if let Some(owner) = session.owner.take() {
        owner.join().expect("Owned recovery controller panicked");
    }
}

#[cfg(target_os = "windows")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "Explicit owned loopback SFU software recovery and cancellation fixture; generated pixels, no hardware encoder or devices"]
async fn actual_protected_software_recovery_preserves_capture_audio_and_cancels_stale_replacements()
{
    let _serial = SFU_TEST_LOCK.lock().await;
    let file = std::env::var("RELAY_RTC_TEST_TICKETS").unwrap();
    let mut fixture: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    let base = fixture["base"].as_str().unwrap().to_owned();
    let base_url = url::Url::parse(&base).unwrap();
    assert!(base_url
        .host_str()
        .unwrap()
        .parse::<std::net::IpAddr>()
        .unwrap()
        .is_loopback());
    for name in ["alice", "bob", "eve"] {
        let channel = if name == "eve" {
            fixture["other_id"].clone()
        } else {
            fixture["room_id"].clone()
        };
        let token = fixture["users"][name]["session"]["token"].as_str().unwrap();
        api(
            &base,
            token,
            "/api/voice/join",
            json!({"channel_id":channel}),
        )
        .await;
        fixture["users"][name]["ticket"] = api(
            &base,
            token,
            "/api/rtc/token",
            json!({"channel_id":channel}),
        )
        .await;
    }
    let ticket = |name: &str| -> RtcTicket {
        let ticket: RtcTicket =
            serde_json::from_value(fixture["users"][name]["ticket"].clone()).unwrap();
        let parsed = url::Url::parse(&ticket.url).unwrap();
        assert!(parsed
            .host_str()
            .unwrap()
            .parse::<std::net::IpAddr>()
            .unwrap()
            .is_loopback());
        ticket
    };
    let mut viewer = RtcSession::connect(
        ticket("bob"),
        Context::default(),
        fixture_call_keys(&fixture, "bob"),
    )
    .unwrap();
    let mut other = RtcSession::connect(
        ticket("eve"),
        Context::default(),
        fixture_call_keys(&fixture, "eve"),
    )
    .unwrap();
    wait_active(&viewer).await;
    wait_active(&other).await;
    let keys = fixture_call_keys(&fixture, "alice");
    let alice = ticket("alice");
    let (room, _events) = tokio::time::timeout(
        Duration::from_secs(15),
        Room::connect(
            &alice.url,
            &alice.token,
            protected_room_options(&keys).unwrap(),
        ),
    )
    .await
    .expect("Owned recovery publisher connection deadline")
    .unwrap();
    let state = recovery_fixture_state(keys, 1);
    let scope_before = state.call_keys.scope.clone();
    let key_epoch_before = state.call_keys.epoch;
    let options = CaptureOptions {
        codec: VideoCodecPreference::Auto,
        adaptive_video: false,
        ..Default::default()
    };
    let mut current = recovery_encoded_fixture(&room, &state, options, 1).await;
    let old_sid = current.sid.clone();

    let audio_source = NativeAudioSource::new(AudioSourceOptions::default(), 48_000, 2, 960);
    let audio_track = LocalAudioTrack::create_audio_track(
        "owned-coexisting-audio",
        RtcAudioSource::Native(audio_source),
    );
    let audio_publication = tokio::time::timeout(
        Duration::from_secs(8),
        room.local_participant().publish_track(
            LocalTrack::Audio(audio_track.clone()),
            TrackPublishOptions {
                source: TrackSource::ScreenshareAudio,
                ..Default::default()
            },
        ),
    )
    .await
    .expect("Owned coexisting audio publication deadline")
    .unwrap();
    assert_eq!(audio_publication.encryption_type(), EncryptionType::Gcm);
    state
        .audio_tracks
        .lock()
        .unwrap()
        .insert(false, audio_track.clone());
    state
        .application_audio_active
        .store(true, Ordering::Release);

    let failure = hardware_reconfiguration::<()>(
        VideoCodecPreference::Auto,
        Err("Owned unsupported adaptive format".into()),
        (1280, 720),
    )
    .unwrap_err();
    let VideoPumpError::SoftwareFallback { dimensions, .. } = failure else {
        panic!("Auto failure did not request recovery");
    };
    assert!(
        recovery_fixture_transition(&room, &mut current, &state, dimensions)
            .await
            .unwrap()
    );
    assert_ne!(current.sid, old_sid);
    let publications = room.local_participant().track_publications();
    assert!(!publications.contains_key(&old_sid));
    assert_eq!(
        publications.get(&current.sid).unwrap().encryption_type(),
        EncryptionType::Gcm
    );
    assert!(publications.contains_key(&audio_publication.sid()));
    assert!(state.application_audio_active.load(Ordering::Acquire));
    assert!(state.audio_tracks.lock().unwrap().contains_key(&false));
    assert_eq!(
        (state.call_keys.scope.as_str(), state.call_keys.epoch),
        (scope_before.as_str(), key_epoch_before)
    );
    assert_eq!(current.generation, 1);
    assert!(matches!(current.pipeline, VideoPipeline::Software));
    let decoded_before = viewer.decoded_frames();
    for _ in 0..90 {
        let mut latest = None;
        for _ in 0..2 {
            match current.receiver.try_recv() {
                Ok(frame) => latest = Some(frame),
                Err(_) => break,
            }
        }
        pump_video(
            &mut current,
            latest,
            &state,
            false,
            &state.call_keys.local_user_id,
        )
        .unwrap();
        assert!(
            other.frames().is_empty(),
            "Software recovery leaked outside its room"
        );
        tokio::time::sleep(Duration::from_millis(33)).await;
    }
    let decoded = viewer.decoded_frames() - decoded_before;
    assert!(
        decoded >= 10,
        "Protected software recovery produced too few decoded frames: {decoded}"
    );
    assert!(
        current.capture.is_active(),
        "Recovery restarted or cancelled the selected capture"
    );
    assert!(
        state.frames.lock().unwrap().frames.is_empty(),
        "Hidden publisher allocated preview pixels"
    );
    let software_track = state.tracks.lock().unwrap().get(&false).unwrap().clone();
    let stats = tokio::time::timeout(Duration::from_secs(3), software_track.get_stats())
        .await
        .expect("Owned software video stats deadline")
        .unwrap();
    assert!(stats.iter().any(|stat| matches!(stat, RtcStats::OutboundRtp(outbound) if outbound.outbound.frames_encoded > 0 && outbound.sent.bytes_sent > 0)));

    // Pause only after the actual replacement has been accepted as GCM. This
    // deterministically places selection/Stop inside the post-publish gap.
    let (reached, replacement) = tokio::sync::oneshot::channel();
    let (resume, wait_resume) = tokio::sync::oneshot::channel();
    current.recovery_pause = Some(RecoveryPause {
        reached,
        resume: wait_resume,
    });
    let (recovered, cancelled_sid) = tokio::join!(
        recovery_fixture_transition(&room, &mut current, &state, (1280, 720)),
        async {
            let sid = tokio::time::timeout(Duration::from_secs(22), replacement)
                .await
                .map_err(|_| "Owned generation probe publication deadline")?
                .map_err(|_| "Recovery ended before the generation probe")?;
            state.epochs[0].store(2, Ordering::Release);
            state.source_states[0].store((2 << 2) | 1, Ordering::Release);
            resume
                .send(())
                .map_err(|_| "Generation probe ended before resume")?;
            Ok::<_, &str>(sid)
        }
    );
    assert!(!recovered.unwrap());
    let cancelled_sid = cancelled_sid.unwrap();
    assert!(
        !state.stop.load(Ordering::Acquire),
        "Changing selection should not terminate the protected room"
    );
    assert!(!room
        .local_participant()
        .track_publications()
        .contains_key(&cancelled_sid));
    assert_eq!(state.epochs[0].load(Ordering::Acquire), 2);
    current.capture.cancel();
    drop(current);

    let mut current = recovery_encoded_fixture(&room, &state, options, 2).await;
    let (reached, replacement) = tokio::sync::oneshot::channel();
    let (resume, wait_resume) = tokio::sync::oneshot::channel();
    current.recovery_pause = Some(RecoveryPause {
        reached,
        resume: wait_resume,
    });
    let (recovered, cancelled_sid) = tokio::join!(
        recovery_fixture_transition(&room, &mut current, &state, (1280, 720)),
        async {
            let sid = tokio::time::timeout(Duration::from_secs(22), replacement)
                .await
                .map_err(|_| "Owned Stop probe publication deadline")?
                .map_err(|_| "Recovery ended before the Stop probe")?;
            stop_protected_media(&state, "Owned cancellation during software recovery");
            resume
                .send(())
                .map_err(|_| "Stop probe ended before resume")?;
            Ok::<_, &str>(sid)
        }
    );
    assert!(!recovered.unwrap());
    let cancelled_sid = cancelled_sid.unwrap();
    assert!(!room
        .local_participant()
        .track_publications()
        .contains_key(&cancelled_sid));
    assert!(room
        .local_participant()
        .track_publications()
        .values()
        .all(|publication| publication.kind() != TrackKind::Video));
    assert!(!state.ready.load(Ordering::Acquire));
    current.capture.cancel();
    tokio::time::timeout(Duration::from_secs(5), room.close())
        .await
        .unwrap()
        .unwrap();

    let failed_state =
        recovery_fixture_state(fixture_call_keys(&fixture, "alice"), current.generation);
    assert!(
        recovery_fixture_transition(&room, &mut current, &failed_state, (1280, 720))
            .await
            .is_err()
    );
    assert!(
        failed_state.stop.load(Ordering::Acquire) && !failed_state.ready.load(Ordering::Acquire)
    );
    drop(current);
    close_recovery_controller(&mut viewer).await;
    close_recovery_controller(&mut other).await;
    std::fs::write(std::path::Path::new(&file).parent().unwrap().join("software-recovery-evidence.json"), serde_json::to_vec_pretty(&json!({
        "scope":"Injected format failure and actual protected H264-publication-to-software-VP8 handoff; no H264 payload or hardware encoder exercised",
        "encryption":"Gcm", "software_decoded_frames":decoded,
        "selected_capture_preserved":true, "coexisting_audio_publication_preserved":true,
        "scope_and_key_epoch_preserved":true, "other_room_empty":true,
        "changed_generation_replacement_retired":true, "stopped_replacement_retired":true,
        "closed_room_precondition_rejection_fails_closed":true,
        "application_audio_pcm_or_capture_pump_exercised":false,
        "physical_devices_or_hardware_encoder_opened":false, "actual_hardware_format_failure_or_wan_acceptance":false
    })).unwrap()).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn software_publication_delivers_native_frames_while_hidden_and_rejects_stale_source() {
    let options = CaptureOptions {
        quality: crate::capture::CaptureQuality::P480,
        fps: 30,
        codec: VideoCodecPreference::Software,
        adaptive_video: false,
        ..Default::default()
    };
    let state = WorkerState {
        stop: Arc::new(AtomicBool::new(false)),
        ready: Arc::new(AtomicBool::new(true)),
        reconnecting: Arc::new(AtomicBool::new(false)),
        video_visible: Arc::new(AtomicBool::new(false)),
        errors: Arc::new(Mutex::new(None)),
        frames: Arc::new(Mutex::new(LatestFrames::default())),
        video_conversion: Arc::new(Mutex::new(())),
        decoded_frames: Arc::new(AtomicU64::new(0)),
        epochs: [Arc::new(AtomicU64::new(1)), Arc::new(AtomicU64::new(1))],
        source_states: [
            Arc::new(AtomicU64::new((1 << 2) | 2)),
            Arc::new(AtomicU64::new(0)),
        ],
        tracks: Arc::new(Mutex::new(HashMap::new())),
        audio_tracks: Arc::new(Mutex::new(HashMap::new())),
        audio_sink: Arc::new(Mutex::new(None)),
        application_audio_active: Arc::new(AtomicBool::new(false)),
        diagnostics: Arc::new(Mutex::new(HashMap::new())),
        wake: Arc::new(tokio::sync::Notify::new()),
        ctx: Context::default(),
        connection_id: 0,
        call_keys: fixture_call_keys(
            &json!({"room_id":"owned-local-pump", "other_id":"unused", "users":{
                "alice":{"session":{"user":{"id":"owned-local-alice"}}},
                "bob":{"session":{"user":{"id":"owned-local-bob"}}},
                "eve":{"session":{"user":{"id":"owned-local-eve"}}}
            }}),
            "alice",
        ),
    };
    let source = NativeVideoSource::new(
        VideoResolution {
            width: 16,
            height: 16,
        },
        true,
    );
    let track = LocalVideoTrack::create_video_track(
        "owned-background-software",
        RtcVideoSource::Native(source.clone()),
    );
    let mut stream = NativeVideoStream::new(track.rtc_track());
    let (sender, receiver) = mpsc::sync_channel(1);
    let capture = CaptureSession::synthetic_raw(sender, 1, options).unwrap();
    let mut publication = Publication {
        capture,
        receiver,
        source,
        sid: "TR_owned_background".to_owned().try_into().unwrap(),
        native_id: track.rtc_track().id(),
        generation: 1,
        options,
        pipeline: VideoPipeline::Software,
        adaptation: AdaptiveVideo::new(options),
        stats_task: None,
        next_stats: Instant::now(),
        previous_sample: None,
        previous_sample_at: None,
        outbound_mbps: None,
        next_frame: None,
        rate_bitrate: None,
        adaptive_samples: 0,
        recovery_pause: None,
        #[cfg(target_os = "windows")]
        audio: None,
    };
    let generated = |timestamp_us| RawCaptureFrame {
        width: 16,
        height: 16,
        rgba: [200, 30, 60, 255].repeat(16 * 16),
        timestamp_us,
    };
    pump_video(
        &mut publication,
        Some(generated(1_000_000)),
        &state,
        false,
        "owned-local-alice",
    )
    .unwrap();
    let delivered = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .expect("Hidden software publication did not reach the actual native video track")
        .expect("Native video stream ended unexpectedly");
    assert_eq!(
        (delivered.buffer.width(), delivered.buffer.height()),
        (16, 16)
    );
    let mut rgba = vec![0; 16 * 16 * 4];
    delivered
        .buffer
        .to_argb(VideoFormatType::ABGR, &mut rgba, 16 * 4, 16, 16);
    assert!(rgba
        .chunks_exact(4)
        .any(|pixel| pixel[0].saturating_sub(pixel[2]) > 80));
    assert!(
        state.frames.lock().unwrap().frames.is_empty(),
        "Hidden preview should not allocate a cached image"
    );

    state.epochs[0].store(2, Ordering::Release);
    pump_video(
        &mut publication,
        Some(generated(1_100_000)),
        &state,
        false,
        "owned-local-alice",
    )
    .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(150), stream.next())
            .await
            .is_err(),
        "An old source generation reached the native track after Stop/replacement"
    );
    publication.capture.cancel();
    track.rtc_track().set_enabled(false);
    drop(stream);
}

#[test]
fn stopped_source_cannot_be_restored_by_a_late_capture_tick() {
    let state = Arc::new(AtomicU64::new((7 << 2) | 2));
    let stopped = Arc::new(std::sync::Barrier::new(2));
    let worker_state = state.clone();
    let worker_barrier = stopped.clone();
    let worker = thread::spawn(move || {
        // The worker already observed the old active capture before Stop.
        assert_eq!(source_phase(&worker_state, 7), 2);
        worker_barrier.wait();
        worker_barrier.wait();
        assert!(!set_source_phase(&worker_state, 7, 3));
    });
    stopped.wait();
    state.store(0, Ordering::Release);
    stopped.wait();
    worker.join().unwrap();
    assert_eq!(source_phase(&state, 7), 0);
}

#[test]
fn stale_source_progress_or_failure_cannot_change_a_new_request() {
    let state = AtomicU64::new((9 << 2) | 1);
    assert!(!set_source_phase(&state, 8, 3));
    assert!(!set_source_phase(&state, 8, 0));
    assert_eq!(source_phase(&state, 9), 1);
    assert!(set_source_phase(&state, 9, 2));
    assert!(set_source_phase(&state, 9, 3));
    assert_eq!(source_phase(&state, 8), 0);
    assert_eq!(source_phase(&state, 9), 3);
    assert!(set_source_phase(&state, 9, 0));
    assert!(!set_source_phase(&state, 9, 3));
}

#[test]
#[ignore = "Native C++ exception regression; closed peer only, no network or media devices"]
fn actual_native_closed_peer_returns_error_without_abort() {
    use livekit::webrtc::{
        peer_connection_factory::{PeerConnectionFactory, RtcConfiguration},
        rtp_transceiver::RtpTransceiverInit,
        MediaType,
    };
    let factory = PeerConnectionFactory::default();
    let peer = factory
        .create_peer_connection(RtcConfiguration::default())
        .unwrap();
    let sender = peer
        .add_transceiver_for_media(
            MediaType::Video,
            RtpTransceiverInit {
                direction: livekit::webrtc::rtp_transceiver::RtpTransceiverDirection::SendOnly,
                stream_ids: Vec::new(),
                send_encodings: Vec::new(),
            },
        )
        .unwrap()
        .sender();
    peer.close();
    for _ in 0..32 {
        let error = peer
            .remove_track(sender.clone())
            .expect_err("Closed peer removal silently succeeded");
        assert!(
            !error.message.is_empty(),
            "Native error lost its explanation"
        );
        assert_eq!(
            error.error_type,
            livekit::webrtc::RtcErrorType::InvalidState
        );
    }
}

#[tokio::test]
async fn idle_controller_wait_does_not_pump_and_existing_notify_still_wakes() {
    let mut tick = controller_pump_interval(Duration::from_secs(1));
    tick.reset_at(tokio::time::Instant::now() - Duration::from_secs(5));
    assert!(tokio::time::timeout(
        Duration::from_millis(25),
        wait_for_controller_maintenance(&mut tick, false, None)
    )
    .await
    .is_err());

    let wake = tokio::sync::Notify::new();
    // A visibility/cancellation notification sent before select retains a permit.
    wake.notify_one();
    tokio::time::timeout(Duration::from_secs(1), async {
        tokio::select! {
            _ = wait_for_controller_maintenance(&mut tick, false, None) => panic!("Idle owner pumped without local work"),
            _ = wake.notified() => {},
        }
    })
    .await
    .expect("Idle owner lost an existing notification");

    // The overdue interval was not consumed while idle. Skip allows one immediate
    // resumption, then a future deadline, rather than replaying five missed ticks.
    tokio::time::timeout(
        Duration::from_secs(1),
        wait_for_controller_maintenance(&mut tick, true, None),
    )
    .await
    .expect("Local source work did not resume the pump");
    assert!(wait_for_controller_maintenance(&mut tick, true, None)
        .now_or_never()
        .is_none());
}

#[tokio::test]
async fn idle_controller_wait_yields_to_source_commands_then_keeps_active_pacing() {
    let mut tick = controller_pump_interval(Duration::from_millis(5));
    let (commands, mut receiver) = tokio::sync::mpsc::channel(1);
    commands.send(true).await.unwrap();
    let camera = tokio::time::timeout(Duration::from_secs(1), async {
        tokio::select! {
            _ = wait_for_controller_maintenance(&mut tick, false, None) => panic!("Idle owner pumped instead of handling a source command"),
            command = receiver.recv() => command.expect("Source command was lost"),
        }
    })
    .await
    .expect("Idle owner did not accept a source command");
    assert!(camera);
    for _ in 0..3 {
        tokio::time::timeout(
            Duration::from_secs(1),
            wait_for_controller_maintenance(&mut tick, true, None),
        )
        .await
        .expect("Active local work lost the existing frame pump");
    }
}

#[tokio::test]
async fn idle_reconnect_wait_uses_original_deadline_without_a_periodic_pump() {
    let mut tick = controller_pump_interval(Duration::from_secs(1));
    let since = Instant::now() - RTC_RECOVERY_TIMEOUT + Duration::from_millis(200);
    let wake = tokio::sync::Notify::new();
    wake.notify_one();
    tokio::select! {
        _ = wait_for_controller_maintenance(&mut tick, false, Some(since)) => panic!("Reconnect wait finished before its original deadline"),
        _ = wake.notified() => {},
    }
    // Re-entering select after another owner event retains the same original
    // deadline even though this room has no publication or pending source job.
    tokio::time::timeout(
        Duration::from_secs(1),
        wait_for_controller_maintenance(&mut tick, false, Some(since)),
    )
    .await
    .expect("Viewer-only reconnect did not reach its deadline");
    assert!(recovery_expired(Some(since), Instant::now()));
}

#[tokio::test]
async fn viewer_visibility_notification_reconciles_cache_without_a_pump_tick() {
    let state = recovery_fixture_state(recovery_fixture_keys(), 7);
    let mut visible = true;
    state.frames.lock().unwrap().insert(
        "owned-recovery-alice:screen".into(),
        RtcFrame {
            user_id: "owned-recovery-alice".into(),
            kind: "screen".into(),
            image: Arc::new(ColorImage::new([2, 2], eframe::egui::Color32::RED)),
            timestamp_us: 1,
        },
    );
    state.wake.notify_one();
    let mut tick = controller_pump_interval(Duration::from_secs(1));
    let mut receivers = HashMap::new();
    tokio::time::timeout(Duration::from_secs(1), async {
        tokio::select! {
            _ = wait_for_controller_maintenance(&mut tick, false, None) => panic!("Hidden viewer required an idle pump tick"),
            _ = state.wake.notified() => refresh_remote_video_visibility(&state, &mut visible, &HashMap::new(), &mut receivers).await,
        }
    })
    .await
    .expect("Viewer visibility notification was lost");
    assert!(!visible);
    assert!(state.frames.lock().unwrap().frames.is_empty());
    state.video_visible.store(true, Ordering::Release);
    state.wake.notify_one();
    tokio::time::timeout(Duration::from_secs(1), async {
        tokio::select! {
            _ = wait_for_controller_maintenance(&mut tick, false, None) => panic!("Visible viewer required an idle pump tick"),
            _ = state.wake.notified() => refresh_remote_video_visibility(&state, &mut visible, &HashMap::new(), &mut receivers).await,
        }
    })
    .await
    .expect("Viewer resumption notification was lost");
    assert!(visible);
}

#[test]
fn recovery_deadline_is_bounded_and_does_not_restart_on_another_retry() {
    let start = Instant::now();
    assert!(!recovery_expired(None, start + Duration::from_secs(60)));
    assert!(!recovery_expired(
        Some(start),
        start + Duration::from_millis(29_999)
    ));
    assert!(recovery_expired(
        Some(start),
        start + Duration::from_secs(30)
    ));
    assert!(recovery_expired(
        Some(start),
        start + Duration::from_secs(60)
    ));
    assert!(!recovery_expired(
        Some(start + Duration::from_secs(1)),
        start
    ));
}

async fn controller_room(session: &RtcSession) -> Arc<Room> {
    let (reply, result) = tokio::sync::oneshot::channel();
    session
        .commands
        .send(Command::InspectRoom(reply))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), result)
        .await
        .unwrap()
        .unwrap()
}

/// An owned loopback signal proxy: established streams close on fault; retry
/// sockets stay open without a WebSocket handshake. No OS/network rules change.
struct OwnedSignalGate {
    url: String,
    blocked: tokio::sync::watch::Sender<bool>,
    held_retries: Arc<AtomicU64>,
    owner: tokio::task::JoinHandle<()>,
}
impl OwnedSignalGate {
    async fn new(original: &str) -> Self {
        let mut url = url::Url::parse(original).unwrap();
        assert_eq!(
            url.scheme(),
            "ws",
            "Fixture must use local unencrypted signalling"
        );
        let host: std::net::IpAddr = url.host_str().unwrap().parse().unwrap();
        assert!(
            host.is_loopback(),
            "Proxy must never connect to a real server"
        );
        let upstream = std::net::SocketAddr::new(host, url.port().unwrap());
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        url.set_host(Some("127.0.0.1")).unwrap();
        url.set_port(Some(listener.local_addr().unwrap().port()))
            .unwrap();
        let (blocked, receiver) = tokio::sync::watch::channel(false);
        let held_retries = Arc::new(AtomicU64::new(0));
        let held = held_retries.clone();
        let owner = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (mut socket, _) = accepted.unwrap();
                        if connections.len() >= 16 { continue; }
                        let mut fault = receiver.clone();
                        let held = held.clone();
                        connections.spawn(async move {
                            if *fault.borrow_and_update() {
                                held.fetch_add(1, Ordering::Relaxed);
                                tokio::select! {
                                    _ = fault.changed() => {},
                                    _ = tokio::time::sleep(Duration::from_secs(60)) => {},
                                }
                                return;
                            }
                            let connected = tokio::time::timeout(Duration::from_secs(3), tokio::net::TcpStream::connect(upstream)).await;
                            let Ok(Ok(mut target)) = connected else { return; };
                            tokio::select! {
                                _ = tokio::io::copy_bidirectional(&mut socket, &mut target) => {},
                                _ = fault.changed() => {},
                            }
                        });
                    },
                    Some(result) = connections.join_next(), if !connections.is_empty() => { result.unwrap(); },
                }
            }
        });
        Self {
            url: url.to_string(),
            blocked,
            held_retries,
            owner,
        }
    }
    fn block(&self, blocked: bool) {
        self.blocked.send_replace(blocked);
    }
    async fn shutdown(&mut self) {
        self.owner.abort();
        assert!(matches!((&mut self.owner).await, Err(error) if error.is_cancelled()));
    }
}
impl Drop for OwnedSignalGate {
    fn drop(&mut self) {
        self.owner.abort();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "Owned loopback signal outage/deadline and Cancel during blocked retry; generated video only"]
async fn actual_controller_signal_outage_deadline_and_retry_cancel() {
    let _serial = SFU_TEST_LOCK.lock().await;
    let file = std::env::var("RELAY_RTC_TEST_TICKETS").unwrap();
    let fixture: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    let base = fixture["base"].as_str().unwrap();
    let token = fixture["users"]["alice"]["session"]["token"]
        .as_str()
        .unwrap();
    api(
        base,
        token,
        "/api/voice/join",
        json!({"channel_id":fixture["room_id"]}),
    )
    .await;
    let mut ticket: RtcTicket = serde_json::from_value(
        api(
            base,
            token,
            "/api/rtc/token",
            json!({"channel_id":fixture["room_id"]}),
        )
        .await,
    )
    .unwrap();
    let mut gate = OwnedSignalGate::new(&ticket.url).await;
    ticket.url = gate.url.clone();
    let keys = fixture_call_keys(&fixture, "alice");
    let mut publisher =
        RtcSession::connect(ticket.clone(), Context::default(), keys.clone()).unwrap();
    wait_active(&publisher).await;
    let room = controller_room(&publisher).await;
    publisher
        .start_screen(
            CaptureSource {
                id: "relay-owned-synthetic-fixture".into(),
                name: "Owned outage pattern".into(),
                kind: crate::capture::SourceKind::Window,
                width: 854,
                height: 480,
            },
            CaptureOptions {
                quality: crate::capture::CaptureQuality::P480,
                fps: 15,
                codec: VideoCodecPreference::Software,
                adaptive_video: false,
                ..Default::default()
            },
        )
        .unwrap();
    let until = Instant::now() + Duration::from_secs(10);
    while !publisher.is_sharing(false) {
        assert!(
            Instant::now() < until,
            "Generated source never became active"
        );
        assert!(publisher.take_error().is_none());
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let publications = room.local_participant().track_publications();
    assert_eq!(publications.len(), 1);
    assert_eq!(
        publications.values().next().unwrap().encryption_type(),
        EncryptionType::Gcm
    );
    gate.block(true);
    let until = Instant::now() + Duration::from_secs(8);
    while !publisher.is_reconnecting() {
        assert!(
            Instant::now() < until,
            "Signal failure did not start production recovery"
        );
        assert!(publisher.take_error().is_none());
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let recovery_started = Instant::now();
    assert!(publisher.is_sharing(false), "Recovery hid the Stop control");
    let until = recovery_started + Duration::from_secs(40);
    while !publisher.owner.as_ref().unwrap().is_finished() {
        assert!(
            Instant::now() < until,
            "Persistent signal outage left an unbounded worker"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let recovery_ms = recovery_started.elapsed().as_millis();
    assert!(
        recovery_ms >= 29_500,
        "Test ended before exercising Relay's 30-second deadline"
    );
    assert!(
        gate.held_retries.load(Ordering::Acquire) > 0,
        "No actual retry reached the blocked signal proxy"
    );
    let explanation = publisher.take_error().unwrap();
    assert!(explanation.contains("within 30 seconds") && explanation.contains("Connect Video"));
    assert!(!publisher.is_active() && !publisher.is_reconnecting() && !publisher.is_sharing(false));
    assert!(publisher.source_diagnostics(false).is_none());
    assert!(
        publisher.tracks.lock().unwrap().is_empty()
            && publisher.audio_tracks.lock().unwrap().is_empty()
    );
    assert!(room.local_participant().track_publications().is_empty());
    close_controller(&mut publisher).await;

    // A fresh, explicit connection has no source selected. Cancel while its
    // real signalling retry is blocked must end promptly and remain quiet.
    gate.block(false);
    let mut renewed = RtcSession::connect(ticket, Context::default(), keys).unwrap();
    wait_active(&renewed).await;
    assert!(!renewed.is_sharing(false) && !renewed.is_source_starting(false));
    let retries_before_cancel_test = gate.held_retries.load(Ordering::Acquire);
    gate.block(true);
    let until = Instant::now() + Duration::from_secs(8);
    while !renewed.is_reconnecting()
        || gate.held_retries.load(Ordering::Acquire) <= retries_before_cancel_test
    {
        assert!(
            Instant::now() < until,
            "Fresh session did not enter blocked retry"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let cancelled = Instant::now();
    renewed.cancel();
    let until = cancelled + Duration::from_secs(7);
    while !renewed.owner.as_ref().unwrap().is_finished() {
        assert!(
            Instant::now() < until,
            "Cancel waited for retry rather than bounded cleanup"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let cancel_ms = cancelled.elapsed().as_millis();
    assert!(
        renewed.take_error().is_none(),
        "Explicit Cancel reported a late reconnect error"
    );
    assert!(!renewed.is_starting() && !renewed.is_active() && !renewed.is_reconnecting());
    close_controller(&mut renewed).await;
    gate.shutdown().await;
    std::fs::write(std::path::Path::new(&file).parent().unwrap().join("rtc-outage-evidence.json"), serde_json::to_vec_pretty(&json!({
        "signal_recovery_ms":recovery_ms,"cancel_during_retry_ms":cancel_ms,
        "actual_blocked_retries":gate.held_retries.load(Ordering::Acquire),
        "local_publications_after_deadline":0,"previous_sources_restarted":false,"cancel_reported_error":false,
        "physical_devices_opened":false,
        "scope":"Actual owned SFU with loopback TCP signal fault, blocked WebSocket retry, Relay 30-second deadline plus cleanup, then explicit source-free reconnect and Cancel. No OS network-rule changes; not complete media-path/WAN or IV-uniqueness acceptance."
    })).unwrap()).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "Owned SFU signal/full reconnect with generated video, including stop after republish"]
async fn actual_controller_reconnect_updates_publications_and_keeps_stop_terminal() {
    let _serial = SFU_TEST_LOCK.lock().await;
    let file = std::env::var("RELAY_RTC_TEST_TICKETS").unwrap();
    let fixture: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    let mut tickets = Vec::new();
    for name in ["alice", "bob"] {
        let base = fixture["base"].as_str().unwrap();
        let token = fixture["users"][name]["session"]["token"].as_str().unwrap();
        api(
            base,
            token,
            "/api/voice/join",
            json!({"channel_id":fixture["room_id"]}),
        )
        .await;
        tickets.push(
            serde_json::from_value::<RtcTicket>(
                api(
                    base,
                    token,
                    "/api/rtc/token",
                    json!({"channel_id":fixture["room_id"]}),
                )
                .await,
            )
            .unwrap(),
        );
    }
    let mut publisher = RtcSession::connect(
        tickets.remove(0),
        Context::default(),
        fixture_call_keys(&fixture, "alice"),
    )
    .unwrap();
    let mut viewer = RtcSession::connect(
        tickets.remove(0),
        Context::default(),
        fixture_call_keys(&fixture, "bob"),
    )
    .unwrap();
    wait_active(&publisher).await;
    wait_active(&viewer).await;
    let room = controller_room(&publisher).await;
    publisher
        .start_screen(
            CaptureSource {
                id: "relay-owned-synthetic-fixture".into(),
                name: "Owned reconnect pattern".into(),
                kind: crate::capture::SourceKind::Window,
                width: 854,
                height: 480,
            },
            CaptureOptions {
                quality: crate::capture::CaptureQuality::P480,
                fps: 15,
                codec: VideoCodecPreference::Software,
                adaptive_video: false,
                ..Default::default()
            },
        )
        .unwrap();
    let until = Instant::now() + Duration::from_secs(15);
    while viewer.decoded_frames() < 10 {
        assert!(
            Instant::now() < until,
            "Initial protected video did not arrive"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let generation = publisher.epochs[0].load(Ordering::Acquire);
    let mut evidence = Vec::new();
    for scenario in [
        livekit::SimulateScenario::SignalReconnect,
        livekit::SimulateScenario::FullReconnect,
        livekit::SimulateScenario::FullReconnect,
    ] {
        let publications = room.local_participant().track_publications();
        assert_eq!(publications.len(), 1);
        let previous_publication = publications.values().next().unwrap().clone();
        #[cfg(target_os = "windows")]
        let previous_track = previous_publication.track().unwrap();
        let previous = previous_publication.sid();
        let decoded = viewer.decoded_frames();
        room.simulate_scenario(scenario).await.unwrap();
        let until = Instant::now() + Duration::from_secs(20);
        let mut saw_reconnecting = false;
        loop {
            if let Some(error) = publisher.take_error().or_else(|| viewer.take_error()) {
                panic!("Protected recovery failed: {error}");
            }
            saw_reconnecting |= room.connection_state() == ConnectionState::Reconnecting;
            let publications = room.local_participant().track_publications();
            let changed =
                publications.len() == 1 && publications.values().next().unwrap().sid() != previous;
            let complete = changed
                && (scenario != livekit::SimulateScenario::SignalReconnect || saw_reconnecting);
            if complete
                && room.connection_state() == ConnectionState::Connected
                && viewer.decoded_frames() >= decoded + 10
            {
                break;
            }
            assert!(
                Instant::now() < until,
                "RTC recovery did not restore generated protected video: scenario={scenario:?}, state={:?}, saw_reconnecting={saw_reconnecting}, changed={changed}, published={}, frames_before={decoded}, frames_now={}, active={}, sharing={}, source_phase={}",
                room.connection_state(),publications.len(),viewer.decoded_frames(),publisher.is_active(),publisher.is_sharing(false),publisher.source_states[0].load(Ordering::Acquire)
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let publications = room.local_participant().track_publications();
        assert_eq!(
            publications.len(),
            1,
            "Recovery duplicated the local publication"
        );
        let publication = publications.values().next().unwrap();
        assert_eq!(publication.encryption_type(), EncryptionType::Gcm);
        if scenario == livekit::SimulateScenario::SignalReconnect {
            assert_ne!(
                publication.sid(),
                previous,
                "Short resume must replace the sender that can freeze a remote decoder"
            );
        }
        assert_eq!(
            publisher.epochs[0].load(Ordering::Acquire),
            generation,
            "Recovery restarted capture generation"
        );
        #[cfg(target_os = "windows")]
        if scenario != livekit::SimulateScenario::SignalReconnect {
            assert_eq!(
                publication.track().unwrap().rtc_track().id(),
                previous_track.rtc_track().id(),
                "Full SDK republish should retain this selected native source"
            );
            concurrent_replay_late(&publisher, previous_publication, previous_track, false)
                .await
                .unwrap();
            let before = viewer.decoded_frames();
            let until = Instant::now() + Duration::from_secs(3);
            while viewer.decoded_frames() < before + 5 {
                assert!(
                    Instant::now() < until,
                    "A delayed old-SID Published event disabled the current native source"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert_eq!(room.local_participant().track_publications().len(), 1);
        }
        evidence.push(json!({"scenario":format!("{scenario:?}"),"same_capture_generation":true,"local_publications":publications.len(),"protected_frames_after_recovery":viewer.decoded_frames()-decoded,"encryption":"Gcm"}));
    }
    room.simulate_scenario(livekit::SimulateScenario::FullReconnect)
        .await
        .unwrap();
    let until = Instant::now() + Duration::from_secs(5);
    while !publisher.is_reconnecting() {
        assert!(
            Instant::now() < until,
            "Production controller did not report reconnecting"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert!(
        publisher.is_sharing(false),
        "An interrupted connection hid the active source's Stop control"
    );
    publisher.stop_screen();
    let until = Instant::now() + Duration::from_secs(5);
    while room.connection_state() != ConnectionState::Connected
        || !room.local_participant().track_publications().is_empty()
    {
        assert!(
            Instant::now() < until,
            "Stopping after full reconnect left an orphan publication on the SFU: state={:?}, published={}, active={}, reconnecting={}, sharing={}, error={:?}",
            room.connection_state(),room.local_participant().track_publications().len(),publisher.is_active(),publisher.is_reconnecting(),publisher.is_sharing(false),publisher.take_error()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!publisher.is_sharing(false));
    assert!(publisher.source_diagnostics(false).is_none());
    tokio::time::sleep(Duration::from_secs(2)).await;
    let stopped_frames = viewer.decoded_frames();
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(
        viewer.decoded_frames(),
        stopped_frames,
        "A stopped source restarted after a late republish"
    );
    assert!(room.local_participant().track_publications().is_empty());
    std::fs::write(std::path::Path::new(&file).parent().unwrap().join("rtc-reconnect-evidence.json"),serde_json::to_vec_pretty(&json!({
        "cycles":evidence,"orphan_publications_after_stop":0,"physical_devices_opened":false,
        "scope":"Actual owned SDK signal resume and two full reconnects through the local SFU; generated pixels, unchanged source generation and authenticated decoding. Not a proof of native IV uniqueness or physical WAN acceptance."
    })).unwrap()).unwrap();
    close_controller(&mut publisher).await;
    close_controller(&mut viewer).await;
}

#[test]
fn terminal_rtc_error_preserves_specific_failure_and_keeps_cancel_silent() {
    assert_eq!(rtc_end_error(Ok(()), false).as_deref(), Some(RTC_ENDED));
    assert_eq!(
        rtc_end_error(Err("protected media failed".into()), false).as_deref(),
        Some("protected media failed")
    );
    assert!(rtc_end_error(Ok(()), true).is_none());
    assert!(rtc_end_error(Err("late transport error".into()), true).is_none());
    let mut empty = None;
    assert!(!RtcSession::discard_ended(&mut empty));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "Explicit owned SFU terminal-disconnect/reconnect regression; generated media only"]
async fn actual_controller_terminal_disconnect_releases_session_without_source_restart() {
    let _serial = SFU_TEST_LOCK.lock().await;
    let file = std::env::var("RELAY_RTC_TEST_TICKETS").unwrap();
    let fixture: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    let mut tickets = Vec::new();
    for name in ["alice", "bob"] {
        let base = fixture["base"].as_str().unwrap();
        let token = fixture["users"][name]["session"]["token"].as_str().unwrap();
        api(
            base,
            token,
            "/api/voice/join",
            json!({"channel_id":fixture["room_id"]}),
        )
        .await;
        tickets.push(
            serde_json::from_value::<RtcTicket>(
                api(
                    base,
                    token,
                    "/api/rtc/token",
                    json!({"channel_id":fixture["room_id"]}),
                )
                .await,
            )
            .unwrap(),
        );
    }
    let ticket = tickets.remove(0);
    let keys = fixture_call_keys(&fixture, "alice");
    let mut publisher =
        Some(RtcSession::connect(ticket.clone(), Context::default(), keys.clone()).unwrap());
    let mut viewer = RtcSession::connect(
        tickets.remove(0),
        Context::default(),
        fixture_call_keys(&fixture, "bob"),
    )
    .unwrap();
    for session in [publisher.as_ref().unwrap(), &viewer] {
        wait_active(session).await;
    }
    assert!(
        !RtcSession::discard_ended(&mut publisher),
        "active session must survive UI polling"
    );
    publisher
        .as_ref()
        .unwrap()
        .start_screen(
            CaptureSource {
                id: "relay-owned-synthetic-fixture".into(),
                name: "Owned disconnect pattern".into(),
                kind: crate::capture::SourceKind::Window,
                width: 854,
                height: 480,
            },
            CaptureOptions {
                quality: crate::capture::CaptureQuality::P480,
                fps: 15,
                codec: VideoCodecPreference::Software,
                adaptive_video: false,
                ..Default::default()
            },
        )
        .unwrap();
    let until = Instant::now() + Duration::from_secs(10);
    while viewer.decoded_frames() < 5 {
        assert!(
            Instant::now() < until,
            "owned source did not reach receiver"
        );
        assert!(publisher.as_ref().unwrap().take_error().is_none());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(publisher.as_ref().unwrap().is_sharing(false));
    // A second connection with this owned fixture identity makes the SFU close
    // the first transport. It publishes no media and opens no physical device.
    let (replacement, _) = tokio::time::timeout(
        Duration::from_secs(10),
        Room::connect(
            &ticket.url,
            &ticket.token,
            protected_room_options(&keys).unwrap(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    let until = Instant::now() + Duration::from_secs(10);
    while !publisher
        .as_ref()
        .unwrap()
        .owner
        .as_ref()
        .unwrap()
        .is_finished()
    {
        assert!(
            Instant::now() < until,
            "terminal disconnect did not finish cleanup"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let old = publisher.as_ref().unwrap();
    assert!(!old.is_active() && !old.is_starting() && !old.is_sharing(false));
    assert!(old.source_diagnostics(false).is_none());
    assert!(old.tracks.lock().unwrap().is_empty() && old.audio_tracks.lock().unwrap().is_empty());
    assert!(!old.is_application_audio_active());
    assert_eq!(old.take_error().as_deref(), Some(RTC_ENDED));
    assert!(RtcSession::discard_ended(&mut publisher));
    assert!(
        publisher.is_none(),
        "UI must be able to request a fresh video ticket"
    );
    tokio::time::timeout(Duration::from_secs(5), replacement.close())
        .await
        .unwrap()
        .unwrap();
    publisher = Some(RtcSession::connect(ticket, Context::default(), keys).unwrap());
    wait_active(publisher.as_ref().unwrap()).await;
    assert!(!RtcSession::discard_ended(&mut publisher));
    let before = viewer.decoded_frames();
    tokio::time::sleep(Duration::from_secs(3)).await;
    let renewed = publisher.as_ref().unwrap();
    assert!(!renewed.is_sharing(false) && !renewed.is_sharing(true));
    assert!(!renewed.is_source_starting(false) && !renewed.is_source_starting(true));
    assert!(
        renewed.source_diagnostics(false).is_none() && renewed.source_diagnostics(true).is_none()
    );
    assert!(
        renewed.tracks.lock().unwrap().is_empty()
            && renewed.audio_tracks.lock().unwrap().is_empty()
    );
    assert!(renewed.take_error().is_none());
    assert_eq!(
        viewer.decoded_frames(),
        before,
        "explicit transport reconnect restarted the old source"
    );
    let evidence = json!({"scope":"Actual owned SFU DuplicateIdentity disconnect and production RTC cleanup followed by explicit transport reconnect; generated source only.","terminal_error_reported":true,"ended_session_removed":true,"active_session_preserved":true,"new_transport_active":true,"previous_sources_restarted":false,"physical_devices_opened":false,"automatic_sdk_resume_or_wan_acceptance":false});
    std::fs::write(
        std::path::Path::new(&file)
            .parent()
            .unwrap()
            .join("terminal-rtc-evidence.json"),
        serde_json::to_vec_pretty(&evidence).unwrap(),
    )
    .unwrap();
    close_controller(publisher.as_mut().unwrap()).await;
    assert!(
        publisher.as_ref().unwrap().take_error().is_none(),
        "normal cancel must not report disconnect"
    );
    close_controller(&mut viewer).await;
}

#[tokio::test]
async fn video_upload_is_measured_without_enabling_adaptive_quality() {
    let state = recovery_fixture_state(recovery_fixture_keys(), 1);
    let source = NativeVideoSource::new(VideoResolution { width: 16, height: 16 }, true);
    let options = CaptureOptions { adaptive_video: false, ..Default::default() };
    let mut publication = recovery_fixture_publication(
        source, "TR_owned_upload".to_owned().try_into().unwrap(),
        "owned-upload".into(), options, 1,
    );
    publication.next_stats = Instant::now() + Duration::from_secs(10);
    publication.previous_sample = Some(VideoSample {
        id: "owned-upload".into(), timestamp: 100, encoded: 50,
        bytes_sent: 500_000, pressure: VideoPressure::Bandwidth,
    });
    publication.previous_sample_at = Some(Instant::now() - Duration::from_secs(2));
    publication.stats_task = Some(tokio::spawn(async {
        Some((VideoSample {
            id: "owned-upload".into(), timestamp: 200, encoded: 100,
            bytes_sent: 1_500_000, pressure: VideoPressure::Bandwidth,
        }, Instant::now()))
    }));
    while !publication.stats_task.as_ref().unwrap().is_finished() {
        tokio::task::yield_now().await;
    }
    let profile = publication.adaptation.profile();
    update_video_adaptation(&mut publication, &state, false).await;
    assert!(publication.outbound_mbps.is_some_and(|(rate, _)| (3.5..4.5).contains(&rate)));
    assert_eq!(publication.adaptive_samples, 0);
    assert_eq!(publication.adaptation.profile(), profile);
    // An unavailable next poll clears the displayed rate instead of reporting zero.
    publication.stats_task = Some(tokio::spawn(async { None }));
    while !publication.stats_task.as_ref().unwrap().is_finished() {
        tokio::task::yield_now().await;
    }
    update_video_adaptation(&mut publication, &state, false).await;
    assert!(publication.outbound_mbps.is_none());
    assert!(publication.previous_sample_at.is_none());
}

#[tokio::test]
async fn video_upload_uses_collection_time_and_rejects_delayed_stale_results() {
    async fn consume(publication: &mut Publication, state: &WorkerState, sample: VideoSample, at: Instant) {
        publication.stats_task = Some(tokio::spawn(async move { Some((sample, at)) }));
        while !publication.stats_task.as_ref().unwrap().is_finished() {
            tokio::task::yield_now().await;
        }
        update_video_adaptation(publication, state, false).await;
    }
    let state = recovery_fixture_state(recovery_fixture_keys(), 1);
    let source = NativeVideoSource::new(VideoResolution { width: 16, height: 16 }, true);
    let options = CaptureOptions { adaptive_video: true, ..Default::default() };
    let mut publication = recovery_fixture_publication(
        source, "TR_owned_delayed_upload".to_owned().try_into().unwrap(),
        "owned-delayed-upload".into(), options, 1,
    );
    publication.next_stats = Instant::now() + Duration::from_secs(30);
    let now = Instant::now();
    let sample = VideoSample {
        id: "owned-delayed-upload".into(), timestamp: 100, encoded: 50,
        bytes_sent: 500_000, pressure: VideoPressure::Bandwidth,
    };
    // Old completed work must not look freshly collected or affect adaptation.
    consume(&mut publication, &state, sample.clone(), now - Duration::from_secs(6)).await;
    assert!(publication.outbound_mbps.is_none());
    assert!(publication.previous_sample.is_none());
    assert!(publication.previous_sample_at.is_none());
    assert_eq!(publication.adaptive_samples, 0);

    consume(&mut publication, &state, sample.clone(), now - Duration::from_secs(4)).await;
    assert!(publication.outbound_mbps.is_none());
    let second_at = now - Duration::from_secs(2);
    consume(&mut publication, &state, VideoSample {
        timestamp: 200, encoded: 100, bytes_sent: 1_500_000, ..sample
    }, second_at).await;
    // Two-second collections consumed almost together still measure exactly 4 Mbps.
    assert_eq!(publication.outbound_mbps, Some((4.0, second_at)));
    assert_eq!(publication.previous_sample_at, Some(second_at));
}

#[test]
fn measured_video_upload_rejects_reset_stale_and_nonprogressing_samples() {
    let previous = VideoSample {
        id: "screen-sender".into(), timestamp: 100, encoded: 50,
        bytes_sent: 500_000, pressure: VideoPressure::Unknown,
    };
    let current = VideoSample {
        timestamp: 200, encoded: 100, bytes_sent: 1_500_000,
        ..previous.clone()
    };
    assert_eq!(video_upload_mbps(&previous, &current, Duration::from_secs(2)), Some(4.0));
    // A confirmed, unchanged byte counter really is zero upload; missing data is not.
    assert_eq!(video_upload_mbps(&previous, &VideoSample { bytes_sent: previous.bytes_sent, ..current.clone() }, Duration::from_secs(2)), Some(0.0));
    for invalid in [
        VideoSample { id: "new-sender".into(), ..current.clone() },
        VideoSample { timestamp: previous.timestamp, ..current.clone() },
        VideoSample { timestamp: previous.timestamp - 1, ..current.clone() },
        VideoSample { encoded: previous.encoded - 1, ..current.clone() },
        VideoSample { bytes_sent: previous.bytes_sent - 1, ..current.clone() },
    ] {
        assert_eq!(video_upload_mbps(&previous, &invalid, Duration::from_secs(2)), None);
    }
    for elapsed in [Duration::ZERO, Duration::from_millis(499), Duration::from_millis(5001)] {
        assert_eq!(video_upload_mbps(&previous, &current, elapsed), None);
    }
}

#[test]
fn adaptive_video_measurements_require_a_live_unambiguous_video_sender() {
    let report = json!({"type":"outbound-rtp","id":"owned-outbound","timestamp":1000,"kind":"video", "active":true,
        "framesEncoded":50,"bytesSent":50000,"frameWidth":1280,"frameHeight":720,
        "qualityLimitationReason":"bandwidth"});
    let stats: RtcStats = serde_json::from_value(report.clone()).unwrap();
    assert!(matches!(
        video_sample(std::slice::from_ref(&stats)).unwrap().pressure,
        VideoPressure::Bandwidth
    ));
    assert!(video_sample(&[stats.clone(), stats]).is_none());
    for field in [
        "id",
        "timestamp",
        "kind",
        "active",
        "framesEncoded",
        "bytesSent",
        "frameWidth",
        "frameHeight",
    ] {
        let mut invalid = report.clone();
        invalid.as_object_mut().unwrap().remove(field);
        assert!(video_sample(&[serde_json::from_value(invalid).unwrap()]).is_none());
    }
    let mut absent = report.clone();
    absent
        .as_object_mut()
        .unwrap()
        .remove("qualityLimitationReason");
    assert_eq!(
        video_sample(&[serde_json::from_value(absent).unwrap()])
            .unwrap()
            .pressure,
        VideoPressure::Unknown
    );
    let mut healthy = report;
    healthy["qualityLimitationReason"] = json!("none");
    healthy["qualityLimitationDurations"] = json!({"none":1.0});
    assert_eq!(
        video_sample(&[serde_json::from_value(healthy).unwrap()])
            .unwrap()
            .pressure,
        VideoPressure::Healthy
    );
}

#[test]
fn rtc_accepts_4k_measurements_and_rejects_untrusted_dimensions() {
    let report = json!({"type":"outbound-rtp","id":"owned-4k-outbound","timestamp":1000,"kind":"video", "active":true,
        "framesEncoded":50,"bytesSent":50000,"frameWidth":3840,"frameHeight":2160,
        "qualityLimitationReason":"bandwidth"});
    assert!(video_sample(&[serde_json::from_value(report.clone()).unwrap()]).is_some());
    assert_eq!(rtc_rgba_bytes(3840, 2160), Some(33_177_600));
    for (width, height) in [
        (0, 2160),
        (3840, 0),
        (3842, 2160),
        (3840, 2162),
        (u32::MAX, u32::MAX),
    ] {
        let mut invalid = report.clone();
        invalid["frameWidth"] = json!(width);
        invalid["frameHeight"] = json!(height);
        assert!(rgba_bytes(width, height).is_none());
        assert!(rtc_rgba_bytes(width, height).is_none());
        assert!(video_sample(&[serde_json::from_value(invalid).unwrap()]).is_none());
        assert!(rgba_to_i420(&RawCaptureFrame {
            width,
            height,
            rgba: vec![],
            timestamp_us: 0
        })
        .is_err());
    }
    for (width, height) in [(853, 479), (3839, 2160), (3840, 2159), (1, 1)] {
        let mut odd = report.clone();
        odd["frameWidth"] = json!(width);
        odd["frameHeight"] = json!(height);
        assert_eq!(
            rgba_bytes(width, height),
            Some(width as usize * height as usize * 4)
        );
        assert!(video_sample(&[serde_json::from_value(odd).unwrap()]).is_some());
        assert!(rtc_rgba_bytes(width, height).is_none());
        assert!(rgba_to_i420(&RawCaptureFrame {
            width,
            height,
            rgba: vec![],
            timestamp_us: 0,
        })
        .is_err());
    }
}

#[test]
fn odd_legacy_i420_frames_convert_and_reach_the_bounded_render_cache() {
    use livekit::webrtc::video_frame::native::VideoFrameBufferExt;
    let (width, height) = (853, 479);
    let mut buffer = I420Buffer::new(width, height);
    assert_eq!((buffer.chroma_width(), buffer.chroma_height()), (427, 240));
    let (y, u, v) = buffer.data_mut();
    y.fill(70);
    u.fill(128);
    v.fill(128);
    let bytes = rgba_bytes(width, height).unwrap();
    assert_eq!(bytes, 1_634_348);
    let mut rgba = vec![0; bytes];
    buffer.to_argb(
        VideoFormatType::ABGR,
        &mut rgba,
        width * 4,
        width as i32,
        height as i32,
    );
    // Check the final odd column and row too, rather than only the first pixel.
    assert!(rgba
        .chunks_exact(4)
        .all(|pixel| { pixel[0] > 20 && pixel[1] > 20 && pixel[2] > 20 && pixel[3] == 255 }));
    let image = Arc::new(ColorImage::from_rgba_unmultiplied(
        [width as usize, height as usize],
        &rgba,
    ));
    let mut cache = LatestFrames::default();
    cache.insert(
        "legacy-screen".into(),
        RtcFrame {
            user_id: "legacy".into(),
            kind: "screen".into(),
            image,
            timestamp_us: 123_456,
        },
    );
    let accepted = &cache.frames["legacy-screen"].frame;
    assert_eq!(accepted.image.size, [853, 479]);
    assert_eq!(accepted.image.pixels.len() * 4, bytes);
    assert_eq!(accepted.timestamp_us, 123_456);
    assert!(cache.frames.len() <= MAX_STREAMS);
    assert!(
        cache
            .frames
            .values()
            .map(|frame| frame.frame.image.pixels.len() * 4)
            .sum::<usize>()
            <= MAX_VIDEO_BYTES
    );
    cache.insert(
        "legacy-screen".into(),
        RtcFrame {
            user_id: "legacy".into(),
            kind: "screen".into(),
            image: Arc::new(ColorImage {
                size: [853, 479],
                pixels: vec![],
            }),
            timestamp_us: 123_457,
        },
    );
    // A malformed replacement cannot remove the last valid frame.
    assert_eq!(cache.frames["legacy-screen"].frame.timestamp_us, 123_456);
}

#[test]
fn latest_4k_frames_evict_oldest_without_growing_the_render_cache() {
    let mut cache = LatestFrames::default();
    let image = Arc::new(ColorImage::new([3840, 2160], eframe::egui::Color32::RED));
    for id in 0..3 {
        cache.insert(
            id.to_string(),
            RtcFrame {
                user_id: id.to_string(),
                kind: "screen".into(),
                image: image.clone(),
                timestamp_us: id,
            },
        );
    }
    assert_eq!(cache.frames.len(), 2);
    assert!(!cache.frames.contains_key("0"));
    assert!(cache.frames.contains_key("1") && cache.frames.contains_key("2"));
    assert!(
        cache
            .frames
            .values()
            .map(|frame| frame.frame.image.pixels.len() * 4)
            .sum::<usize>()
            <= MAX_VIDEO_BYTES
    );

    let mut malformed = ColorImage::new([2, 2], eframe::egui::Color32::RED);
    malformed.size = [4, 2];
    for image in [
        malformed,
        ColorImage::new([3842, 2], eframe::egui::Color32::RED),
        ColorImage::new([0, 2], eframe::egui::Color32::RED),
    ] {
        cache.insert(
            "invalid".into(),
            RtcFrame {
                user_id: "invalid".into(),
                kind: "screen".into(),
                image: Arc::new(image),
                timestamp_us: 0,
            },
        );
        assert!(!cache.frames.contains_key("invalid"));
        assert_eq!(cache.frames.len(), 2);
    }
}

#[test]
fn rtc_4k_pixels_reach_i420_without_an_implicit_1080p_cap() {
    use livekit::webrtc::prelude::VideoBuffer;
    let frame = resize_for_profile(
        RawCaptureFrame {
            width: 3840,
            height: 2160,
            rgba: [200, 30, 60, 255].repeat(3840 * 2160),
            timestamp_us: 123_456,
        },
        VideoProfile {
            quality: crate::capture::CaptureQuality::P2160,
            fps: 60,
        },
    )
    .unwrap();
    assert_eq!(
        (frame.width, frame.height, frame.timestamp_us),
        (3840, 2160, 123_456)
    );
    let buffer = rgba_to_i420(&frame).unwrap();
    assert_eq!((buffer.width(), buffer.height()), (3840, 2160));
    assert!(buffer.data().0.iter().any(|value| *value > 20));
}

#[test]
fn source_publication_uses_real_dimensions_and_profile_bitrate_limits() {
    use crate::capture::{CaptureQuality, SourceKind, VideoBitrate};
    let options = CaptureOptions {
        quality: CaptureQuality::Source,
        fps: 60,
        ..Default::default()
    };
    let mut source = CaptureSource {
        id: "owned-source".into(),
        name: "Generated metadata".into(),
        kind: SourceKind::Window,
        width: 640,
        height: 360,
    };
    assert_eq!(publication_dimensions(options, &source), (640, 360));
    source.width = 7680;
    source.height = 4320;
    assert_eq!(publication_dimensions(options, &source), (3840, 2160));
    source.width = 1080;
    source.height = 3840;
    assert_eq!(publication_dimensions(options, &source), (606, 2160));
    assert_eq!(
        hardware_video_bitrate(options, 3840, 2160, 60, None),
        32_000_000
    );
    assert_eq!(
        hardware_video_bitrate(options, 3840, 2160, 30, Some(u64::MAX)),
        16_000_000
    );
    assert_eq!(
        hardware_video_bitrate(options, 2560, 1440, 30, None),
        8_000_000
    );
    assert_eq!(
        hardware_video_bitrate(options, 1280, 720, 30, None),
        4_000_000
    );
    assert_eq!(
        hardware_video_bitrate(options, 1280, 720, 30, Some(800_000)),
        800_000
    );
    assert_eq!(
        hardware_video_bitrate(
            CaptureOptions {
                bitrate: VideoBitrate::Mbps8,
                ..options
            },
            3840,
            2160,
            60,
            Some(u64::MAX)
        ),
        8_000_000
    );
}

#[test]
fn adapted_video_frames_preserve_aspect_source_clock_and_user_cap() {
    let frame = RawCaptureFrame {
        width: 1280,
        height: 720,
        rgba: vec![55; 1280 * 720 * 4],
        timestamp_us: 234567,
    };
    let output = resize_for_profile(
        frame,
        VideoProfile {
            quality: crate::capture::CaptureQuality::P480,
            fps: 15,
        },
    )
    .unwrap();
    assert_eq!(
        (output.width, output.height, output.timestamp_us),
        (852, 480, 234567)
    );
    assert_eq!(
        output.rgba.len(),
        output.width as usize * output.height as usize * 4
    );
    let small = RawCaptureFrame {
        width: 320,
        height: 180,
        rgba: vec![55; 320 * 180 * 4],
        timestamp_us: 234567,
    };
    let output = resize_for_profile(
        small,
        VideoProfile {
            quality: crate::capture::CaptureQuality::P720,
            fps: 30,
        },
    )
    .unwrap();
    assert_eq!((output.width, output.height), (320, 180));
    let odd = resize_for_profile(
        RawCaptureFrame {
            width: 17,
            height: 9,
            rgba: vec![55; 17 * 9 * 4],
            timestamp_us: 234567,
        },
        VideoProfile {
            quality: crate::capture::CaptureQuality::Source,
            fps: 24,
        },
    )
    .unwrap();
    assert_eq!((odd.width, odd.height, odd.timestamp_us), (16, 8, 234567));
    assert_eq!(odd.rgba.len(), 16 * 8 * 4);
    assert!(resize_for_profile(
        RawCaptureFrame {
            width: 1920,
            height: 1080,
            rgba: vec![0; 4],
            timestamp_us: 0
        },
        VideoProfile {
            quality: crate::capture::CaptureQuality::P480,
            fps: 15
        }
    )
    .is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "Explicit local SFU production adaptive telemetry with generated pixels only"]
async fn actual_controller_adaptive_video_telemetry_and_cleanup() {
    let _serial = SFU_TEST_LOCK.lock().await;
    let file = std::env::var("RELAY_RTC_TEST_TICKETS").unwrap();
    let fixture: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    let mut tickets = Vec::new();
    for name in ["alice", "bob"] {
        let base = fixture["base"].as_str().unwrap();
        let token = fixture["users"][name]["session"]["token"].as_str().unwrap();
        api(
            base,
            token,
            "/api/voice/join",
            json!({"channel_id":fixture["room_id"]}),
        )
        .await;
        tickets.push(
            serde_json::from_value::<RtcTicket>(
                api(
                    base,
                    token,
                    "/api/rtc/token",
                    json!({"channel_id":fixture["room_id"]}),
                )
                .await,
            )
            .unwrap(),
        );
    }
    let publisher = RtcSession::connect(
        tickets.remove(0),
        Context::default(),
        fixture_call_keys(&fixture, "alice"),
    )
    .unwrap();
    let viewer = RtcSession::connect(
        tickets.remove(0),
        Context::default(),
        fixture_call_keys(&fixture, "bob"),
    )
    .unwrap();
    wait_active(&publisher).await;
    wait_active(&viewer).await;
    let source = CaptureSource {
        id: "relay-owned-synthetic-fixture".into(),
        name: "Owned adaptive video pattern".into(),
        kind: crate::capture::SourceKind::Window,
        width: 854,
        height: 480,
    };
    publisher
        .start_screen(
            source,
            CaptureOptions {
                quality: crate::capture::CaptureQuality::P480,
                fps: 15,
                codec: VideoCodecPreference::Software,
                adaptive_video: true,
                ..Default::default()
            },
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let diagnostics = loop {
        if let Some(error) = publisher.take_error() {
            panic!("Adaptive production RTC failed: {error}");
        }
        if let Some(diagnostics) = publisher.source_diagnostics(false) {
            if diagnostics.adaptive_samples > 0 && viewer.decoded_frames() >= 20 {
                break diagnostics;
            }
        }
        assert!(
            Instant::now() < deadline,
            "Production RTC telemetry did not yield a valid progressing sender sample"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert!(diagnostics
        .adaptive_output
        .as_ref()
        .unwrap()
        .contains("480p"));
    let evidence = json!({"generated_pixels_only":true,"physical_devices_opened":false,"adaptive_quality_samples":diagnostics.adaptive_samples,
        "decoded_frames":viewer.decoded_frames(),"upper_profile":"480p / 15 fps", "output":diagnostics.adaptive_output,
        "scope":"Actual production telemetry, encoder and protected SFU path on one PC; WAN pressure and subjective quality remain open."});
    std::fs::write(
        std::path::Path::new(&file)
            .parent()
            .unwrap()
            .join("adaptive-video-evidence.json"),
        serde_json::to_vec_pretty(&evidence).unwrap(),
    )
    .unwrap();
    publisher.stop_screen();
    let deadline = Instant::now() + Duration::from_secs(3);
    while publisher.source_diagnostics(false).is_some() {
        assert!(
            Instant::now() < deadline,
            "Stopped source kept adaptive diagnostics/task state"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    for mut session in [publisher, viewer] {
        session.cancel();
        let handle = session.owner.take().unwrap();
        tokio::task::spawn_blocking(move || handle.join().unwrap())
            .await
            .unwrap();
    }
}

pub(super) fn fixture_call_keys(fixture: &Value, local_name: &str) -> Arc<CallKeys> {
    let sender_keys = [("alice", 71_u8), ("bob", 83_u8), ("eve", 97_u8)]
        .into_iter()
        .map(|(name, byte)| {
            let user = fixture["users"][name]["session"]["user"]["id"]
                .as_str()
                .unwrap();
            (
                user.to_owned(),
                crate::e2ee::SenderKeys {
                    device_id: format!("owned-device-{name}"),
                    opus: zeroize::Zeroizing::new([byte; 32]),
                    rtc: zeroize::Zeroizing::new([byte + 1; 32]),
                },
            )
        })
        .collect();
    let local = fixture["users"][local_name]["session"]["user"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let channel = if local_name == "eve" {
        "other_id"
    } else {
        "room_id"
    };
    Arc::new(CallKeys {
        group_id: format!("owned-rtc-{}", fixture[channel].as_str().unwrap()),
        scope: fixture[channel].as_str().unwrap().into(),
        epoch: 9,
        local_user_id: local,
        local_device_id: format!("owned-device-{local_name}"),
        sender_keys,
        voice_replay: std::sync::Mutex::default(),
    })
}

async fn wait_active(session: &RtcSession) {
    let until = Instant::now() + Duration::from_secs(12);
    while !session.is_active() {
        if let Some(error) = session.take_error() {
            panic!("RTC fixture connection failed: {error}");
        }
        assert!(Instant::now() < until, "RTC connection readiness deadline");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
async fn publish(room: &Room) -> (NativeVideoSource, LocalVideoTrack, LocalTrackPublication) {
    let source = NativeVideoSource::new(
        VideoResolution {
            width: 1280,
            height: 720,
        },
        true,
    );
    let track = LocalVideoTrack::create_video_track(
        "generated-test-pattern",
        RtcVideoSource::Native(source.clone()),
    );
    let publication = room
        .local_participant()
        .publish_track(
            LocalTrack::Video(track.clone()),
            TrackPublishOptions {
                source: TrackSource::Screenshare,
                video_codec: VideoCodec::VP8,
                video_encoding: Some(VideoEncoding {
                    max_bitrate: 2_000_000,
                    max_framerate: 30.0,
                }),
                simulcast: false,
                ..Default::default()
            },
        )
        .await
        .expect("Publish generated video");
    assert_eq!(publication.encryption_type(), EncryptionType::Gcm);
    (source, track, publication)
}
async fn feed(
    source: &NativeVideoSource,
    viewer: &RtcSession,
    other: &RtcSession,
    count: usize,
    clock: &mut i64,
) -> (usize, bool) {
    let mut received = HashSet::new();
    let mut has_color = false;
    let start = tokio::time::Instant::now();
    for index in 0..count {
        let mut buffer = I420Buffer::new(1280, 720);
        let (y, u, v) = buffer.data_mut();
        y.fill(80 + (index % 30) as u8);
        u.fill(90);
        v.fill(230);
        *clock += 33_333;
        source.capture_frame(&VideoFrame {
            rotation: VideoRotation::VideoRotation0,
            timestamp_us: *clock,
            frame_metadata: None,
            buffer,
        });
        tokio::time::sleep_until(start + Duration::from_micros((index as u64 + 1) * 33_333)).await;
        for frame in viewer.frames() {
            assert_eq!(frame.kind, "screen");
            assert_eq!(frame.image.size, [1280, 720]);
            received.insert(frame.timestamp_us);
            has_color |= frame
                .image
                .pixels
                .iter()
                .take(20)
                .any(|p| p.r().abs_diff(p.b()) > 40);
        }
        assert!(other.frames().is_empty(), "Video leaked into another room");
    }
    (received.len(), has_color)
}
async fn api(base: &str, token: &str, path: &str, body: Value) -> Value {
    let (base, token, path) = (base.to_owned(), token.to_owned(), path.to_owned());
    tokio::task::spawn_blocking(move || {
        reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap()
            .post(format!("{base}{path}"))
            .bearer_auth(token)
            .json(&body)
            .send()
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .unwrap_or(Value::Null)
    })
    .await
    .unwrap()
}

async fn rejection_frames(source: &NativeVideoSource, count: usize, clock: &mut i64) {
    for index in 0..count {
        let mut buffer = I420Buffer::new(1280, 720);
        let (y, u, v) = buffer.data_mut();
        y.fill(80 + (index % 30) as u8);
        u.fill(90);
        v.fill(230);
        *clock += 33_333;
        source.capture_frame(&VideoFrame {
            rotation: VideoRotation::VideoRotation0,
            timestamp_us: *clock,
            frame_metadata: None,
            buffer,
        });
        tokio::time::sleep(Duration::from_millis(33)).await;
    }
}
async fn close_controller(session: &mut RtcSession) {
    session.cancel();
    if let Some(owner) = session.owner.take() {
        tokio::task::spawn_blocking(move || owner.join().unwrap())
            .await
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "Explicit local SFU E2EE rejection fixture; generated frames only"]
async fn actual_sfu_protected_video_rejects_plaintext_wrong_key_and_unknown_sender() {
    let _serial = SFU_TEST_LOCK.lock().await;
    let file = std::env::var("RELAY_RTC_TEST_TICKETS").unwrap();
    let fixture: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    let mut tickets = HashMap::new();
    for name in ["alice", "bob", "eve"] {
        let base = fixture["base"].as_str().unwrap();
        let token = fixture["users"][name]["session"]["token"].as_str().unwrap();
        api(
            base,
            token,
            "/api/voice/join",
            json!({"channel_id":fixture["room_id"]}),
        )
        .await;
        tickets.insert(
            name,
            serde_json::from_value::<RtcTicket>(
                api(
                    base,
                    token,
                    "/api/rtc/token",
                    json!({"channel_id":fixture["room_id"]}),
                )
                .await,
            )
            .unwrap(),
        );
    }
    let bob_keys = || {
        let mut keys = fixture_call_keys(&fixture, "bob");
        let eve = fixture["users"]["eve"]["session"]["user"]["id"]
            .as_str()
            .unwrap();
        Arc::get_mut(&mut keys).unwrap().sender_keys.remove(eve);
        keys
    };
    let mut viewer =
        RtcSession::connect(tickets["bob"].clone(), Context::default(), bob_keys()).unwrap();
    wait_active(&viewer).await;
    let alice = &tickets["alice"];
    let (plain_room, _events) = Room::connect(&alice.url, &alice.token, RoomOptions::default())
        .await
        .unwrap();
    // This publication deliberately uses no encryption to model a downgrade.
    let plain_source = NativeVideoSource::new(
        VideoResolution {
            width: 1280,
            height: 720,
        },
        true,
    );
    let plain_track = LocalVideoTrack::create_video_track(
        "deliberately-unprotected-fixture",
        RtcVideoSource::Native(plain_source.clone()),
    );
    let plain_publication = plain_room
        .local_participant()
        .publish_track(
            LocalTrack::Video(plain_track),
            TrackPublishOptions {
                source: TrackSource::Screenshare,
                video_codec: VideoCodec::VP8,
                simulcast: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(plain_publication.encryption_type(), EncryptionType::None);
    let mut clock = 1_000_000;
    rejection_frames(&plain_source, 24, &mut clock).await;
    assert_eq!(viewer.decoded_frames(), 0);
    assert!(viewer.frames().is_empty() && viewer.is_active());
    plain_room.close().await.unwrap();
    close_controller(&mut viewer).await;

    let mut bad_keys = bob_keys();
    let alice_id = fixture["users"]["alice"]["session"]["user"]["id"]
        .as_str()
        .unwrap();
    Arc::get_mut(&mut bad_keys)
        .unwrap()
        .sender_keys
        .get_mut(alice_id)
        .unwrap()
        .rtc[0] ^= 1;
    let mut wrong_viewer =
        RtcSession::connect(tickets["bob"].clone(), Context::default(), bad_keys).unwrap();
    wait_active(&wrong_viewer).await;
    let (protected_room, _events) = Room::connect(
        &alice.url,
        &alice.token,
        protected_room_options(&fixture_call_keys(&fixture, "alice")).unwrap(),
    )
    .await
    .unwrap();
    let (source, track, publication) = publish(&protected_room).await;
    rejection_frames(&source, 30, &mut clock).await;
    assert_eq!(
        wrong_viewer.decoded_frames(),
        0,
        "wrong key delivered plaintext pixels"
    );
    assert!(wrong_viewer.frames().is_empty());
    let authentication_error = wrong_viewer.take_error();
    assert!(
        authentication_error.is_some(),
        "wrong-key denial must report a protected RTC failure"
    );
    close_controller(&mut wrong_viewer).await;

    // Reconnect with the correct key while keeping the same publisher/codec.
    let mut correct =
        RtcSession::connect(tickets["bob"].clone(), Context::default(), bob_keys()).unwrap();
    wait_active(&correct).await;
    rejection_frames(&source, 45, &mut clock).await;
    assert!(
        correct.decoded_frames() >= 5,
        "correct-key control did not decode"
    );
    let mut sent_bytes = 0_u64;
    for stat in track.get_stats().await.unwrap() {
        if let RtcStats::OutboundRtp(stat) = stat {
            sent_bytes += stat.sent.bytes_sent;
        }
    }
    assert!(sent_bytes > 0);
    protected_room
        .local_participant()
        .unpublish_track(&publication.sid())
        .await
        .unwrap();
    protected_room.close().await.unwrap();
    let until = Instant::now() + Duration::from_secs(3);
    while !correct.frames().is_empty() {
        assert!(
            Instant::now() < until,
            "closed protected source retained pixels"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let before = correct.decoded_frames();
    let eve = &tickets["eve"];
    let (unknown_room, _events) = Room::connect(
        &eve.url,
        &eve.token,
        protected_room_options(&fixture_call_keys(&fixture, "eve")).unwrap(),
    )
    .await
    .unwrap();
    let (unknown_source, _track, _publication) = publish(&unknown_room).await;
    rejection_frames(&unknown_source, 24, &mut clock).await;
    assert_eq!(
        correct.decoded_frames(),
        before,
        "unverified sender subscribed"
    );
    assert!(correct.frames().is_empty());
    unknown_room.close().await.unwrap();
    close_controller(&mut correct).await;
    let evidence = json!({"fixture":"native-SFU-GCM-fail-closed-synthetic-VP8","plaintext_publication_rejected":true,"wrong_participant_key_no_pixels":true,"wrong_key_reported_failure":authentication_error.is_some(),"correct_key_decoded_frames":before,"encrypted_publisher_rtp_bytes":sent_bytes,"unverified_same_room_sender_rejected":true,"physical_devices_opened":false});
    std::fs::write(
        std::path::Path::new(&file)
            .parent()
            .unwrap()
            .join("protected-rtc-rejection-evidence.json"),
        serde_json::to_vec_pretty(&evidence).unwrap(),
    )
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "Explicit local SFU fixture required; generated video only, no hardware"]
async fn actual_sfu_synthetic_video_room_scope_stop_rejoin_and_revoke() {
    let _serial = SFU_TEST_LOCK.lock().await;
    let file = std::env::var("RELAY_RTC_TEST_TICKETS")
        .expect("RELAY_RTC_TEST_TICKETS must point to private owned fixture JSON");
    let mut fixture: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    for name in ["alice", "bob", "eve"] {
        let channel = if name == "eve" {
            fixture["other_id"].clone()
        } else {
            fixture["room_id"].clone()
        };
        let base = fixture["base"].as_str().unwrap();
        let token = fixture["users"][name]["session"]["token"].as_str().unwrap();
        api(
            base,
            token,
            "/api/voice/join",
            json!({"channel_id":channel}),
        )
        .await;
        let ticket = api(base, token, "/api/rtc/token", json!({"channel_id":channel})).await;
        fixture["users"][name]["ticket"] = ticket;
    }
    let ticket = |name: &str| -> RtcTicket {
        serde_json::from_value(fixture["users"][name]["ticket"].clone()).unwrap()
    };
    let viewer = RtcSession::connect(
        ticket("bob"),
        Context::default(),
        fixture_call_keys(&fixture, "bob"),
    )
    .unwrap();
    let other = RtcSession::connect(
        ticket("eve"),
        Context::default(),
        fixture_call_keys(&fixture, "eve"),
    )
    .unwrap();
    wait_active(&viewer).await;
    wait_active(&other).await;
    let alice = ticket("alice");
    let (room, _events) = Room::connect(
        &alice.url,
        &alice.token,
        protected_room_options(&fixture_call_keys(&fixture, "alice")).unwrap(),
    )
    .await
    .unwrap();
    let (source, track, publication) = publish(&room).await;
    let mut clock = 1_000_000;
    let started = Instant::now();
    let (first_count, color) = feed(&source, &viewer, &other, 90, &mut clock).await;
    assert!(
        first_count >= 10,
        "Too few decoded generated frames: {first_count}"
    );
    assert!(
        color,
        "Expected generated chroma to survive actual VP8 encode/decode"
    );
    let mut bytes = 0_u64;
    let mut encoded = 0_u32;
    for stat in track.get_stats().await.unwrap() {
        if let RtcStats::OutboundRtp(stat) = stat {
            bytes = bytes.saturating_add(stat.sent.bytes_sent);
            encoded = encoded.saturating_add(stat.outbound.frames_encoded);
        }
    }
    assert!(bytes > 0 && encoded > 0, "Native sender RTP stats missing");
    let first_elapsed = started.elapsed().as_secs_f64();
    let decoded = viewer.decoded_frames();
    viewer.set_video_visible(false);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let hidden_count = viewer.decoded_frames();
    feed(&source, &viewer, &other, 30, &mut clock).await;
    assert_eq!(
        viewer.decoded_frames(),
        hidden_count,
        "Hidden video continued into decoded-frame consumer"
    );
    assert!(viewer.frames().is_empty());
    viewer.set_video_visible(true);
    let (resumed, _) = feed(&source, &viewer, &other, 30, &mut clock).await;
    assert!(resumed >= 5, "Video subscriptions did not resume");
    track.rtc_track().set_enabled(false);
    room.local_participant()
        .unpublish_track(&publication.sid())
        .await
        .unwrap();
    let until = Instant::now() + Duration::from_secs(3);
    while !viewer.frames().is_empty() {
        assert!(
            Instant::now() < until,
            "Unpublished stream retained in receiver cache"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    room.close().await.unwrap();
    let (rejoined, _events2) = Room::connect(
        &alice.url,
        &alice.token,
        protected_room_options(&fixture_call_keys(&fixture, "alice")).unwrap(),
    )
    .await
    .unwrap();
    let (source2, _track2, _publication2) = publish(&rejoined).await;
    let (rejoin_count, _) = feed(&source2, &viewer, &other, 45, &mut clock).await;
    assert!(
        rejoin_count >= 5,
        "Video did not resume after explicit reconnect"
    );
    let base = fixture["base"].as_str().unwrap();
    let token = fixture["users"]["alice"]["session"]["token"]
        .as_str()
        .unwrap();
    api(base, token, "/api/voice/leave", json!({})).await;
    let until = Instant::now() + Duration::from_secs(8);
    while rejoined.connection_state() != ConnectionState::Disconnected {
        assert!(
            Instant::now() < until,
            "Relay leave did not revoke active RTC participant"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        Room::connect(
            &alice.url,
            &alice.token,
            protected_room_options(&fixture_call_keys(&fixture, "alice")).unwrap()
        )
        .await
        .is_err(),
        "Revoked cached JWT rejoined through Relay gateway"
    );
    let until = Instant::now() + Duration::from_secs(3);
    while !viewer.frames().is_empty() {
        assert!(Instant::now() < until, "Revoked source still visible");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let evidence = json!({"fixture":"syntheticI420-localSFU", "personal_hardware_opened":false,"e2ee":"LiveKitGCM-per-verified-participant",
        "input_frames":90,"input_size":[1280,720],"target_fps":30,"unique_preview_timestamps_observed":first_count,"decoded_frames":decoded,
        "elapsed_seconds":first_elapsed,"observed_decode_fps":decoded as f64 / first_elapsed,
        "rtp_payload_bytes_sent":bytes,"frames_encoded":encoded,"rejoin_decoded_frames":rejoin_count,
        "e2ee":"LiveKitGCM-per-verified-participant","other_room_empty":true,"stop_cleared_cache":true,"relay_leave_revoked_active_participant":true,"cached_revoked_token_rejected":true,"hidden_video_unsubscribed_and_resumed":true,
        "limitations":"One-PC loopback; not physical capture, cross-PC latency, GPU encoding, desktop audio or A/V sync acceptance"});
    let evidence_path = std::path::Path::new(&file)
        .parent()
        .unwrap()
        .join("native-rtc-evidence.json");
    std::fs::write(evidence_path, serde_json::to_vec_pretty(&evidence).unwrap()).unwrap();
    for mut session in [viewer, other] {
        session.cancel();
        assert!(!session.is_active() && session.frames().is_empty());
        let handle = session.owner.take().unwrap();
        tokio::task::spawn_blocking(move || handle.join().unwrap())
            .await
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "Explicit actual SFU and synthetic GPU codec acceptance; no capture devices"]
async fn actual_controller_hardware_h264_generated_video_and_cancel() {
    let _serial = SFU_TEST_LOCK.lock().await;
    let file = std::env::var("RELAY_RTC_TEST_TICKETS").unwrap();
    let fixture: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    let mut tickets = Vec::new();
    for name in ["alice", "bob"] {
        let base = fixture["base"].as_str().unwrap();
        let token = fixture["users"][name]["session"]["token"].as_str().unwrap();
        api(
            base,
            token,
            "/api/voice/join",
            json!({"channel_id":fixture["room_id"]}),
        )
        .await;
        tickets.push(
            serde_json::from_value::<RtcTicket>(
                api(
                    base,
                    token,
                    "/api/rtc/token",
                    json!({"channel_id":fixture["room_id"]}),
                )
                .await,
            )
            .unwrap(),
        );
    }
    let mut publisher = RtcSession::connect(
        tickets.remove(0),
        Context::default(),
        fixture_call_keys(&fixture, "alice"),
    )
    .unwrap();
    let mut viewer = RtcSession::connect(
        tickets.remove(0),
        Context::default(),
        fixture_call_keys(&fixture, "bob"),
    )
    .unwrap();
    wait_active(&publisher).await;
    wait_active(&viewer).await;
    let source = CaptureSource {
        id: "relay-owned-synthetic-fixture".into(),
        name: "Owned generated test pattern".into(),
        kind: crate::capture::SourceKind::Window,
        width: 1280,
        height: 720,
    };
    publisher
        .start_screen(
            source.clone(),
            CaptureOptions {
                codec: VideoCodecPreference::Hardware,
                ..Default::default()
            },
        )
        .unwrap();
    let start = Instant::now();
    let until = start + Duration::from_secs(10);
    while viewer.decoded_frames() < 30 {
        if let Some(error) = publisher.take_error() {
            panic!("Production hardware RTC path failed: {error}");
        }
        assert!(
            Instant::now() < until,
            "Hardware H.264 did not arrive through SFU"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(publisher.is_sharing(false));
    assert!(
        !publisher.frames().is_empty(),
        "Selected local source preview missing"
    );
    let diagnostics = publisher.source_diagnostics(false).unwrap();
    assert!(diagnostics.video_codec.starts_with("Hardware H.264"));
    let received = viewer.decoded_frames();
    let seconds = start.elapsed().as_secs_f64();
    publisher.stop_screen();
    assert!(!publisher.is_sharing(false));
    let until = Instant::now() + Duration::from_secs(3);
    while !viewer.frames().is_empty() {
        assert!(
            Instant::now() < until,
            "Hardware source retained after stop"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    publisher
        .start_screen(
            source,
            CaptureOptions {
                codec: VideoCodecPreference::Hardware,
                ..Default::default()
            },
        )
        .unwrap();
    publisher.stop_screen();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!publisher.is_sharing(false) && !publisher.is_source_starting(false));
    assert!(
        viewer.frames().is_empty(),
        "Cancelled queued source published later"
    );
    let evidence = json!({"fixture":"productionRtcController-syntheticRGBA-hardwareH264","size":[1280,720],"target_fps":30,"decoded_frames":received,"elapsed_seconds":seconds,"observed_fps_including_startup":received as f64/seconds,"encoder":diagnostics.video_codec,"capture_delivered_fps":diagnostics.delivered_fps,"stop_cleared_remote":true,"cancelled_queued_source_did_not_publish":true,"local_source_preview":true,"personal_hardware_opened":false,"e2ee":"LiveKitGCM-per-verified-participant","limitations":"Synthetic capture, actualhardwareencoderandSFU. CPUstaging, notzerocopy; startupincluded; no physicalcamera, networklatencyorAVsyncacceptance."});
    std::fs::write(
        std::path::Path::new(&file)
            .parent()
            .unwrap()
            .join("hardware-rtc-evidence.json"),
        serde_json::to_vec_pretty(&evidence).unwrap(),
    )
    .unwrap();
    for session in [&mut publisher, &mut viewer] {
        session.cancel();
        let handle = session.owner.take().unwrap();
        tokio::task::spawn_blocking(move || handle.join().unwrap())
            .await
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "Explicit local SFU generated application-audio acceptance; no playback device"]
async fn actual_sfu_generated_application_audio_hidden_video_and_cleanup() {
    let _serial = SFU_TEST_LOCK.lock().await;
    let file = std::env::var("RELAY_RTC_TEST_TICKETS").unwrap();
    let fixture: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    let mut tickets = Vec::new();
    for name in ["alice", "bob", "eve"] {
        let channel = if name == "eve" {
            fixture["other_id"].clone()
        } else {
            fixture["room_id"].clone()
        };
        let base = fixture["base"].as_str().unwrap();
        let token = fixture["users"][name]["session"]["token"].as_str().unwrap();
        api(
            base,
            token,
            "/api/voice/join",
            json!({"channel_id":channel}),
        )
        .await;
        tickets.push(
            serde_json::from_value::<RtcTicket>(
                api(base, token, "/api/rtc/token", json!({"channel_id":channel})).await,
            )
            .unwrap(),
        );
    }
    let publisher = tickets.remove(0);
    let mut viewer = RtcSession::connect(
        tickets.remove(0),
        Context::default(),
        fixture_call_keys(&fixture, "bob"),
    )
    .unwrap();
    let mut other = RtcSession::connect(
        tickets.remove(0),
        Context::default(),
        fixture_call_keys(&fixture, "eve"),
    )
    .unwrap();
    let sink = ApplicationAudioSink::default();
    let other_sink = ApplicationAudioSink::default();
    viewer.set_application_audio_sink(sink.clone());
    other.set_application_audio_sink(other_sink.clone());
    viewer.set_video_visible(false);
    wait_active(&viewer).await;
    wait_active(&other).await;
    let (room, _events) = Room::connect(
        &publisher.url,
        &publisher.token,
        protected_room_options(&fixture_call_keys(&fixture, "alice")).unwrap(),
    )
    .await
    .unwrap();
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
        "generated-selected-app-audio",
        RtcAudioSource::Native(source.clone()),
    );
    let publication = room
        .local_participant()
        .publish_track(
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
        )
        .await
        .unwrap();
    assert_eq!(publication.encryption_type(), EncryptionType::Gcm);
    let mut energy = 0_f64;
    let mut samples = 0;
    let mut phase = 0_f32;
    let mut stereo_power = [[0.0_f64; 2]; 2];
    let mut render = crate::media::application_audio_test_renderer(sink.clone(), 2);
    for _ in 0..200 {
        let pcm = (0..480)
            .flat_map(|_| {
                phase += std::f32::consts::TAU * 1000.0 / 48_000.0;
                let sample = (phase.sin() * 8000.0) as i16;
                [sample, ((phase * 2.0).sin() * 8000.0) as i16]
            })
            .collect::<Vec<_>>();
        source
            .capture_frame(&AudioFrame {
                data: pcm.into(),
                sample_rate: 48_000,
                num_channels: 2,
                samples_per_channel: 480,
            })
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        let mut pcm = [0.0_f32; 960];
        render(&mut pcm);
        let mut spectra = [[(0.0_f64, 0.0_f64); 2]; 2];
        for (n, frame) in pcm.chunks_exact(2).enumerate() {
            let mono = f64::from((frame[0] + frame[1]) * 0.5);
            energy += mono * mono;
            samples += 1;
            for channel in 0..2 {
                for (frequency, hz) in [1000.0_f64, 2000.0].into_iter().enumerate() {
                    let phase = n as f64 * std::f64::consts::TAU * hz / 48_000.0;
                    spectra[channel][frequency].0 += f64::from(frame[channel]) * phase.cos();
                    spectra[channel][frequency].1 += f64::from(frame[channel]) * phase.sin();
                }
            }
        }
        for channel in 0..2 {
            for frequency in 0..2 {
                let (real, imag) = spectra[channel][frequency];
                stereo_power[channel][frequency] += real * real + imag * imag;
            }
        }
        if let Some(mut queued) = other_sink.try_queues() {
            assert_eq!(
                queued.next_sample(),
                0.0,
                "App audio leaked into other room"
            );
        }
    }
    assert!(
        energy > 1.0,
        "Actual Opus appaudio did not reach boundedvoiceoutputmixer"
    );
    for channel in 0..2 {
        assert!(
            stereo_power[channel][channel] > 100.0
                && stereo_power[channel][1 - channel] < stereo_power[channel][channel] * 0.01,
            "Actual SDK stereo collapsed/swapped channels: {stereo_power:?}"
        );
    }
    println!("Owned actual-SDK/native-render stereo spectral power {stereo_power:?}");
    let publisher_id = room.local_participant().identity().to_string();
    let mut volume_energy = [0.0_f64; 2];
    for (index, volume) in [0.0, 1.0].into_iter().enumerate() {
        sink.set_user_volume(&publisher_id, volume);
        for block in 0..40 {
            let pcm = (0..480)
                .flat_map(|_| {
                    phase += std::f32::consts::TAU * 1000.0 / 48_000.0;
                    [
                        (phase.sin() * 8000.0) as i16,
                        ((phase * 2.0).sin() * 8000.0) as i16,
                    ]
                })
                .collect::<Vec<_>>();
            source
                .capture_frame(&AudioFrame {
                    data: pcm.into(),
                    sample_rate: 48_000,
                    num_channels: 2,
                    samples_per_channel: 480,
                })
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(10)).await;
            let mut pcm = [0.0_f32; 960];
            render(&mut pcm);
            if block >= 10 {
                volume_energy[index] += pcm
                    .iter()
                    .map(|sample| f64::from(*sample).powi(2))
                    .sum::<f64>();
            }
        }
    }
    assert_eq!(
        volume_energy[0], 0.0,
        "Muted participant's actual SDK application audio remained audible"
    );
    assert!(
        volume_energy[1] > 1.0,
        "Restoring participant volume did not restore actual SDK audio"
    );
    println!("Owned actual-SDK participant volume muted/restored energy: {volume_energy:?}");
    assert!(
        viewer.frames().is_empty(),
        "Hiddenvideo gate unexpectedpreview"
    );
    let mut bytes = 0;
    for stat in track.get_stats().await.unwrap() {
        if let RtcStats::OutboundRtp(stat) = stat {
            bytes += stat.sent.bytes_sent;
        }
    }
    assert!(bytes > 0);
    room.local_participant()
        .unpublish_track(&publication.sid())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    {
        let mut queued = sink.try_queues().unwrap();
        for _ in 0..960 * 8 {
            assert_eq!(
                queued.next_sample(),
                0.0,
                "Unpublishedappaudio left buffered samples"
            );
        }
    }
    room.close().await.unwrap();
    let evidence = json!({"fixture":"generatedstereo48k-actualOpusSFU-boundedPlaybackSink","generated_frames":280,"frame_samples_per_channel":480,"decoded_sample_observations":samples,"decoded_rms":(energy/samples as f64).sqrt(),"stereo_spectral_power":stereo_power,"minimum_channel_separation_db":20,"participant_volume_muted_restored_energy":volume_energy,"rtp_payload_bytes_sent":bytes,"video_hidden_audio_continued":true,"other_room_silent":true,"unpublish_cleared_buffer":true,"physical_capture_or_playback_opened":false,"e2ee":"LiveKitGCM-per-verified-participant","limitations":"Syntheticappaudio transportandmixqueueonly; actualselectedprocesssoundanddeviceplaybackacceptancepending; notAVsyncproof"});
    std::fs::write(
        std::path::Path::new(&file)
            .parent()
            .unwrap()
            .join("application-audio-evidence.json"),
        serde_json::to_vec_pretty(&evidence).unwrap(),
    )
    .unwrap();
    for session in [&mut viewer, &mut other] {
        session.cancel();
        let handle = session.owner.take().unwrap();
        tokio::task::spawn_blocking(move || handle.join().unwrap())
            .await
            .unwrap();
    }
}

#[cfg(target_os = "windows")]
async fn concurrent_deadline<T>(
    limit: Duration,
    label: &str,
    future: impl std::future::Future<Output = T>,
) -> Result<T, String> {
    tokio::time::timeout(limit, future)
        .await
        .map_err(|_| format!("Owned concurrent fixture deadline: {label}"))
}

#[cfg(target_os = "windows")]
fn concurrent_fixture_source() -> CaptureSource {
    CaptureSource {
        id: "relay-owned-concurrent-fixture".into(),
        name: "Owned generated concurrent video".into(),
        kind: crate::capture::SourceKind::Window,
        width: 640,
        height: 480,
    }
}

#[cfg(target_os = "windows")]
fn concurrent_local_video(
    room: &Room,
    camera: bool,
) -> Result<(LocalTrackPublication, LocalVideoTrack), String> {
    let source = if camera {
        TrackSource::Camera
    } else {
        TrackSource::Screenshare
    };
    let publications = room.local_participant().track_publications();
    let selected: Vec<_> = publications
        .values()
        .filter(|publication| {
            publication.kind() == TrackKind::Video && publication.source() == source
        })
        .collect();
    if selected.len() != 1 {
        return Err("Expected one owned video publication for the selected kind".into());
    }
    let publication = selected[0];
    if publication.encryption_type() != EncryptionType::Gcm {
        return Err("Concurrent video publication lost GCM protection".into());
    }
    let Some(LocalTrack::Video(track)) = publication.track() else {
        return Err("Owned video publication has no native video track".into());
    };
    Ok((publication.clone(), track))
}

#[cfg(target_os = "windows")]
async fn concurrent_room(session: &RtcSession) -> Result<Arc<Room>, String> {
    let (reply, result) = tokio::sync::oneshot::channel();
    concurrent_deadline(
        Duration::from_secs(2),
        "InspectRoom send",
        session.commands.send(Command::InspectRoom(reply)),
    )
    .await?
    .map_err(|_| "Concurrent RTC owner stopped before inspection")?;
    concurrent_deadline(Duration::from_secs(3), "InspectRoom response", result)
        .await?
        .map_err(|_| "Concurrent RTC owner dropped inspection response".into())
}

#[cfg(target_os = "windows")]
async fn concurrent_wait_attached(
    publisher: &RtcSession,
    room: &Room,
    camera: bool,
    expected_sid: Option<&livekit::id::TrackSid>,
) -> Result<LocalTrackPublication, String> {
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(error) = publisher.take_error() {
            return Err(format!("Concurrent publisher failed: {error}"));
        }
        if let Ok((publication, track)) = concurrent_local_video(room, camera) {
            let attached = publisher
                .tracks
                .lock()
                .map_err(|_| "Concurrent track lock poisoned")?
                .get(&camera)
                .is_some_and(|current| current.rtc_track().id() == track.rtc_track().id());
            if attached
                && publisher.is_sharing(camera)
                && expected_sid.is_none_or(|sid| publication.sid() == *sid)
            {
                return Ok(publication);
            }
        }
        if Instant::now() >= until {
            return Err("Concurrent selected video did not attach by its deadline".into());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[cfg(target_os = "windows")]
async fn concurrent_wait_native_decoded(
    viewer: &RtcSession,
    room: &Room,
    keys: &CallKeys,
    user: &str,
    camera: bool,
    sid: &livekit::id::TrackSid,
    stage: &str,
) -> Result<Value, String> {
    let kind = if camera { "camera" } else { "screen" };
    let source = if camera {
        TrackSource::Camera
    } else {
        TrackSource::Screenshare
    };
    let started = Instant::now();
    let until = started + Duration::from_secs(3);
    let key = format!("{user}:{kind}");
    let context = format!("Remote decode readiness stage={stage}, live_kind={kind}, sid={sid}");
    let mut first_timestamp = None;
    crate::media::validate_call_keys(keys).map_err(|error| format!("{context}: {error}"))?;
    loop {
        if Instant::now() >= until {
            return Err(format!("{context}: no two distinct actual decoded timestamps within 3 seconds (first={first_timestamp:?})"));
        }
        if !viewer.is_active() {
            return Err(format!("{context}: protected viewer stopped"));
        }
        let publication = room
            .remote_participants()
            .values()
            .find(|participant| participant.identity().to_string() == user)
            .and_then(|participant| participant.track_publications().get(sid).cloned());
        if let Some(publication) = publication {
            if publication.source() != source
                || publication.kind() != TrackKind::Video
                || !verified_publication(keys, user, &publication)
            {
                return Err(format!(
                    "{context}: current SDK publication did not confirm protected source ownership"
                ));
            }
            if let Some(RemoteTrack::Video(track)) = publication.track() {
                let timestamp = viewer
                    .frames
                    .lock()
                    .map_err(|_| format!("{context}: decoded frame lock poisoned"))?
                    .frames
                    .get(&key)
                    .filter(|cached| {
                        cached.source_sid.as_ref() == Some(sid)
                            && cached.frame.user_id == user
                            && cached.frame.kind == kind
                    })
                    .map(|cached| cached.frame.timestamp_us);
                if let Some(timestamp) = timestamp {
                    if let Some(first) = first_timestamp {
                        if timestamp != first {
                            if Instant::now() >= until {
                                return Err(format!(
                                    "{context}: decoded readiness exceeded its 3-second deadline"
                                ));
                            }
                            return Ok(json!({
                                "stage":stage, "live_kind":kind, "sid":sid.to_string(),
                                "verified_current_remote_native_id":track.rtc_track().id(),
                                "first_decoded_timestamp_us":first, "second_decoded_timestamp_us":timestamp,
                                "waited_milliseconds":started.elapsed().as_millis(),
                                "provenance":"actual accepted NativeVideoStream cache insertion", "encryption":"Gcm"
                            }));
                        }
                    } else {
                        first_timestamp = Some(timestamp);
                    }
                }
            }
        }
        if Instant::now() >= until {
            return Err(format!("{context}: no two distinct actual decoded timestamps within 3 seconds (first={first_timestamp:?})"));
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[cfg(target_os = "windows")]
async fn concurrent_native_audio(
    room: &Room,
    keys: &CallKeys,
    user: &str,
    sid: &livekit::id::TrackSid,
) -> Result<NativeAudioStream, String> {
    let until = Instant::now() + Duration::from_secs(8);
    loop {
        for participant in room.remote_participants().values() {
            if participant.identity().to_string() != user {
                continue;
            }
            for publication in participant.track_publications().values() {
                if publication.sid() != *sid || !verified_publication(keys, user, publication) {
                    continue;
                }
                if let Some(RemoteTrack::Audio(track)) = publication.track() {
                    return Ok(NativeAudioStream::with_options(
                        track.rtc_track(),
                        48_000,
                        2,
                        NativeAudioStreamOptions {
                            queue_size_frames: Some(2),
                        },
                    ));
                }
            }
        }
        if Instant::now() >= until {
            return Err("Verified generated application audio did not subscribe".into());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[cfg(target_os = "windows")]
async fn concurrent_rtp_bytes(track: &LocalVideoTrack) -> Result<u64, String> {
    let stats = concurrent_deadline(
        Duration::from_secs(1),
        "native video RTP stats",
        track.get_stats(),
    )
    .await?
    .map_err(|error| format!("Read concurrent native video stats: {error}"))?;
    Ok(stats
        .into_iter()
        .filter_map(|stat| match stat {
            RtcStats::OutboundRtp(stat) => Some(stat.sent.bytes_sent),
            _ => None,
        })
        .sum())
}

#[cfg(target_os = "windows")]
async fn concurrent_audio_rtp_bytes_with_limit(
    track: &LocalAudioTrack,
    limit: Duration,
) -> Result<u64, String> {
    let stats = concurrent_deadline(limit, "native app-audio RTP stats", track.get_stats())
        .await?
        .map_err(|error| format!("Read concurrent native app-audio stats: {error}"))?;
    Ok(stats
        .into_iter()
        .filter_map(|stat| match stat {
            RtcStats::OutboundRtp(stat) => Some(stat.sent.bytes_sent),
            _ => None,
        })
        .sum())
}

#[cfg(target_os = "windows")]
async fn concurrent_audio_rtp_bytes(track: &LocalAudioTrack) -> Result<u64, String> {
    concurrent_audio_rtp_bytes_with_limit(track, Duration::from_secs(1)).await
}

#[cfg(target_os = "windows")]
async fn concurrent_observe_live(
    publisher: &RtcSession,
    viewer: &RtcSession,
    room: &Room,
    user: &str,
    live_camera: bool,
    audio: &mut NativeAudioStream,
    stage: &str,
) -> Result<Value, String> {
    let kind = if live_camera { "camera" } else { "screen" };
    let mut diagnostics = json!({"stage":stage, "live_kind":kind});
    let result = async {
    let probe_started = Instant::now();
    let (publication, track) = concurrent_local_video(room, live_camera)?;
    diagnostics["video_sid"] = json!(publication.sid().to_string());
    diagnostics["video_native_id"] = json!(track.rtc_track().id());
    diagnostics["video_native_enabled_before"] = json!(track.rtc_track().enabled());
    let audio_publication = room
        .local_participant()
        .track_publications()
        .values()
        .find(|publication| publication.source() == TrackSource::ScreenshareAudio)
        .cloned()
        .ok_or("Concurrent generated audio publication absent")?;
    let Some(LocalTrack::Audio(audio_track)) = audio_publication.track() else {
        return Err("Concurrent generated audio has no native track".into());
    };
    diagnostics["audio_sid"] = json!(audio_publication.sid().to_string());
    diagnostics["audio_native_id"] = json!(audio_track.rtc_track().id());
    let (before, audio_before) = tokio::join!(
        concurrent_rtp_bytes(&track),
        concurrent_audio_rtp_bytes(&audio_track)
    );
    let before = before?;
    let audio_before = audio_before?;
    diagnostics["video_rtp_bytes_before"] = json!(before);
    diagnostics["audio_rtp_bytes_before"] = json!(audio_before);
    let mut last_video = None;
    let mut video_changes = 0_u64;
    let mut next_video = Instant::now();
    let mut previous_audio = Instant::now();
    let mut largest_audio_gap_ms = 0_u128;
    let mut windows = Vec::new();
    let mut total_audio_frames = 0_u64;
    let mut low_marker_frames = 0_u64;
    let mut high_marker_frames = 0_u64;
    // Mark the hold after draining only the configured two-frame decode queue.
    // All six windows must then show fresh PCM, beyond its bounded backlog.
    use futures_util::FutureExt as _;
    let mut drained_audio_frames = 0_u64;
    for _ in 0..2 {
        if matches!(audio.next().now_or_never(), Some(Some(_))) {
            drained_audio_frames += 1;
        } else {
            break;
        }
    }
    let mut window_audio_bytes = audio_before;
    for _ in 0..6 {
        let window_started = Instant::now();
        let window_video_before = video_changes;
        let until = window_started + Duration::from_millis(150);
        let mut window_low_marker_frames = 0_u64;
        let mut window_high_marker_frames = 0_u64;
        let mut frames = 0_u64;
        let mut samples = 0_u64;
        let mut energy = 0_f64;
        while Instant::now() < until {
            if !publisher.is_active() || !viewer.is_active() {
                return Err("Concurrent protected owner stopped during the timing probe".into());
            }
            if Instant::now() >= next_video {
                let timestamp = viewer.frames.lock().map_err(|_| "Concurrent decoded frame lock poisoned")?
                    .frames.get(&format!("{user}:{kind}"))
                    .filter(|cached| cached.source_sid.as_ref() == Some(&publication.sid())
                        && cached.frame.user_id == user && cached.frame.kind == kind)
                    .map(|cached| cached.frame.timestamp_us);
                if let Some(timestamp) = timestamp {
                    diagnostics["video_timestamp_last"] = json!(timestamp);
                    if diagnostics["video_timestamp_first"].is_null() { diagnostics["video_timestamp_first"] = json!(timestamp); }
                    if last_video.is_some_and(|old| old != timestamp) {
                        video_changes += 1;
                        let changed_at = probe_started.elapsed().as_millis();
                        if diagnostics["video_first_change_elapsed_ms"].is_null() { diagnostics["video_first_change_elapsed_ms"] = json!(changed_at); }
                        diagnostics["video_last_change_elapsed_ms"] = json!(changed_at);
                    }
                    last_video = Some(timestamp);
                }
                next_video = Instant::now() + Duration::from_millis(30);
            }
            let remaining = until.saturating_duration_since(Instant::now());
            match tokio::time::timeout(remaining, audio.next()).await {
                Ok(Some(frame)) => {
                    if frame.sample_rate != 48_000 || frame.num_channels != 2 {
                        return Err(
                            "Generated application audio decoded with unexpected format".into()
                        );
                    }
                    let now = Instant::now();
                    largest_audio_gap_ms =
                        largest_audio_gap_ms.max(now.duration_since(previous_audio).as_millis());
                    previous_audio = now;
                    frames += 1;
                    let mut previous = None;
                    let mut crossings = 0_u32;
                    let mut channel_samples = 0_u32;
                    for stereo in frame.data.chunks_exact(2) {
                        let value = stereo[0];
                        if previous.is_some_and(|previous| previous <= 0 && value > 0) {
                            crossings += 1;
                        }
                        previous = Some(value);
                        channel_samples += 1;
                    }
                    let frequency = if channel_samples == 0 {
                        0.0
                    } else {
                        f64::from(crossings) * 48_000.0 / f64::from(channel_samples)
                    };
                    if (250.0..650.0).contains(&frequency) {
                        low_marker_frames += 1;
                        window_low_marker_frames += 1;
                    }
                    if (700.0..1100.0).contains(&frequency) {
                        high_marker_frames += 1;
                        window_high_marker_frames += 1;
                    }
                    for sample in frame.data.iter() {
                        let sample = f64::from(*sample);
                        energy += sample * sample;
                        samples += 1;
                    }
                }
                Ok(None) => {
                    return Err("Generated application audio stream ended during recovery".into())
                }
                Err(_) => break,
            }
        }
        let rms = if samples == 0 {
            0.0
        } else {
            (energy / samples as f64).sqrt()
        };
        let new_audio_bytes =
            concurrent_audio_rtp_bytes_with_limit(&audio_track, Duration::from_millis(250)).await?;
        diagnostics["audio_window_rtp_bytes_before"] = json!(window_audio_bytes);
        diagnostics["audio_window_rtp_bytes_after"] = json!(new_audio_bytes);
        diagnostics["audio_window_rtp_bytes_added"] = json!(new_audio_bytes.saturating_sub(window_audio_bytes));
        diagnostics["video_timestamp_changes"] = json!(video_changes);
        diagnostics["audio_frames_in_completed_windows"] = json!(total_audio_frames);
        diagnostics["current_pcm_window"] = json!({"index":windows.len(), "decoded_frames":frames,"fresh_low_marker_frames":window_low_marker_frames,"fresh_high_marker_frames":window_high_marker_frames,"rms":rms});
        if frames < 5
            || rms < 500.0
            || window_low_marker_frames < 2
            || window_high_marker_frames < 2
            || new_audio_bytes <= window_audio_bytes
        {
            return Err(format!(
                "Application audio freshness failed within a recovery window (frames={frames}, rms={rms:.1}, low={window_low_marker_frames}, high={window_high_marker_frames})"
            ));
        }
        total_audio_frames += frames;
        windows.push(json!({
            "elapsed_milliseconds":window_started.elapsed().as_millis(),
            "video_timestamp_changes":video_changes-window_video_before,
            "decoded_frames":frames, "decoded_samples":samples, "rms":rms,
            "fresh_low_marker_frames":window_low_marker_frames,
            "fresh_high_marker_frames":window_high_marker_frames,
            "native_audio_rtp_bytes_added":new_audio_bytes-window_audio_bytes
        }));
        diagnostics["completed_pcm_windows"] = json!(&windows);
        window_audio_bytes = new_audio_bytes;
    }
    let (after, audio_after) = tokio::join!(
        concurrent_rtp_bytes(&track),
        concurrent_audio_rtp_bytes(&audio_track)
    );
    let after = after?;
    let audio_after = audio_after?;
    diagnostics["video_rtp_bytes_after"] = json!(after);
    diagnostics["video_rtp_bytes_added"] = json!(after.saturating_sub(before));
    diagnostics["audio_rtp_bytes_after"] = json!(audio_after);
    diagnostics["audio_rtp_bytes_added"] = json!(audio_after.saturating_sub(audio_before));
    diagnostics["video_native_enabled_after"] = json!(track.rtc_track().enabled());
    diagnostics["video_timestamp_changes"] = json!(video_changes);
    diagnostics["decoded_audio_frames"] = json!(total_audio_frames);
    diagnostics["observed_milliseconds"] = json!(probe_started.elapsed().as_millis());
    diagnostics["publisher_epoch"] = json!(publisher.epochs[usize::from(live_camera)].load(Ordering::Acquire));
    diagnostics["publisher_source_state"] = json!(publisher.source_states[usize::from(live_camera)].load(Ordering::Acquire));
    if video_changes < 8
        || after <= before
        || total_audio_frames < 24
        || audio_after <= audio_before
        || low_marker_frames < 8
        || high_marker_frames < 8
    {
        return Err(format!("Concurrent video/audio progress too small (video changes={video_changes}, audio frames={total_audio_frames})"));
    }
    if concurrent_local_video(room, live_camera)?.0.sid() != publication.sid() {
        return Err("The unaffected video publication changed during recovery".into());
    }
    Ok(json!({
        "live_kind":kind, "observed_milliseconds":probe_started.elapsed().as_millis(), "decoded_video_timestamp_changes":video_changes,
        "native_video_rtp_bytes_added":after-before, "native_audio_rtp_bytes_added":audio_after-audio_before,
        "decoded_pcm_windows":windows, "new_low_frequency_marker_frames":low_marker_frames,
        "new_high_frequency_marker_frames":high_marker_frames,
        "largest_observed_pcm_gap_ms":largest_audio_gap_ms,
        "bounded_pcm_frames_drained_before_hold":drained_audio_frames,
        "video_first_change_elapsed_ms":diagnostics["video_first_change_elapsed_ms"],
        "video_last_change_elapsed_ms":diagnostics["video_last_change_elapsed_ms"]
    }))
    }.await;
    if result.is_err() {
        // Non-consuming failure diagnostics: do not hide owner errors or change
        // any native timing, progress, identity, protection, or Stop guard.
        diagnostics["publisher_error"] =
            json!(publisher.errors.lock().ok().and_then(|slot| slot.clone()));
        diagnostics["viewer_error"] =
            json!(viewer.errors.lock().ok().and_then(|slot| slot.clone()));
        diagnostics["publisher_audio_active"] = json!(publisher.is_application_audio_active());
        diagnostics["publisher_epoch_at_failure"] =
            json!(publisher.epochs[usize::from(live_camera)].load(Ordering::Acquire));
        diagnostics["local_publications_at_failure"] = json!(room.local_participant().track_publications().values().map(|p| json!({"sid":p.sid().to_string(),"kind":format!("{:?}",p.kind()),"source":format!("{:?}",p.source()),"enabled":p.track().map(|t|t.rtc_track().enabled())})).collect::<Vec<_>>());
    }
    result.map_err(|error: String| format!("{error}; concurrent probe diagnostics={diagnostics}"))
}

#[cfg(target_os = "windows")]
async fn concurrent_stopped_audio_stats(track: &LocalAudioTrack) -> Result<Option<u64>, String> {
    let result = concurrent_deadline(
        Duration::from_secs(1),
        "retired app-audio stats",
        track.get_stats(),
    )
    .await?;
    // The locked SDK may remove the sender/transceiver on unpublish. A retained
    // LocalAudioTrack then has no stats API; report that state without claiming
    // a measured zero byte delta. The exact SID/receiver retirement is checked.
    Ok(result.ok().map(|stats| {
        stats
            .into_iter()
            .filter_map(|stat| match stat {
                RtcStats::OutboundRtp(stat) => Some(stat.sent.bytes_sent),
                _ => None,
            })
            .sum()
    }))
}

#[cfg(target_os = "windows")]
async fn concurrent_camera_after_screen_audio_stop(
    publisher: &RtcSession,
    viewer: &RtcSession,
    room: &Room,
    user: &str,
    saved_audio: &LocalAudioTrack,
) -> Result<Value, String> {
    // Allow only a short, bounded drain beyond the 20 ms native capture queue.
    tokio::time::sleep(Duration::from_millis(120)).await;
    let started = Instant::now();
    let (publication, camera) = concurrent_local_video(room, true)?;
    let (camera_before, audio_before) = tokio::join!(
        concurrent_rtp_bytes(&camera),
        concurrent_stopped_audio_stats(saved_audio),
    );
    let camera_before = camera_before?;
    let audio_before = audio_before?;
    let mut previous_timestamp = None;
    let mut video_changes = 0_u64;
    let until = Instant::now() + Duration::from_millis(900);
    while Instant::now() < until {
        if !publisher.is_active()
            || !publisher.is_sharing(true)
            || !viewer.is_active()
            || publisher.is_application_audio_active()
        {
            return Err("Screen Stop interrupted the camera or restarted application audio".into());
        }
        if let Some(frame) = viewer
            .frames()
            .into_iter()
            .find(|frame| frame.user_id == user && frame.kind == "camera")
        {
            if previous_timestamp.is_some_and(|previous| previous != frame.timestamp_us) {
                video_changes += 1;
            }
            previous_timestamp = Some(frame.timestamp_us);
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    let (camera_after, audio_after) = tokio::join!(
        concurrent_rtp_bytes(&camera),
        concurrent_stopped_audio_stats(saved_audio),
    );
    let camera_after = camera_after?;
    let audio_after = audio_after?;
    // Removed/reset sender counters are not comparable. Only two available,
    // nondecreasing samples establish a measured byte delta after Stop.
    let measured_audio_delta = match (audio_before, audio_after) {
        (Some(before), Some(after)) if after >= before => Some(after - before),
        _ => None,
    };
    let stable_audio_stats = measured_audio_delta.map(|delta| delta == 0);
    if video_changes < 8
        || camera_after <= camera_before
        || measured_audio_delta.is_some_and(|delta| delta > 0)
    {
        return Err(
            "Camera stalled or measured app-audio RTP still advanced after screen Stop".into(),
        );
    }
    if concurrent_local_video(room, true)?.0.sid() != publication.sid() {
        return Err("Camera publication changed while screen audio was stopping".into());
    }
    Ok(json!({
        "stage":"camera_after_pending_screen_stop_and_audio_drain",
        "elapsed_milliseconds":started.elapsed().as_millis(),
        "camera_timestamp_changes":video_changes,
        "native_camera_rtp_bytes_added":camera_after-camera_before,
        "saved_audio_rtp_bytes_before":audio_before, "saved_audio_rtp_bytes_after":audio_after,
        "audio_rtp_stable_after_bounded_drain_when_available":stable_audio_stats,
        "audio_stats_available_before":audio_before.is_some(),
        "audio_stats_available_after":audio_after.is_some(),
        "stopped_audio_stats_scope":"SDK may remove or reset sender stats on unpublish; null stability is unavailable/uncomparable, not a measured zero delta",
        "measured_audio_rtp_bytes_added_after_stop":measured_audio_delta,
        "drain_milliseconds":120
    }))
}

#[cfg(target_os = "windows")]
async fn concurrent_replay_late(
    publisher: &RtcSession,
    publication: LocalTrackPublication,
    track: LocalTrack,
    republished: bool,
) -> Result<(), String> {
    let (reply, result) = tokio::sync::oneshot::channel();
    concurrent_deadline(
        Duration::from_secs(2),
        "late event send",
        publisher
            .commands
            .send(Command::TestReplayLocalPublication {
                publication,
                track,
                republished,
                reply,
            }),
    )
    .await?
    .map_err(|_| "Concurrent owner stopped before late event")?;
    concurrent_deadline(Duration::from_secs(3), "late event response", result)
        .await?
        .map_err(|_| "Concurrent owner dropped late event response")?
}

#[cfg(target_os = "windows")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "Explicit owned loopback SFU pending recovery fixture; generated video and PCM, no hardware encoder or devices"]
async fn actual_pending_recovery_keeps_simultaneous_video_and_generated_application_audio_live() {
    use futures_util::FutureExt as _;
    let _serial = SFU_TEST_LOCK.lock().await;
    let file = std::env::var("RELAY_RTC_TEST_TICKETS").unwrap();
    let mut publisher = None;
    let mut viewer = None;
    let mut other = None;
    let pcm_stop = Arc::new(AtomicBool::new(false));
    let generated_pcm = Arc::new(AtomicU64::new(0));
    let mut pcm_owner: Option<tokio::task::JoinHandle<()>> = None;
    let body = std::panic::AssertUnwindSafe(async {
        let mut fixture: Value =
            serde_json::from_slice(&std::fs::read(&file).map_err(|error| error.to_string())?)
                .map_err(|error| error.to_string())?;
        let base = fixture["base"]
            .as_str()
            .ok_or("Owned fixture is missing its local base")?
            .to_owned();
        let base_url = url::Url::parse(&base).map_err(|error| error.to_string())?;
        if !base_url
            .host_str()
            .ok_or("Owned fixture base has no host")?
            .parse::<std::net::IpAddr>()
            .map_err(|error| error.to_string())?
            .is_loopback()
        {
            return Err("Concurrent fixture must use a loopback Relay server".into());
        }
        for name in ["alice", "bob", "eve"] {
            let channel = fixture[if name == "eve" { "other_id" } else { "room_id" }].clone();
            let token = fixture["users"][name]["session"]["token"]
                .as_str()
                .ok_or("Owned fixture session missing")?;
            concurrent_deadline(
                Duration::from_secs(12),
                "owned voice join",
                api(
                    &base,
                    token,
                    "/api/voice/join",
                    json!({"channel_id":channel}),
                ),
            )
            .await?;
            fixture["users"][name]["ticket"] = concurrent_deadline(
                Duration::from_secs(12),
                "owned RTC ticket",
                api(
                    &base,
                    token,
                    "/api/rtc/token",
                    json!({"channel_id":channel}),
                ),
            )
            .await?;
        }
        let ticket = |name: &str| -> Result<RtcTicket, String> {
            let ticket: RtcTicket =
                serde_json::from_value(fixture["users"][name]["ticket"].clone())
                    .map_err(|error| error.to_string())?;
            let parsed = url::Url::parse(&ticket.url).map_err(|error| error.to_string())?;
            if !parsed
                .host_str()
                .ok_or("Owned RTC ticket has no host")?
                .parse::<std::net::IpAddr>()
                .map_err(|error| error.to_string())?
                .is_loopback()
            {
                return Err("Concurrent fixture RTC endpoint must be loopback".into());
            }
            Ok(ticket)
        };
        let alice_keys = fixture_call_keys(&fixture, "alice");
        let bob_keys = fixture_call_keys(&fixture, "bob");
        publisher = Some(RtcSession::connect(
            ticket("alice")?,
            Context::default(),
            alice_keys.clone(),
        )?);
        viewer = Some(RtcSession::connect(
            ticket("bob")?,
            Context::default(),
            bob_keys.clone(),
        )?);
        other = Some(RtcSession::connect(
            ticket("eve")?,
            Context::default(),
            fixture_call_keys(&fixture, "eve"),
        )?);
        let publisher = publisher.as_ref().ok_or("Publisher missing")?;
        let viewer = viewer.as_ref().ok_or("Viewer missing")?;
        let other = other.as_ref().ok_or("Other-room owner missing")?;
        for session in [publisher, viewer, other] {
            wait_active(session).await;
        }
        publisher.set_video_visible(false);
        viewer.set_video_visible(true);
        let options = CaptureOptions {
            quality: crate::capture::CaptureQuality::P480,
            fps: 30,
            codec: VideoCodecPreference::Auto,
            adaptive_video: false,
            application_audio: false,
            ..Default::default()
        };
        publisher.start_screen(concurrent_fixture_source(), options)?;
        publisher.start_camera(concurrent_fixture_source(), options)?;
        let room = concurrent_room(publisher).await?;
        concurrent_wait_attached(publisher, &room, false, None).await?;
        concurrent_wait_attached(publisher, &room, true, None).await?;
        let original_scope = alice_keys.scope.clone();
        let original_key_epoch = alice_keys.epoch;
        let (sender, receiver) = mpsc::sync_channel(2);
        let (reply, result) = tokio::sync::oneshot::channel();
        concurrent_deadline(
            Duration::from_secs(2),
            "application audio attach send",
            publisher
                .commands
                .send(Command::TestApplicationAudio { receiver, reply }),
        )
        .await?
        .map_err(|_| "Concurrent publisher stopped before audio attach")?;
        let audio_sid = concurrent_deadline(
            Duration::from_secs(10),
            "protected application audio attach",
            result,
        )
        .await?
        .map_err(|_| "Concurrent owner dropped audio attach response")??;
        let stop = pcm_stop.clone();
        let generated = generated_pcm.clone();
        pcm_owner = Some(tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(20));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut phase = 0_f32;
            let mut timestamp_us = 1_000_000_i64;
            let mut packet_sequence = 0_u64;
            while !stop.load(Ordering::Acquire) {
                tick.tick().await;
                if stop.load(Ordering::Acquire) {
                    break;
                }
                // Markers alternate every 40 ms, so each measured window
                // requires fresh frequency changes beyond the decode backlog.
                let frequency = if (packet_sequence / 2).is_multiple_of(2) {
                    440.0
                } else {
                    880.0
                };
                packet_sequence += 1;
                let samples = (0..960)
                    .flat_map(|_| {
                        phase += std::f32::consts::TAU * frequency / 48_000.0;
                        let sample = (phase.sin() * 9000.0) as i16;
                        [sample, sample]
                    })
                    .collect();
                let frame = ProcessAudioFrame {
                    samples,
                    timestamp_us,
                };
                timestamp_us += 20_000;
                match sender.try_send(frame) {
                    Ok(()) => {
                        generated.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(mpsc::TrySendError::Full(_)) => {}
                    Err(mpsc::TrySendError::Disconnected(_)) => break,
                }
            }
        }));
        let viewer_room = concurrent_room(viewer).await?;
        let user = alice_keys.local_user_id.as_str();
        let mut audio = concurrent_native_audio(&viewer_room, &bob_keys, user, &audio_sid).await?;
        // Warm up the actual decoder before measuring separate recovery windows.
        let warm_until = Instant::now() + Duration::from_millis(400);
        while Instant::now() < warm_until {
            concurrent_deadline(Duration::from_secs(1), "PCM decoder warmup", audio.next())
                .await?
                .ok_or("Generated PCM decoder ended in warmup")?;
        }
        let mut observations = Vec::new();
        let mut decoded_readiness = Vec::new();
        for camera in [false, true] {
            let publication = concurrent_local_video(&room, camera)?.0;
            decoded_readiness.push(
                concurrent_wait_native_decoded(
                    viewer,
                    &viewer_room,
                    &bob_keys,
                    user,
                    camera,
                    &publication.sid(),
                    if camera {
                        "initial_camera"
                    } else {
                        "initial_screen"
                    },
                )
                .await?,
            );
        }
        let mut retained_late = Vec::new();
        for camera in [true, false] {
            let (old, old_track) = concurrent_local_video(&room, camera)?;
            retained_late.push((old.clone(), LocalTrack::Video(old_track)));
            let (reached, reached_result) = tokio::sync::oneshot::channel();
            let (resume, resume_result) = tokio::sync::oneshot::channel();
            let (published, published_result) = tokio::sync::oneshot::channel();
            let (attach, attach_result) = tokio::sync::oneshot::channel();
            let (reply, result) = tokio::sync::oneshot::channel();
            concurrent_deadline(
                Duration::from_secs(2),
                "recovery request send",
                publisher.commands.send(Command::TestRecovery {
                    camera,
                    before_retire: Some(RecoveryGate {
                        reached,
                        resume: resume_result,
                    }),
                    after_publish: Some(RecoveryPause {
                        reached: published,
                        resume: attach_result,
                    }),
                    reply,
                }),
            )
            .await?
            .map_err(|_| "Concurrent owner stopped before recovery")?;
            concurrent_deadline(Duration::from_secs(3), "recovery acknowledgment", result)
                .await?
                .map_err(|_| "Concurrent owner dropped recovery acknowledgment")??;
            concurrent_deadline(
                Duration::from_secs(3),
                "before-retirement gate",
                reached_result,
            )
            .await?
            .map_err(|_| "Recovery did not reach the retirement gate")?;
            concurrent_room(publisher).await?;
            let mut observation = concurrent_observe_live(
                publisher,
                viewer,
                &room,
                user,
                !camera,
                &mut audio,
                "before_retire",
            )
            .await?;
            observation["stage"] = json!("before_retire");
            observation["recovering_kind"] = json!(if camera { "camera" } else { "screen" });
            observations.push(observation);
            resume
                .send(())
                .map_err(|_| "Recovery retirement gate expired")?;
            let replacement = concurrent_deadline(
                Duration::from_secs(12),
                "accepted GCM replacement",
                published_result,
            )
            .await?
            .map_err(|_| "Recovery failed before the GCM publication gate")?;
            let pending = room
                .local_participant()
                .track_publications()
                .get(&replacement)
                .cloned()
                .ok_or("Accepted replacement SID missing during protected handoff")?;
            if pending.encryption_type() != EncryptionType::Gcm || pending.sid() == old.sid() {
                return Err("Pending recovery did not produce a new protected publication".into());
            }
            concurrent_room(publisher).await?;
            let mut observation = concurrent_observe_live(
                publisher,
                viewer,
                &room,
                user,
                !camera,
                &mut audio,
                "after_publish_before_attach",
            )
            .await?;
            observation["stage"] = json!("after_publish_before_attach");
            observation["recovering_kind"] = json!(if camera { "camera" } else { "screen" });
            observations.push(observation);
            attach
                .send(())
                .map_err(|_| "Recovery attachment gate expired")?;
            concurrent_wait_attached(publisher, &room, camera, Some(&replacement)).await?;
            decoded_readiness.push(
                concurrent_wait_native_decoded(
                    viewer,
                    &viewer_room,
                    &bob_keys,
                    user,
                    camera,
                    &replacement,
                    "recovered_source_before_next_hold",
                )
                .await?,
            );
            let publications = room.local_participant().track_publications();
            if publications.contains_key(&old.sid()) || !publications.contains_key(&audio_sid) {
                return Err(
                    "Recovery retained the old SID or interrupted the app-audio publication".into(),
                );
            }
            if !publisher.is_application_audio_active() {
                return Err("Real app-audio pump became inactive during recovery".into());
            }
        }
        let (previous_camera, previous_track) = concurrent_local_video(&room, true)?;
        retained_late.push((previous_camera.clone(), LocalTrack::Video(previous_track)));
        let (published, published_result) = tokio::sync::oneshot::channel();
        let (resume, resume_result) = tokio::sync::oneshot::channel();
        let (reply, result) = tokio::sync::oneshot::channel();
        concurrent_deadline(
            Duration::from_secs(2),
            "cancelled recovery request send",
            publisher.commands.send(Command::TestRecovery {
                camera: true,
                before_retire: None,
                after_publish: Some(RecoveryPause {
                    reached: published,
                    resume: resume_result,
                }),
                reply,
            }),
        )
        .await?
        .map_err(|_| "Concurrent owner stopped before cancellation probe")?;
        concurrent_deadline(
            Duration::from_secs(3),
            "cancelled recovery acknowledgment",
            result,
        )
        .await?
        .map_err(|_| "Concurrent owner dropped cancellation acknowledgment")??;
        let cancelled_sid = concurrent_deadline(
            Duration::from_secs(12),
            "cancelled replacement gate",
            published_result,
        )
        .await?
        .map_err(|_| "Cancellation probe failed before GCM publication")?;
        let cancelled_publication = room
            .local_participant()
            .track_publications()
            .get(&cancelled_sid)
            .cloned()
            .ok_or("Cancelled protected publication was absent before Stop")?;
        let cancelled_track = cancelled_publication
            .track()
            .ok_or("Pending protected replacement has no retained native track")?;
        retained_late.push((cancelled_publication, cancelled_track));
        publisher.stop_camera();
        publisher.start_camera(
            concurrent_fixture_source(),
            CaptureOptions { fps: 15, ..options },
        )?;
        publisher.start_camera(concurrent_fixture_source(), options)?;
        let newest_generation = publisher.epochs[1].load(Ordering::Acquire);
        concurrent_room(publisher).await?;
        let mut observation = concurrent_observe_live(
            publisher,
            viewer,
            &room,
            user,
            false,
            &mut audio,
            "stopped_replacement_with_two_queued_starts",
        )
        .await?;
        observation["stage"] = json!("stopped_replacement_with_two_queued_starts");
        observations.push(observation);
        // Cancellation may complete the gate early; a closed resume receiver is safe.
        let _ = resume.send(());
        let latest = concurrent_wait_attached(publisher, &room, true, None).await?;
        if latest.sid() == cancelled_sid
            || latest.sid() == previous_camera.sid()
            || source_phase(&publisher.source_states[1], newest_generation) != 3
            || publisher.epochs[1].load(Ordering::Acquire) != newest_generation
        {
            return Err(
                "Cancelled recovery or superseded Start attached instead of the newest source"
                    .into(),
            );
        }
        decoded_readiness.push(
            concurrent_wait_native_decoded(
                viewer,
                &viewer_room,
                &bob_keys,
                user,
                true,
                &latest.sid(),
                "newest_queued_camera_before_injected_late_events",
            )
            .await?,
        );
        let publications = room.local_participant().track_publications();
        if publications.contains_key(&cancelled_sid)
            || publications.contains_key(&previous_camera.sid())
        {
            return Err("Cancelled recovery retained an obsolete native SID".into());
        }
        for (publication, track) in &retained_late {
            concurrent_replay_late(publisher, publication.clone(), track.clone(), false).await?;
            concurrent_replay_late(publisher, publication.clone(), track.clone(), true).await?;
        }
        if concurrent_local_video(&room, true)?.0.sid() != latest.sid() {
            return Err("Injected stale publication event replaced the newest source".into());
        }
        let mut observation = concurrent_observe_live(
            publisher,
            viewer,
            &room,
            user,
            true,
            &mut audio,
            "newest_camera_after_injected_late_events",
        )
        .await?;
        observation["stage"] = json!("newest_camera_after_injected_late_events");
        observations.push(observation);
        let latest_track = latest
            .track()
            .ok_or("Newest protected camera lost its retained native track")?;
        publisher.stop_camera();
        let until = Instant::now() + Duration::from_secs(8);
        loop {
            let absent = room
                .local_participant()
                .track_publications()
                .values()
                .all(|publication| publication.source() != TrackSource::Camera);
            let no_pixels = viewer
                .frames()
                .iter()
                .all(|frame| frame.user_id != user || frame.kind != "camera");
            if absent && no_pixels {
                break;
            }
            if Instant::now() >= until {
                return Err(
                    "Final Stop did not remove the camera publication and cached pixels".into(),
                );
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        concurrent_replay_late(publisher, latest.clone(), latest_track.clone(), false).await?;
        concurrent_replay_late(publisher, latest, latest_track, true).await?;
        let mut observation = concurrent_observe_live(
            publisher,
            viewer,
            &room,
            user,
            false,
            &mut audio,
            "screen_and_pcm_after_stop_and_injected_late_events",
        )
        .await?;
        observation["stage"] = json!("screen_and_pcm_after_stop_and_injected_late_events");
        observations.push(observation);
        if publisher.is_sharing(true)
            || room
                .local_participant()
                .track_publications()
                .values()
                .any(|publication| publication.source() == TrackSource::Camera)
        {
            return Err("Injected late event resurrected the stopped camera".into());
        }

        // Stop a recovering screen publication itself: its application-audio
        // capture/queue must retire while the simultaneously selected camera lives.
        publisher.start_camera(concurrent_fixture_source(), options)?;
        let new_camera = concurrent_wait_attached(publisher, &room, true, None).await?;
        decoded_readiness.push(
            concurrent_wait_native_decoded(
                viewer,
                &viewer_room,
                &bob_keys,
                user,
                true,
                &new_camera.sid(),
                "new_camera_before_pending_screen_stop",
            )
            .await?,
        );
        let audio_publication = room
            .local_participant()
            .track_publications()
            .get(&audio_sid)
            .cloned()
            .ok_or("Generated application audio disappeared after video recovery")?;
        let Some(LocalTrack::Audio(audio_track)) = audio_publication.track() else {
            return Err("Generated audio lost its native track".into());
        };
        let stats = concurrent_deadline(
            Duration::from_secs(1),
            "native app-audio RTP stats",
            audio_track.get_stats(),
        )
        .await?
        .map_err(|error| error.to_string())?;
        let audio_bytes: u64 = stats
            .into_iter()
            .filter_map(|stat| match stat {
                RtcStats::OutboundRtp(stat) => Some(stat.sent.bytes_sent),
                _ => None,
            })
            .sum();
        if audio_bytes == 0 || audio_publication.encryption_type() != EncryptionType::Gcm {
            return Err("Actual protected application audio produced no native RTP bytes".into());
        }

        let old_screen = concurrent_local_video(&room, false)?.0;
        let (published, published_result) = tokio::sync::oneshot::channel();
        let (resume, resume_result) = tokio::sync::oneshot::channel();
        let (reply, result) = tokio::sync::oneshot::channel();
        concurrent_deadline(
            Duration::from_secs(2),
            "pending-screen Stop request send",
            publisher.commands.send(Command::TestRecovery {
                camera: false,
                before_retire: None,
                after_publish: Some(RecoveryPause {
                    reached: published,
                    resume: resume_result,
                }),
                reply,
            }),
        )
        .await?
        .map_err(|_| "Owner stopped before pending-screen Stop probe")?;
        concurrent_deadline(
            Duration::from_secs(3),
            "pending-screen Stop acknowledgment",
            result,
        )
        .await?
        .map_err(|_| "Owner dropped pending-screen Stop acknowledgment")??;
        let stopped_screen_sid = concurrent_deadline(
            Duration::from_secs(12),
            "pending-screen protected replacement",
            published_result,
        )
        .await?
        .map_err(|_| "Screen Stop probe did not reach protected replacement gate")?;
        let protected_screen = room
            .local_participant()
            .track_publications()
            .get(&stopped_screen_sid)
            .cloned()
            .ok_or("Pending screen replacement missing before Stop")?;
        if protected_screen.encryption_type() != EncryptionType::Gcm
            || protected_screen.source() != TrackSource::Screenshare
        {
            return Err(
                "Pending screen Stop probe did not own a protected screen publication".into(),
            );
        }
        concurrent_room(publisher).await?;
        let mut observation = concurrent_observe_live(
            publisher,
            viewer,
            &room,
            user,
            true,
            &mut audio,
            "camera_and_own_screen_pcm_before_pending_screen_stop",
        )
        .await?;
        observation["stage"] = json!("camera_and_own_screen_pcm_before_pending_screen_stop");
        observations.push(observation);
        publisher.stop_screen();
        if publisher.is_application_audio_active()
            || publisher.is_sharing(false)
            || publisher.source_states[0].load(Ordering::Acquire) != 0
        {
            return Err(
                "Screen Stop did not immediately clear its source phase and application-audio flag"
                    .into(),
            );
        }
        let _ = resume.send(());
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            let publications = room.local_participant().track_publications();
            let retired = !publications.contains_key(&old_screen.sid())
                && !publications.contains_key(&stopped_screen_sid)
                && !publications.contains_key(&audio_sid)
                && publications.values().all(|publication| {
                    !matches!(
                        publication.source(),
                        TrackSource::Screenshare | TrackSource::ScreenshareAudio
                    )
                });
            let no_screen_pixels = viewer
                .frames()
                .iter()
                .all(|frame| frame.user_id != user || frame.kind != "screen");
            if retired && no_screen_pixels {
                break;
            }
            if Instant::now() >= until {
                return Err(
                    "Pending screen Stop retained screen/audio SID or cached screen pixels".into(),
                );
            }
            if !publisher.is_active() || !publisher.is_sharing(true) {
                return Err("Pending screen Stop terminated the unaffected camera".into());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // The test producer must stop naturally because the actual app-audio
        // receiver was dropped, before the fixture's explicit cleanup stop flag.
        let owner = pcm_owner
            .as_mut()
            .ok_or("Generated PCM owner disappeared before screen Stop")?;
        concurrent_deadline(
            Duration::from_secs(2),
            "app-audio receiver disconnection",
            owner,
        )
        .await?
        .map_err(|error| format!("Generated PCM owner failed after screen Stop: {error}"))?;
        pcm_owner.take();
        if !publisher
            .audio_tracks
            .lock()
            .map_err(|_| "Stopped app-audio track map poisoned")?
            .is_empty()
        {
            return Err(
                "Pending screen Stop retained a tracked native application-audio publication"
                    .into(),
            );
        }
        if pcm_stop.load(Ordering::Acquire) {
            return Err(
                "Generated PCM required explicit fixture cancellation instead of receiver cleanup"
                    .into(),
            );
        }
        drop(audio);
        observations.push(
            concurrent_camera_after_screen_audio_stop(publisher, viewer, &room, user, &audio_track)
                .await?,
        );

        if alice_keys.scope != original_scope
            || alice_keys.epoch != original_key_epoch
            || !publisher.frames().is_empty()
            || !other.frames().is_empty()
        {
            return Err(
                "Concurrent recovery changed protected scope or exposed hidden/other-room pixels"
                    .into(),
            );
        }
        let other_room = concurrent_room(other).await?;
        if other_room
            .remote_participants()
            .values()
            .any(|participant| participant.identity().to_string() == user)
        {
            return Err("Concurrent publication appeared in the other owned room".into());
        }
        Ok::<Value, String>(json!({
            "fixture":"production RTC owner with bounded pending video recovery and generated process-audio frames",
            "encryption":"Gcm per verified participant", "observations":observations, "decoded_readiness_preconditions":decoded_readiness,
            "generated_pcm_packets":generated_pcm.load(Ordering::Relaxed), "native_application_audio_rtp_bytes":audio_bytes,
            "application_audio_sid_preserved_during_recovery":true, "camera_and_screen_recovered":true,
            "pending_screen_stop_retired_own_audio_and_replacement":true,
            "generated_pcm_owner_finished_on_receiver_disconnect":true,
            "unaffected_camera_continued_after_pending_screen_stop":true,
            "stop_and_two_queued_starts_newest_generation":newest_generation,
            "cancelled_replacement_retired":true, "injected_late_publication_and_republication_events_harmless":true,
            "late_event_scope":"Retained actual SDK publication/track objects with already-absent old SIDs; stale rejection only, no genuine SDK republish/new SID or AddTrack-failure proof",
            "camera_stop_late_events_did_not_resurrect_camera":true, "scope_and_key_epoch_preserved":true,
            "other_room_empty":true, "hidden_publisher_preview_empty":true,
            "physical_capture_or_playback_opened":false, "hardware_encoder_exercised":false,
            "limitations":"Failure and late events are injected. Actual GCM SFU VP8/Opus transport and production video/audio pumps are exercised. No real encoder failure, selected-process capture, device playback, WAN latency, or AV-sync acceptance."
        }))
    });
    let outcome = tokio::time::timeout(Duration::from_secs(100), body.catch_unwind()).await;
    // Stop every owner first, then attempt each cleanup independently, including
    // when the main body returned an error, panicked, or exceeded its deadline.
    pcm_stop.store(true, Ordering::Release);
    for session in [&publisher, &viewer, &other].into_iter().flatten() {
        session.cancel();
    }
    let mut cleanup_errors = Vec::new();
    if let Some(mut owner) = pcm_owner.take() {
        match tokio::time::timeout(Duration::from_secs(2), &mut owner).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => cleanup_errors.push(format!("Generated PCM owner failed: {error}")),
            Err(_) => {
                owner.abort();
                let _ = tokio::time::timeout(Duration::from_secs(1), owner).await;
                cleanup_errors.push("Generated PCM owner required abort".into());
            }
        }
    }
    for session in [&mut publisher, &mut viewer, &mut other]
        .into_iter()
        .flatten()
    {
        if std::panic::AssertUnwindSafe(close_recovery_controller(session))
            .catch_unwind()
            .await
            .is_err()
        {
            cleanup_errors.push("A protected RTC owner did not finish normal cleanup".into());
        }
    }
    assert!(
        cleanup_errors.is_empty(),
        "Owned concurrent cleanup failed: {}",
        cleanup_errors.join("; ")
    );
    let evidence = match outcome {
        Ok(Ok(Ok(evidence))) => evidence,
        Ok(Ok(Err(error))) => panic!("Owned concurrent recovery fixture failed: {error}"),
        Ok(Err(panic)) => std::panic::resume_unwind(panic),
        Err(_) => panic!("Owned concurrent recovery fixture exceeded its overall deadline"),
    };
    std::fs::write(
        std::path::Path::new(&file)
            .parent()
            .unwrap()
            .join("concurrent-recovery-evidence.json"),
        serde_json::to_vec_pretty(&evidence).unwrap(),
    )
    .unwrap();
}

#[cfg(target_os = "windows")]
async fn reconnect_audio_mixer_progress(sink: &ApplicationAudioSink) -> Result<Value, String> {
    let mut windows = Vec::new();
    // Use the production render callback without opening an output device.
    // A producer may briefly own the queue; the real callback stays nonblocking.
    // Each window exceeds the normal40ms bound and still requires fresh sound.
    let mut render = crate::media::application_audio_test_renderer(sink.clone(), 1);
    for _ in 0..3 {
        let mut nonzero = 0_u64;
        let mut squared = 0_f64;
        for _ in 0..15 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let mut pcm = [0.0; 480];
            render(&mut pcm);
            for sample in pcm {
                let value = f64::from(sample);
                squared += value * value;
                nonzero += u64::from(value.abs() > 0.01);
            }
        }
        let rms = (squared / 7200.0).sqrt();
        if nonzero < 2400 || rms < 0.04 {
            return Err(format!(
                "Application-audio mixer stalled: nonzero={nonzero}, rms={rms}"
            ));
        }
        windows.push(json!({"sampled_mono_samples":7200,"nonzero_samples":nonzero,"rms":rms}));
    }
    Ok(json!(windows))
}

#[cfg(target_os = "windows")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "Owned loopback SFU application audio across SDK signal/full reconnect, generated PCM only"]
async fn actual_application_audio_survives_signal_and_full_reconnect_then_stops() {
    use futures_util::FutureExt as _;
    let _serial = SFU_TEST_LOCK.lock().await;
    let file = std::env::var("RELAY_RTC_TEST_TICKETS").unwrap();
    let mut publisher = None;
    let mut viewer = None;
    let mut other = None;
    let pcm_stop = Arc::new(AtomicBool::new(false));
    let generated_pcm = Arc::new(AtomicU64::new(0));
    let mut pcm_owner: Option<tokio::task::JoinHandle<()>> = None;
    let body = std::panic::AssertUnwindSafe(async {
        let mut fixture: Value =
            serde_json::from_slice(&std::fs::read(&file).map_err(|error| error.to_string())?)
                .map_err(|error| error.to_string())?;
        let base = fixture["base"]
            .as_str()
            .ok_or("Owned fixture is missing its local base")?
            .to_owned();
        let base_url = url::Url::parse(&base).map_err(|error| error.to_string())?;
        if !base_url
            .host_str()
            .ok_or("Owned fixture base has no host")?
            .parse::<std::net::IpAddr>()
            .map_err(|error| error.to_string())?
            .is_loopback()
        {
            return Err("Concurrent fixture must use a loopback Relay server".into());
        }
        for name in ["alice", "bob", "eve"] {
            let channel = fixture[if name == "eve" { "other_id" } else { "room_id" }].clone();
            let token = fixture["users"][name]["session"]["token"]
                .as_str()
                .ok_or("Owned fixture session missing")?;
            concurrent_deadline(
                Duration::from_secs(12),
                "owned voice join",
                api(
                    &base,
                    token,
                    "/api/voice/join",
                    json!({"channel_id":channel}),
                ),
            )
            .await?;
            fixture["users"][name]["ticket"] = concurrent_deadline(
                Duration::from_secs(12),
                "owned RTC ticket",
                api(
                    &base,
                    token,
                    "/api/rtc/token",
                    json!({"channel_id":channel}),
                ),
            )
            .await?;
        }
        let ticket = |name: &str| -> Result<RtcTicket, String> {
            let ticket: RtcTicket =
                serde_json::from_value(fixture["users"][name]["ticket"].clone())
                    .map_err(|error| error.to_string())?;
            let parsed = url::Url::parse(&ticket.url).map_err(|error| error.to_string())?;
            if !parsed
                .host_str()
                .ok_or("Owned RTC ticket has no host")?
                .parse::<std::net::IpAddr>()
                .map_err(|error| error.to_string())?
                .is_loopback()
            {
                return Err("Concurrent fixture RTC endpoint must be loopback".into());
            }
            Ok(ticket)
        };
        let alice_keys = fixture_call_keys(&fixture, "alice");
        let bob_keys = fixture_call_keys(&fixture, "bob");
        publisher = Some(RtcSession::connect(
            ticket("alice")?,
            Context::default(),
            alice_keys.clone(),
        )?);
        viewer = Some(RtcSession::connect(
            ticket("bob")?,
            Context::default(),
            bob_keys.clone(),
        )?);
        other = Some(RtcSession::connect(
            ticket("eve")?,
            Context::default(),
            fixture_call_keys(&fixture, "eve"),
        )?);
        let publisher = publisher.as_ref().ok_or("Publisher missing")?;
        let viewer = viewer.as_ref().ok_or("Viewer missing")?;
        let other = other.as_ref().ok_or("Other-room owner missing")?;
        let sink = ApplicationAudioSink::default();
        viewer.set_application_audio_sink(sink.clone());
        for session in [publisher, viewer, other] {
            wait_active(session).await;
        }
        publisher.set_video_visible(false);
        viewer.set_video_visible(true);
        let options = CaptureOptions {
            quality: crate::capture::CaptureQuality::P480,
            fps: 30,
            codec: VideoCodecPreference::Auto,
            adaptive_video: false,
            application_audio: false,
            ..Default::default()
        };
        publisher.start_screen(concurrent_fixture_source(), options)?;
        let room = concurrent_room(publisher).await?;
        concurrent_wait_attached(publisher, &room, false, None).await?;
        let original_scope = alice_keys.scope.clone();
        let original_key_epoch = alice_keys.epoch;
        let (sender, receiver) = mpsc::sync_channel(2);
        let (reply, result) = tokio::sync::oneshot::channel();
        concurrent_deadline(
            Duration::from_secs(2),
            "application audio attach send",
            publisher
                .commands
                .send(Command::TestApplicationAudio { receiver, reply }),
        )
        .await?
        .map_err(|_| "Concurrent publisher stopped before audio attach")?;
        let audio_sid = concurrent_deadline(
            Duration::from_secs(10),
            "protected application audio attach",
            result,
        )
        .await?
        .map_err(|_| "Concurrent owner dropped audio attach response")??;
        let stop = pcm_stop.clone();
        let generated = generated_pcm.clone();
        pcm_owner = Some(tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(20));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut phase = 0_f32;
            let mut timestamp_us = 1_000_000_i64;
            let mut packet_sequence = 0_u64;
            while !stop.load(Ordering::Acquire) {
                tick.tick().await;
                if stop.load(Ordering::Acquire) {
                    break;
                }
                // Markers alternate every 40 ms, so each measured window
                // requires fresh frequency changes beyond the decode backlog.
                let frequency = if (packet_sequence / 2).is_multiple_of(2) {
                    440.0
                } else {
                    880.0
                };
                packet_sequence += 1;
                let samples = (0..960)
                    .flat_map(|_| {
                        phase += std::f32::consts::TAU * frequency / 48_000.0;
                        let sample = (phase.sin() * 9000.0) as i16;
                        [sample, sample]
                    })
                    .collect();
                let frame = ProcessAudioFrame {
                    samples,
                    timestamp_us,
                };
                timestamp_us += 20_000;
                match sender.try_send(frame) {
                    Ok(()) => {
                        generated.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(mpsc::TrySendError::Full(_)) => {}
                    Err(mpsc::TrySendError::Disconnected(_)) => break,
                }
            }
        }));
        let viewer_room = concurrent_room(viewer).await?;
        let user = alice_keys.local_user_id.as_str();
        let mut audio = concurrent_native_audio(&viewer_room, &bob_keys, user, &audio_sid).await?;
        // Warm up the actual decoder before measuring separate recovery windows.
        let warm_until = Instant::now() + Duration::from_millis(400);
        while Instant::now() < warm_until {
            concurrent_deadline(Duration::from_secs(1), "PCM decoder warmup", audio.next())
                .await?
                .ok_or("Generated PCM decoder ended in warmup")?;
        }

        let initial = concurrent_observe_live(
            publisher,
            viewer,
            &room,
            user,
            false,
            &mut audio,
            "initial_audio",
        )
        .await?;
        let initial_mixer = reconnect_audio_mixer_progress(&sink).await?;
        drop(audio);
        let generation = publisher.epochs[0].load(Ordering::Acquire);
        let mut evidence = Vec::new();
        for scenario in [
            livekit::SimulateScenario::SignalReconnect,
            livekit::SimulateScenario::FullReconnect,
            livekit::SimulateScenario::FullReconnect,
        ] {
            let local = room.local_participant().track_publications();
            if local.len() != 2 {
                return Err("Screen/audio publications duplicated before recovery".into());
            }
            let previous = local
                .values()
                .find(|p| p.source() == TrackSource::ScreenshareAudio)
                .cloned()
                .ok_or("Owned audio publication disappeared")?;
            let previous_track = previous.track().ok_or("Owned native audio track missing")?;
            let old_video = concurrent_local_video(&room, false)?.0.sid();
            let start = Instant::now();
            room.simulate_scenario(scenario)
                .await
                .map_err(|e| e.to_string())?;
            let mut saw_reconnecting = false;
            let current = loop {
                if let Some(error) = publisher.take_error().or_else(|| viewer.take_error()) {
                    return Err(error);
                }
                saw_reconnecting |= room.connection_state() == ConnectionState::Reconnecting;
                let local = room.local_participant().track_publications();
                let current = local
                    .values()
                    .find(|p| p.source() == TrackSource::ScreenshareAudio)
                    .cloned();
                let video_changed = local
                    .values()
                    .any(|p| p.source() == TrackSource::Screenshare && p.sid() != old_video);
                if let Some(current) = current {
                    let sid_ok = if scenario == livekit::SimulateScenario::SignalReconnect {
                        current.sid() == previous.sid()
                    } else {
                        current.sid() != previous.sid()
                    };
                    if saw_reconnecting
                        && sid_ok
                        && video_changed
                        && local.len() == 2
                        && publisher.is_active()
                        && room.connection_state() == ConnectionState::Connected
                    {
                        break current;
                    }
                }
                if start.elapsed() > Duration::from_secs(20) {
                    return Err(format!(
                    "Audio did not recover: {scenario:?}, state={:?}, published={}, active={}, error={:?}",
                    room.connection_state(),local.len(),publisher.is_active(),publisher.take_error()));
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            };
            if current.encryption_type() != EncryptionType::Gcm
                || current.track().is_none_or(|t| {
                    t.rtc_track().id() != previous_track.rtc_track().id()
                        || !t.rtc_track().enabled()
                })
            {
                return Err(
                    "Recovered audio lost protection or its original enabled native source".into(),
                );
            }
            if publisher.epochs[0].load(Ordering::Acquire) != generation
                || !publisher.is_application_audio_active()
            {
                return Err(
                    "Recovery restarted selected capture or stopped generated audio".into(),
                );
            }
            if scenario != livekit::SimulateScenario::SignalReconnect {
                concurrent_replay_late(publisher, previous.clone(), previous_track.clone(), false)
                    .await?;
                concurrent_replay_late(publisher, previous, previous_track, true).await?;
            }
            let mut audio =
                concurrent_native_audio(&viewer_room, &bob_keys, user, &current.sid()).await?;
            let video_sid = concurrent_local_video(&room, false)?.0.sid();
            concurrent_wait_native_decoded(
                viewer,
                &viewer_room,
                &bob_keys,
                user,
                false,
                &video_sid,
                "audio reconnect screen readiness",
            )
            .await?;
            let progress = concurrent_observe_live(
                publisher,
                viewer,
                &room,
                user,
                false,
                &mut audio,
                "recovered_audio",
            )
            .await?;
            let mixer = reconnect_audio_mixer_progress(&sink).await?;
            evidence.push(json!({"scenario":format!("{scenario:?}"),"reconnect_and_observation_ms":start.elapsed().as_millis(),
                "protected_publications":room.local_participant().track_publications().len(),
                "same_capture_generation":true,"native_progress":progress,"production_mixer":mixer}));
        }
        room.simulate_scenario(livekit::SimulateScenario::FullReconnect)
            .await
            .map_err(|e| e.to_string())?;
        let until = Instant::now() + Duration::from_secs(5);
        while !publisher.is_reconnecting() {
            if Instant::now() >= until {
                return Err("Owned audio Stop never saw reconnecting".into());
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let stopped = Instant::now();
        publisher.stop_screen();
        if publisher.is_application_audio_active() {
            return Err("Stop kept application audio active".into());
        }
        loop {
            if let Some(error) = publisher.take_error() {
                return Err(error);
            }
            if room.connection_state() == ConnectionState::Connected
                && room.local_participant().track_publications().is_empty()
                && publisher
                    .audio_tracks
                    .lock()
                    .map_err(|_| "Owned audio map poisoned")?
                    .is_empty()
            {
                break;
            }
            if stopped.elapsed() > Duration::from_secs(5) {
                return Err("Stop left generated audio or screen registered".into());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        concurrent_deadline(
            Duration::from_secs(2),
            "producer ended on receiver disconnect",
            pcm_owner.as_mut().ok_or("Owned PCM owner missing")?,
        )
        .await?
        .map_err(|e| e.to_string())?;
        pcm_owner.take();
        tokio::time::sleep(Duration::from_millis(250)).await;
        sink.clear();
        tokio::time::sleep(Duration::from_millis(250)).await;
        let mut queue = sink.try_queues().ok_or("Owned stopped mixer unavailable")?;
        if (0..4800).any(|_| queue.next_sample() != 0.0) {
            return Err("Stopped audio returned to the production mixer".into());
        }
        if pcm_stop.load(Ordering::Acquire)
            || !publisher.is_active()
            || !room.local_participant().track_publications().is_empty()
        {
            return Err("Audio Stop needed fixture cancellation or closed the room".into());
        }
        if alice_keys.scope != original_scope || alice_keys.epoch != original_key_epoch {
            return Err("Audio recovery changed verified scope or key epoch".into());
        }
        Ok::<Value, String>(
            json!({"initial_native":initial,"initial_production_mixer":initial_mixer,"recoveries":evidence,
            "generated_pcm_packets":generated_pcm.load(Ordering::Relaxed),
            "stop_during_full_reconnect_retired_video_and_audio":true,"producer_finished_on_receiver_disconnect":true,
            "production_mixer_silent_after_stop":true,"physical_devices_opened":false,
            "scope":"Production RTC owner, actual GCM/Opus SFU media, generated stereo PCM, native receiver and actual application-audio mixer sink. Signal/full reconnect injected via SDK; local loopback only. No selected-process capture, physical devices, WAN or subjective listening acceptance."}),
        )
    });
    let outcome = tokio::time::timeout(Duration::from_secs(100), body.catch_unwind()).await;
    // Stop every owner first, then attempt each cleanup independently, including
    // when the main body returned an error, panicked, or exceeded its deadline.
    pcm_stop.store(true, Ordering::Release);
    for session in [&publisher, &viewer, &other].into_iter().flatten() {
        session.cancel();
    }
    let mut cleanup_errors = Vec::new();
    if let Some(mut owner) = pcm_owner.take() {
        match tokio::time::timeout(Duration::from_secs(2), &mut owner).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => cleanup_errors.push(format!("Generated PCM owner failed: {error}")),
            Err(_) => {
                owner.abort();
                let _ = tokio::time::timeout(Duration::from_secs(1), owner).await;
                cleanup_errors.push("Generated PCM owner required abort".into());
            }
        }
    }
    for session in [&mut publisher, &mut viewer, &mut other]
        .into_iter()
        .flatten()
    {
        if std::panic::AssertUnwindSafe(close_recovery_controller(session))
            .catch_unwind()
            .await
            .is_err()
        {
            cleanup_errors.push("A protected RTC owner did not finish normal cleanup".into());
        }
    }
    assert!(
        cleanup_errors.is_empty(),
        "Owned concurrent cleanup failed: {}",
        cleanup_errors.join("; ")
    );
    let evidence = match outcome {
        Ok(Ok(Ok(evidence))) => evidence,
        Ok(Ok(Err(error))) => panic!("Owned concurrent recovery fixture failed: {error}"),
        Ok(Err(panic)) => std::panic::resume_unwind(panic),
        Err(_) => panic!("Owned concurrent recovery fixture exceeded its overall deadline"),
    };
    std::fs::write(
        std::path::Path::new(&file)
            .parent()
            .unwrap()
            .join("application-audio-reconnect-evidence.json"),
        serde_json::to_vec_pretty(&evidence).unwrap(),
    )
    .unwrap();
}
