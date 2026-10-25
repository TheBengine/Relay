//! Silent preflight in an isolated room; all sources are generated in memory.
use super::*;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use livekit::webrtc::{
    native::frame_cryptor::EncryptionState,
    stats::{IceCandidatePairState, IceCandidateType, RtcStats},
};

const FAILED: &str = "The generated voice/video check did not complete.";

pub(crate) fn check(mut tickets: [RtcTicket; 2], cancel: &AtomicBool, private:Option<&crate::iroh_route::PreparedRoute>) -> Result<(), String> {
    if cancel.load(Ordering::Acquire) {
        return Err("Stopped".into());
    }
    let lease=|ticket:&RtcTicket|match private{
        Some(route)=>crate::embedded_network::HttpsMediaLease::open_prepared(ticket.media_bridge.as_ref().ok_or(FAILED)?,&ticket.url,route),
        None=>crate::embedded_network::HttpsMediaLease::open(ticket.media_bridge.as_ref().ok_or(FAILED)?,&ticket.url),
    };
    let first=lease(&tickets[0])?;let second=lease(&tickets[1])?;
    if let Some(route)=private{for ticket in &mut tickets{ticket.url=route.signaling(&ticket.url)?;}}
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|_| FAILED)?;
    let result = runtime.block_on(check_with_turns(
        tickets,
        [first.turn.clone(), second.turn.clone()],
        cancel,
    ));
    runtime.shutdown_timeout(Duration::from_secs(2));
    result
}
fn keys(tickets: &[RtcTicket; 2]) -> Result<[Arc<CallKeys>; 2], String> {
    let mut users = Vec::new();
    for ticket in tickets {
        if ticket.token.len() > 16_384 {
            return Err(FAILED.into());
        }
        let payload = ticket
            .token
            .split('.')
            .nth(1)
            .and_then(|p| URL_SAFE_NO_PAD.decode(p).ok())
            .ok_or(FAILED)?;
        let claims: serde_json::Value = serde_json::from_slice(&payload).map_err(|_| FAILED)?;
        let user = claims["sub"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 128)
            .ok_or(FAILED)?
            .to_owned();
        if claims["video"]["room"] != ticket.room || users.contains(&user) {
            return Err(FAILED.into());
        }
        users.push(user);
    }
    if tickets[0].room != tickets[1].room || tickets[0].url != tickets[1].url {
        return Err(FAILED.into());
    }
    let mut senders = HashMap::new();
    for user in &users {
        let mut opus = zeroize::Zeroizing::new([0u8; 32]);
        let mut rtc = zeroize::Zeroizing::new([0u8; 32]);
        getrandom::getrandom(opus.as_mut()).map_err(|_| FAILED)?;
        getrandom::getrandom(rtc.as_mut()).map_err(|_| FAILED)?;
        senders.insert(
            user.clone(),
            crate::e2ee::SenderKeys {
                device_id: format!("preflight-{}", uuid::Uuid::new_v4()),
                opus,
                rtc,
            },
        );
    }
    Ok([0, 1].map(|i| {
        Arc::new(CallKeys {
            group_id: format!("preflight-{}", tickets[0].room),
            scope: tickets[0].room.clone(),
            epoch: 1,
            local_user_id: users[i].clone(),
            local_device_id: senders[&users[i]].device_id.clone(),
            sender_keys: senders.clone(),
            voice_replay: Mutex::default(),
        })
    }))
}
struct ProbePeer {
    room: Room,
    events: tokio::sync::mpsc::UnboundedReceiver<RoomEvent>,
    keys: Arc<CallKeys>,
    video: Vec<(NativeVideoSource, LocalVideoTrack, LocalTrackPublication, u32, u32)>,
    audio: Option<(NativeAudioSource,LocalAudioTrack,LocalTrackPublication)>,
    readers: HashMap<String, (TrackSource, NativeVideoStream, i64)>,
    audio_readers: HashMap<String, NativeAudioStream>,
    frames: [u32; 2],
    color: [bool; 2],
    samples: u64,
    energy: f64,
}
impl ProbePeer {
    async fn connect(
        ticket: &RtcTicket,
        turn: &crate::embedded_network::Turn,
        keys: Arc<CallKeys>,
    ) -> Result<Self, String> {
        let mut options = protected_room_options(&keys)?;
        options.join_retries = 0;
        options.rtc_config.ice_transport_type =
            livekit::webrtc::peer_connection_factory::IceTransportsType::Relay;
        options.rtc_config.enable_loopback_turn = true;
        options.rtc_config.ice_servers =
            vec![livekit::webrtc::peer_connection_factory::IceServer {
                urls: vec![turn.url.clone()],
                username: turn.username.clone(),
                password: turn.password.clone(),
            }];
        let (room, events) = Room::connect(&ticket.url, &ticket.token, options)
            .await
            .map_err(|_| FAILED)?;
        if room.local_participant().identity().to_string() != keys.local_user_id {
            let _ = room.close().await;
            return Err(FAILED.into());
        }
        Ok(Self {
            room,
            events,
            keys,
            video: Vec::new(),
            audio: None,
            readers: HashMap::new(),
            audio_readers: HashMap::new(),
            frames: [0; 2],
            color: [false; 2],
            samples: 0,
            energy: 0.0,
        })
    }
    async fn publish(&mut self) -> Result<(), String> {
        for (kind, width, height) in [
            (TrackSource::Camera, 256, 144),
            (TrackSource::Screenshare, 512, 288),
        ] {
            let source = NativeVideoSource::new(VideoResolution { width, height }, true);
            let track = LocalVideoTrack::create_video_track(
                "relay-generated-preflight",
                RtcVideoSource::Native(source.clone()),
            );
            let published = self
                .room
                .local_participant()
                .publish_track(
                    LocalTrack::Video(track.clone()),
                    TrackPublishOptions {
                        source: kind,
                        video_codec: VideoCodec::VP8,
                        video_encoding: Some(VideoEncoding {
                            max_bitrate: 200_000,
                            max_framerate: 10.0,
                        }),
                        simulcast: false,
                        ..Default::default()
                    },
                )
                .await
                .map_err(|_| FAILED)?;
            if published.encryption_type() != EncryptionType::Gcm {
                return Err(FAILED.into());
            }
            self.video.push((source,track,published,width,height));
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
            "relay-generated-preflight-tone",
            RtcAudioSource::Native(source.clone()),
        );
        let published = self
            .room
            .local_participant()
            .publish_track(
                LocalTrack::Audio(track.clone()),
                TrackPublishOptions {
                    source: TrackSource::ScreenshareAudio,
                    dtx: false,
                    stereo: true,
                    ..Default::default()
                },
            )
            .await
            .map_err(|_| FAILED)?;
        if published.encryption_type() != EncryptionType::Gcm {
            return Err(FAILED.into());
        }
        self.audio = Some((source,track,published));
        Ok(())
    }
    fn poll(&mut self) -> Result<(), String> {
        while let Ok(event) = self.events.try_recv() {
            match event {
                RoomEvent::TrackPublished {
                    publication,
                    participant,
                } => publication.set_subscribed(verified_publication(
                    &self.keys,
                    &participant.identity().to_string(),
                    &publication,
                )),
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
                        return Err("Probe received an unverified track".into());
                    }
                    let sid = publication.sid().to_string();
                    match track {
                        RemoteTrack::Video(track) => {
                            if self.readers.len() >= 2 && !self.readers.contains_key(&sid) {
                                return Err(FAILED.into());
                            }
                            self.readers.insert(
                                sid,
                                (
                                    publication.source(),
                                    NativeVideoStream::new(track.rtc_track()),
                                    -1,
                                ),
                            );
                        }
                        RemoteTrack::Audio(track) => {
                            if !self.audio_readers.is_empty() && !self.audio_readers.contains_key(&sid) {
                                return Err(FAILED.into());
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
                RoomEvent::Disconnected { .. } | RoomEvent::Reconnecting => {
                    return Err("Probe connection interrupted".into())
                }
                RoomEvent::E2eeStateChanged {
                    state:
                        EncryptionState::EncryptionFailed
                        | EncryptionState::DecryptionFailed
                        | EncryptionState::MissingKey
                        | EncryptionState::InternalError,
                    ..
                } => return Err("Probe media authentication failed".into()),
                _ => {}
            }
        }
        for (kind, stream, last) in self.readers.values_mut() {
            for _ in 0..3 {
                let Some(Some(frame)) = stream.next().now_or_never() else {
                    break;
                };
                let i = if *kind == TrackSource::Camera { 0 } else { 1 };
                let (w, h) = if i == 0 { (256, 144) } else { (512, 288) };
                if (frame.buffer.width(), frame.buffer.height()) != (w, h) {
                    return Err(format!("Probe decoded an unexpected video size: {}x{}",frame.buffer.width(),frame.buffer.height()));
                }
                if frame.timestamp_us != *last {
                    *last = frame.timestamp_us;
                    self.frames[i] += 1;
                    let mut pixels = vec![0u8; (w * h * 4) as usize];
                    frame.buffer.to_argb(
                        VideoFormatType::ABGR,
                        &mut pixels,
                        w * 4,
                        w as i32,
                        h as i32,
                    );
                    self.color[i] |= pixels
                        .chunks_exact(4)
                        .take(20)
                        .any(|p| p[0].abs_diff(p[2]) > 30);
                }
            }
        }
        for stream in self.audio_readers.values_mut() {
            for _ in 0..6 {
                let Some(Some(frame)) = stream.next().now_or_never() else {
                    break;
                };
                if frame.sample_rate != 48_000 || frame.num_channels != 2 {
                    return Err("Probe audio format changed".into());
                }
                self.samples += frame.data.len() as u64;
                self.energy += frame
                    .data
                    .iter()
                    .map(|v| f64::from(*v).powi(2))
                    .sum::<f64>();
            }
        }
        Ok(())
    }
    async fn feed(&self, index: u64) -> Result<(), String> {
        if index.is_multiple_of(10) {
            for (source,_,_,width,height) in &self.video {
                let mut buffer = I420Buffer::new(*width, *height);
                let (y, u, v) = buffer.data_mut();
                y.fill(65 + (index % 100) as u8);
                u.fill(85);
                v.fill(230);
                source.capture_frame(&VideoFrame {
                    rotation: VideoRotation::VideoRotation0,
                    timestamp_us: (index * 10_000 + 1_000_000) as i64,
                    frame_metadata: None,
                    buffer,
                });
            }
        }
        let pcm = (0..480)
            .flat_map(|n| {
                let angle = (index * 480 + n) as f32 * std::f32::consts::TAU * 1000.0 / 48_000.0;
                [
                    (angle.sin() * 8000.0) as i16,
                    ((angle * 2.0).sin() * 8000.0) as i16,
                ]
            })
            .collect::<Vec<_>>();
        self.audio
            .as_ref()
            .ok_or(FAILED)?.0
            .capture_frame(&AudioFrame {
                data: pcm.into(),
                sample_rate: 48_000,
                num_channels: 2,
                samples_per_channel: 480,
            })
            .await
            .map_err(|_| FAILED.into())
    }
    fn decoded(&self) -> bool {
        self.frames.iter().all(|n| *n >= 3)
            && self.color.iter().all(|v| *v)
            && self.samples >= 24_000
            && self.energy / (self.samples.max(1) as f64) > 250_000.0
    }
}
async fn selected_turn(room: &Room, expected: &str) -> Result<(), String> {
    let stats = room.get_stats().await.map_err(|_| FAILED)?;
    let scopes = if room.is_single_peer_connection_active() {
        vec![stats.publisher_stats.as_slice()]
    } else {
        vec![
            stats.publisher_stats.as_slice(),
            stats.subscriber_stats.as_slice(),
        ]
    };
    for rows in scopes {
        let selected = rows
            .iter()
            .filter_map(|r| {
                if let RtcStats::Transport(t) = r {
                    Some(t.transport.selected_candidate_pair_id.as_str())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        let good=rows.iter().any(|r|{
            let RtcStats::CandidatePair(pair)=r else{return false;};
            if !selected.contains(&pair.rtc.id.as_str())||!pair.candidate_pair.nominated||pair.candidate_pair.state!=Some(IceCandidatePairState::Succeeded)||pair.candidate_pair.bytes_sent==0||pair.candidate_pair.bytes_received==0{return false;}
            rows.iter().any(|r|matches!(r,RtcStats::LocalCandidate(c) if c.rtc.id==pair.candidate_pair.local_candidate_id
                && matches!(c.local_candidate.candidate_type,Some(IceCandidateType::Relay|IceCandidateType::Prflx))
                && c.local_candidate.url==expected && format!("{:?}",c.local_candidate.relay_protocol)=="Some(Tcp)"))
        });
        if !good {
            return Err(FAILED.into());
        }
    }
    Ok(())
}
pub(super) async fn check_with_turns(
    tickets: [RtcTicket; 2],
    turns: [crate::embedded_network::Turn; 2],
    cancel: &AtomicBool,
) -> Result<(), String> {
    crate::tls_crypto::initialize();
    let keys = keys(&tickets)?;
    let mut peers = Vec::new();
    let work = async {
        for i in 0..2 {
            peers.push(ProbePeer::connect(&tickets[i], &turns[i], keys[i].clone()).await?);
        }
        for peer in &mut peers {
            peer.publish().await?;
        }
        let mut index = 0;
        let mut tick = tokio::time::interval(Duration::from_millis(10));
        loop {
            tick.tick().await;
            for peer in &mut peers {
                peer.poll()?;
                peer.feed(index).await?;
            }
            index += 1;
            if peers.iter().all(ProbePeer::decoded) {
                break;
            }
        }
        for i in 0..2 {
            selected_turn(&peers[i].room, &turns[i].url).await?;
        }
        Ok::<_, String>(())
    };
    let stopped = async {
        loop {
            if cancel.load(Ordering::Acquire) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    };
    let result = tokio::select! {biased;_=stopped=>Err("Stopped".into()),result=tokio::time::timeout(Duration::from_secs(20),work)=>result.unwrap_or_else(|_|Err(FAILED.into()))};
    for peer in &peers {
        let _ = tokio::time::timeout(Duration::from_secs(2), peer.room.close()).await;
    }
    result
}
