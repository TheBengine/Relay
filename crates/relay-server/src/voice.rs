use crate::{lock, now, AppState};
use chacha20poly1305::{
    aead::{Aead, Payload},
    ChaCha20Poly1305, KeyInit, Nonce,
};
use relay_protocol::direct_voice::{self, Peer, Roster};
use relay_protocol::{
    User, VoiceState, AUDIO_PACKET_V2, AUDIO_PACKET_V3, AUDIO_V2_HEADER_LEN, AUDIO_WHISPER,
    UDP_HEADER_LEN, UDP_MAX_PACKET, UDP_PATH_PROBE, UDP_PING,
};
use sha2::{Digest, Sha256};
use std::net::SocketAddr;

#[derive(Default)]
pub(crate) struct DirectAdmission {
    enabled: bool,
    relay_rtt_us: u64,
    seen: Option<std::time::Instant>,
    generation: u64,
}

pub(crate) struct VoiceSession {
    pub user: User,
    pub channel_id: String,
    pub device_id: Option<String>,
    pub token_hash: String,
    /// The plain digest of the session token, never stored: the app derives
    /// its open-call device name from it.
    pub token_digest: String,
    pub private_call_id: Option<String>,
    /// A resumed private seat cannot manufacture more recovery time through
    /// bootstrap/repeated restarts. Authenticated transport confirms it is live.
    pub recovery_deadline: Option<u64>,
    pub key: [u8; 32],
    pub state: VoiceState,
    pub remote: Option<SocketAddr>,
    pub tunnel_sender: Option<tokio::sync::mpsc::Sender<(u64, bool, Vec<u8>)>>,
    pub tunnel_id: Option<String>,
    pub in_sequence: Option<u64>,
    pub replay: ReplayWindow,
    pub out_sequence: u64,
    pub last_seen: u64,
    pub expires: u64,
    pub direct: DirectAdmission,
    pub can_speak: bool,
    pub can_stream: bool,
    pub whisper_channels: std::sync::Arc<std::collections::HashSet<String>>,
}

/// Authentication must succeed before this replay window is advanced.
#[derive(Clone, Default)]
pub(crate) struct ReplayWindow {
    highest: u64,
    bits: u128,
    initialized: bool,
}
impl ReplayWindow {
    fn accept(&mut self, sequence: u64) -> bool {
        if !self.initialized {
            self.initialized = true;
            self.highest = sequence;
            self.bits = 1;
            return true;
        }
        if sequence > self.highest {
            let distance = sequence - self.highest;
            self.bits = if distance >= 128 {
                1
            } else {
                (self.bits << distance) | 1
            };
            self.highest = sequence;
            return true;
        }
        let distance = self.highest - sequence;
        if distance >= 128 || self.bits & (1 << distance) != 0 {
            return false;
        }
        self.bits |= 1 << distance;
        true
    }
}
impl VoiceSession {
    pub fn alive_at(&self, timestamp: u64) -> bool {
        self.expires > timestamp && timestamp.saturating_sub(self.last_seen) < 60
            && self.recovery_deadline.is_none_or(|end| timestamp < end)
    }
}

pub(crate) struct Permissions {
    pub can_speak: bool,
    pub can_stream: bool,
    pub whisper_channels: std::sync::Arc<std::collections::HashSet<String>>,
}

/// Called only at admission or an authorization change, never for audio packets.
pub(crate) fn permissions(
    db: &rusqlite::Connection,
    channel: &str,
    user: &str,
) -> crate::ApiResult<Permissions> {
    let channels = {
        let mut query = db.prepare("SELECT id FROM channels WHERE kind='voice'")?;
        let rows = query
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        rows
    };
    Ok(Permissions {
        can_speak: crate::conversations::stage_speaker(db, channel, user)?,
        can_stream: crate::channel_access(db, channel, user, crate::STREAM).is_ok(),
        whisper_channels: channels
            .into_iter()
            .filter(|id| crate::channel_access(db, id, user, crate::JOIN).is_ok())
            .collect::<std::collections::HashSet<_>>()
            .into(),
    })
}

fn direct_peer(
    sessions: &std::collections::HashMap<u64, VoiceSession>,
    local_id: u64,
) -> Option<Peer> {
    let local = sessions.get(&local_id)?;
    let fresh = |session: &VoiceSession| {
        session.direct.enabled
            && session.alive_at(now())
            && session.direct.seen.is_some_and(|seen| {
                seen.elapsed() < std::time::Duration::from_millis(direct_voice::LEASE_MS)
            })
    };
    if !fresh(local) {
        return None;
    }
    let members: Vec<_> = sessions
        .iter()
        .filter(|(_, s)| s.channel_id == local.channel_id && s.alive_at(now()))
        .collect();
    if members.len() != 2 {
        return None;
    }
    let (&peer_id, peer) = members.into_iter().find(|(id, _)| **id != local_id)?;
    if !fresh(peer) || peer.user.id == local.user.id {
        return None;
    }
    let address = peer.remote?;
    let ((low_id, low), (high_id, high)) = if local_id < peer_id {
        ((local_id, local), (peer_id, peer))
    } else {
        ((peer_id, peer), (local_id, local))
    };
    let mut digest = Sha256::new();
    digest.update(b"relay-direct-voice-v1");
    digest.update(low_id.to_le_bytes());
    digest.update(high_id.to_le_bytes());
    digest.update(low.key);
    digest.update(high.key);
    digest.update(low.direct.generation.to_le_bytes());
    digest.update(high.direct.generation.to_le_bytes());
    digest.update(local.channel_id.as_bytes());
    Some(Peer {
        session: peer_id,
        user: peer.user.id.clone(),
        address,
        key: digest.finalize().into(),
        relay_rtt_us: peer.direct.relay_rtt_us,
        send_allowed: !local.state.muted && !peer.state.deafened && local.can_speak,
        receive_allowed: !local.state.deafened && !peer.state.muted && peer.can_speak,
    })
}
fn roster_payload(request: u64, peer: Option<Peer>) -> Vec<u8> {
    let mut payload = vec![direct_voice::ROSTER];
    payload.extend(serde_json::to_vec(&Roster { request, peer }).expect("bounded direct roster"));
    payload
}
fn revoke_direct(state: &AppState, socket: &tokio::net::UdpSocket) {
    let mut sessions = lock(&state.0.voice);
    let payload = roster_payload(u64::MAX, None);
    for (&id, session) in sessions
        .iter_mut()
        .filter(|(_, session)| session.direct.enabled)
    {
        if let Some(address) = session.remote {
            if let Some(packet) = seal_packet(id, session, &payload) {
                let _ = socket.try_send_to(&packet, address);
            }
        }
    }
}

pub(crate) fn revoke_all_direct(state: &AppState) {
    let socket = lock(&state.0.voice_socket).clone();
    if let Some(socket) = socket {
        revoke_direct(state, &socket);
    }
}

fn nonce(direction: u32, sequence: u64) -> [u8; 12] {
    let mut result = [0; 12];
    result[..4].copy_from_slice(&direction.to_le_bytes());
    result[4..].copy_from_slice(&sequence.to_le_bytes());
    result
}

fn valid_encrypted_voice(payload: &[u8], user_id: &str, selected_device: Option<&str>) -> bool {
    if payload.len() < 79 || payload[25] & !1 != 0 || payload[52..60] != payload[9..17] {
        return false;
    }
    let user_len = payload[34] as usize;
    let device_len = payload[35] as usize;
    if !(1..=128).contains(&user_len) || !(1..=128).contains(&device_len) {
        return false;
    }
    let cipher_start = 60 + user_len + device_len;
    if payload.len() <= cipher_start + 16 {
        return false;
    }
    let Ok(user) = std::str::from_utf8(&payload[60..60 + user_len]) else {
        return false;
    };
    let Ok(device) = std::str::from_utf8(&payload[60 + user_len..cipher_start]) else {
        return false;
    };
    user == user_id
        && selected_device.is_none_or(|selected| selected == device)
        && device
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b':' | b'.'))
}

pub(crate) fn relay_packet(
    state: &AppState,
    source: SocketAddr,
    packet: &[u8],
) -> Vec<(SocketAddr, Vec<u8>)> {
    if !state.0.handover.writes_allowed()
        || packet.len() < UDP_HEADER_LEN + 17
        || packet.len() > UDP_MAX_PACKET
    {
        return vec![];
    }
    let session_id = u64::from_le_bytes(packet[..8].try_into().expect("fixed header"));
    let sequence = u64::from_le_bytes(packet[8..16].try_into().expect("fixed header"));
    let mut sessions = lock(&state.0.voice);
    let Some(sender) = sessions.get_mut(&session_id) else {
        return vec![];
    };
    if !sender.alive_at(now()) {
        return vec![];
    }
    let cipher = ChaCha20Poly1305::new_from_slice(&sender.key).expect("32-byte key");
    let Ok(mut payload) = cipher.decrypt(
        Nonce::from_slice(&nonce(0, sequence)),
        Payload {
            msg: &packet[16..],
            aad: &packet[..16],
        },
    ) else {
        return vec![];
    };
    match payload.first() {
        Some(0) if payload.len() == 1 => {
            // A delayed keepalive from an old address must not undo a newer NAT binding.
            if sender.remote != Some(source) && sender.in_sequence.is_some_and(|s| sequence <= s) {
                return vec![];
            }
            if !sender.replay.accept(sequence) {
                return vec![];
            }
            sender.remote = Some(source);
            sender.in_sequence = Some(sender.in_sequence.map_or(sequence, |s| s.max(sequence)));
            sender.last_seen = now();
            sender.recovery_deadline = None;
            return vec![];
        }
        Some(1) if payload.len() >= 10 && sender.remote == Some(source) => {}
        Some(&UDP_PING) if payload.len() == 9 && sender.remote == Some(source) => {}
        // The ticket authenticates this challenge from a candidate transport;
        // only an ordinary keepalive may commit a new voice/NAT binding.
        Some(&UDP_PATH_PROBE) if payload.len() == 17 => {}
        Some(&direct_voice::REQUEST)
            if sender.remote == Some(source) && direct_voice::parse_request(&payload).is_some() => {
        }
        Some(&AUDIO_PACKET_V2)
            if payload.len() > AUDIO_V2_HEADER_LEN && sender.remote == Some(source) =>
        {
            if payload[25] & !1 != 0 {
                return vec![];
            }
        }
        Some(&AUDIO_WHISPER)
            if sender.remote == Some(source)
                && relay_protocol::unwrap_whisper(&payload).is_some_and(|(target, inner)| {
                    target != sender.user.id
                        && valid_encrypted_voice(
                            inner,
                            &sender.user.id,
                            sender.device_id.as_deref(),
                        )
                }) => {}
        Some(&AUDIO_PACKET_V3)
            if sender.remote == Some(source)
                && valid_encrypted_voice(
                    &payload,
                    &sender.user.id,
                    sender.device_id.as_deref(),
                ) => {}
        _ => return vec![],
    }
    if !sender.replay.accept(sequence) {
        return vec![];
    }
    sender.in_sequence = Some(sender.in_sequence.map_or(sequence, |s| s.max(sequence)));
    sender.last_seen = now();
    sender.recovery_deadline = None;
    if let Some((request, enabled, rtt)) = direct_voice::parse_request(&payload) {
        if enabled
            && (!sender.direct.enabled
                || sender.direct.seen.is_none_or(|seen| {
                    seen.elapsed() >= std::time::Duration::from_millis(direct_voice::LEASE_MS)
                }))
        {
            sender.direct.generation = rand::random();
        }
        sender.direct.enabled = enabled;
        sender.direct.relay_rtt_us = rtt;
        sender.direct.seen = Some(std::time::Instant::now());
        let peer = direct_peer(&sessions, session_id);
        let payload = roster_payload(request, peer);
        return sessions
            .get_mut(&session_id)
            .and_then(|sender| seal_packet(session_id, sender, &payload))
            .map(|reply| vec![(source, reply)])
            .unwrap_or_default();
    }
    if matches!(payload.first(), Some(&UDP_PING | &UDP_PATH_PROBE)) {
        return seal_packet(session_id, sender, &payload)
            .map(|reply| vec![(source, reply)])
            .unwrap_or_default();
    }
    if sender.state.muted || !sender.can_speak {
        return vec![];
    }
    let channel_id = sender.channel_id.clone();
    let whisper_channels = sender.whisper_channels.clone();
    // The client-supplied sender field is never trusted. The ticket identifies the speaker.
    payload[1..9].copy_from_slice(&session_id.to_le_bytes());
    let whisper_target =
        relay_protocol::unwrap_whisper(&payload).map(|(target, _)| target.to_owned());
    if whisper_target.is_some() {
        let inner_offset = 10 + payload[9] as usize;
        payload[inner_offset + 1..inner_offset + 9].copy_from_slice(&session_id.to_le_bytes());
    }
    let mut outgoing = vec![];
    for (&recipient_id, recipient) in sessions.iter_mut() {
        if whisper_target
            .as_ref()
            .is_some_and(|target| target != &recipient.user.id)
            || recipient_id == session_id
            || (recipient.channel_id != channel_id
                && (whisper_target.is_none() || !whisper_channels.contains(&recipient.channel_id)))
            || recipient.state.deafened
            || !recipient.alive_at(now())
            // The voice mesh between homes carries this pair now.
            || (whisper_target.is_none()
                && state.0.gateways.mesh_carries(session_id, recipient_id))
        {
            continue;
        }
        let Some(address) = recipient.remote else {
            continue;
        };
        if let Some(result) = seal_packet(recipient_id, recipient, &payload) {
            outgoing.push((address, result));
        }
    }
    outgoing
}
fn seal_packet(id: u64, recipient: &mut VoiceSession, payload: &[u8]) -> Option<Vec<u8>> {
    let next = recipient.out_sequence.checked_add(1)?;
    recipient.out_sequence = next;
    let mut header = [0; UDP_HEADER_LEN];
    header[..8].copy_from_slice(&id.to_le_bytes());
    header[8..].copy_from_slice(&next.to_le_bytes());
    let cipher = ChaCha20Poly1305::new_from_slice(&recipient.key).ok()?;
    let encrypted = cipher
        .encrypt(
            Nonce::from_slice(&nonce(1, next)),
            Payload {
                msg: payload,
                aad: &header,
            },
        )
        .ok()?;
    let mut result = header.to_vec();
    result.extend(encrypted);
    Some(result)
}

fn sealed_path_probe(recipient: &VoiceSession, bytes: &[u8]) -> bool {
    if bytes.len() != UDP_HEADER_LEN + 16 + 17 { return false; }
    let sequence = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
    ChaCha20Poly1305::new_from_slice(&recipient.key).ok()
        .and_then(|cipher| cipher.decrypt(Nonce::from_slice(&nonce(1, sequence)),
            Payload { msg: &bytes[16..], aad: &bytes[..16] }).ok())
        .is_some_and(|clear| clear.len() == 17 && clear[0] == UDP_PATH_PROBE)
}

fn sealed_whisper(recipient: &VoiceSession, bytes: &[u8]) -> bool {
    if bytes.len() < UDP_HEADER_LEN + 17 {
        return false;
    }
    let sequence = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
    ChaCha20Poly1305::new_from_slice(&recipient.key)
        .ok()
        .and_then(|cipher| {
            cipher
                .decrypt(
                    Nonce::from_slice(&nonce(1, sequence)),
                    Payload {
                        msg: &bytes[16..],
                        aad: &bytes[..16],
                    },
                )
                .ok()
        })
        .is_some_and(|clear| {
            relay_protocol::unwrap_whisper(&clear)
                .is_some_and(|(target, _)| target == recipient.user.id)
        })
}
fn deliver(
    state: &AppState,
    source: SocketAddr,
    sender_id: u64,
    outgoing: Vec<(SocketAddr, Vec<u8>)>,
    socket: &tokio::net::UdpSocket,
    tunneled: bool,
) {
    if !state.0.handover.writes_allowed() {
        return;
    }
    for (address, bytes) in outgoing {
        let sessions = lock(&state.0.voice);
        let receiver_id = u64::from_le_bytes(bytes[..8].try_into().expect("sealed reply header"));
        if let (Some(sender), Some(receiver)) =
            (sessions.get(&sender_id), sessions.get(&receiver_id))
        {
            let echo = sender_id == receiver_id;
            if echo
                && source == address
                && sender.alive_at(now())
                && sealed_path_probe(receiver, &bytes)
            {
                if tunneled {
                    if let Some(tunnel) = &receiver.tunnel_sender {
                        let _ = tunnel.try_send((sender_id, false, bytes));
                    }
                } else {
                    let _ = socket.try_send_to(&bytes, source);
                }
                continue;
            }
            let whisper = sealed_whisper(receiver, &bytes);
            if sender.remote == Some(source)
                && receiver.remote == Some(address)
                && sender.alive_at(now())
                && receiver.alive_at(now())
                && (echo
                    || (!sender.state.muted
                        && sender.can_speak
                        && !receiver.state.deafened
                        && (sender.channel_id == receiver.channel_id
                            || (whisper
                                && sender.whisper_channels.contains(&receiver.channel_id)))))
            {
                match receiver
                    .tunnel_sender
                    .as_ref()
                    .filter(|_| !echo || tunneled)
                {
                    Some(tunnel) => {
                        let _ = tunnel.try_send((sender_id, whisper, bytes));
                    }
                    None => {
                        let _ = socket.try_send_to(&bytes, address);
                    }
                }
            }
        }
    }
}

pub async fn run_udp(socket: tokio::net::UdpSocket, state: AppState) -> std::io::Result<()> {
    run_udp_with_source_filter(socket, state, false).await
}

/// Receive voice only from loopback or Tailscale's IPv4 address range. Packet
/// authentication, authorization, replay protection, and encryption stay intact.
pub async fn run_udp_tailscale(
    socket: tokio::net::UdpSocket,
    state: AppState,
) -> std::io::Result<()> {
    run_udp_with_source_filter(socket, state, true).await
}

fn is_tailscale_voice_source(address: std::net::IpAddr) -> bool {
    if address.is_loopback() {
        return true;
    }
    match address {
        std::net::IpAddr::V4(address) => {
            let octets = address.octets();
            octets[0] == 100 && (64..=127).contains(&octets[1])
        }
        std::net::IpAddr::V6(_) => false,
    }
}

async fn run_udp_with_source_filter(
    socket: tokio::net::UdpSocket,
    state: AppState,
    tailscale_only: bool,
) -> std::io::Result<()> {
    let socket = std::sync::Arc::new(socket);
    *lock(&state.0.voice_socket) = Some(socket.clone());
    let mut buffer = [0u8; UDP_MAX_PACKET + 1];
    let mut notices = state.0.events.subscribe();
    loop {
        let (length, source) = tokio::select! {
            received = socket.recv_from(&mut buffer) => received?,
            notice = notices.recv() => {
                if notice.as_ref().map_or(true, |n| matches!(n.event.kind.as_str(), "voice" | "members" | "channels" | "refresh" | "e2ee-device")) {
                    use std::sync::atomic::Ordering;
                    if state.0.voice_refresh.swap(2, Ordering::AcqRel) == 0 {
                        let state = state.clone();
                        let socket = socket.clone();
                        // At most one blocking job per server: a slow backup or
                        // a burst of notices must not build a queue of DB waiters.
                        tokio::task::spawn_blocking(move || loop {
                            state.0.voice_refresh.store(1, Ordering::Release);
                            let _ = state.enforce_voice_permissions();
                            revoke_direct(&state, &socket);
                            if state.0.voice_refresh.compare_exchange(1, 0, Ordering::AcqRel, Ordering::Acquire).is_ok() { break; }
                        });
                    }
                }
                continue;
            }
        };
        if tailscale_only && !is_tailscale_voice_source(source.ip()) {
            continue;
        }
        let outgoing = relay_packet(&state, source, &buffer[..length]);
        if !outgoing.is_empty() {
            let sender_id = u64::from_le_bytes(buffer[..8].try_into().unwrap());
            deliver(&state, source, sender_id, outgoing, &socket, false);
        }
    }
}

pub(crate) async fn tunnel(
    axum::extract::State(state): axum::extract::State<AppState>,
    headers: axum::http::HeaderMap,
    upgrade: axum::extract::WebSocketUpgrade,
) -> Result<axum::response::Response, axum::http::StatusCode> {
    use axum::{http::StatusCode, response::IntoResponse};
    use base64::Engine;
    let authority = state.0.handover.watch_transports();
    let session: u64 = headers
        .get("x-relay-voice-session")
        .and_then(|s| s.to_str().ok())
        .and_then(|s| s.parse().ok())
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let key = headers
        .get("x-relay-voice-key")
        .and_then(|s| s.to_str().ok())
        .and_then(|s| base64::engine::general_purpose::STANDARD.decode(s).ok())
        .filter(|s| s.len() == 32)
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let permit = {
        let sessions = lock(&state.0.voice);
        let Some(ticket) = sessions
            .get(&session)
            .filter(|ticket| ticket.alive_at(now()))
        else {
            return Err(StatusCode::UNAUTHORIZED);
        };
        if key
            .iter()
            .zip(ticket.key)
            .fold(0u8, |diff, (a, b)| diff | (*a ^ b))
            != 0
        {
            return Err(StatusCode::UNAUTHORIZED);
        }
        state
            .0
            .voice_connections
            .try_acquire(&ticket.user.id)
            .ok_or(StatusCode::TOO_MANY_REQUESTS)?
    };
    Ok(upgrade
        .max_message_size(UDP_MAX_PACKET)
        .max_frame_size(UDP_MAX_PACKET)
        .on_upgrade(move |socket| async move {
            let _permit = permit;
            tunnel_session(socket, state, session, authority).await
        })
        .into_response())
}

async fn tunnel_session(
    socket: axum::extract::ws::WebSocket,
    state: AppState,
    session_id: u64,
    mut authority: tokio::sync::watch::Receiver<()>,
) {
    use axum::extract::ws::Message;
    use futures_util::{SinkExt, StreamExt};
    if !state.0.handover.writes_allowed() || authority.has_changed().unwrap_or(true) {
        return;
    }
    let id = uuid::Uuid::new_v4().to_string();
    let (sender, mut receiver) = tokio::sync::mpsc::channel::<(u64, bool, Vec<u8>)>(32);
    {
        let mut sessions = lock(&state.0.voice);
        let Some(session) = sessions.get_mut(&session_id).filter(|s| s.alive_at(now())) else {
            return;
        };
        session.tunnel_sender = Some(sender);
        session.tunnel_id = Some(id.clone());
    }
    let (mut output, mut input) = socket.split();
    loop {
        tokio::select! {
            biased;
            _ = authority.changed() => break,
            packet = receiver.recv() => {
                let Some((sender_id, whisper, packet)) = packet else { break; };
                let allowed = {
                    let sessions = lock(&state.0.voice);
                    sessions.get(&session_id).filter(|s| s.alive_at(now()) && s.tunnel_id.as_deref() == Some(&id)).is_some_and(|recipient| sessions.get(&sender_id).filter(|s| s.alive_at(now())).is_some_and(|sender| sender_id == session_id || (!sender.state.muted && !recipient.state.deafened && (sender.channel_id == recipient.channel_id || (whisper && sender.whisper_channels.contains(&recipient.channel_id))) && sender.can_speak)))
                };
                if !allowed { continue; }
                if tokio::time::timeout(std::time::Duration::from_millis(100), output.send(Message::Binary(packet.into()))).await.map_or(true, |result| result.is_err()) { break; }
            },
            incoming = input.next() => {
                match incoming {
                    Some(Ok(Message::Binary(packet))) if packet.len() >= UDP_HEADER_LEN + 17 && packet.len() <= UDP_MAX_PACKET => {
                        if u64::from_le_bytes(packet[..8].try_into().unwrap()) != session_id { break; }
                        let source = {
                            let sessions = lock(&state.0.voice);
                            let Some(session) = sessions.get(&session_id).filter(|s| s.alive_at(now()) && s.tunnel_id.as_deref() == Some(&id)) else { break; };
                            session.remote.unwrap_or_else(|| "127.0.0.1:1".parse().unwrap())
                        };
                        let outgoing = relay_packet(&state, source, &packet);
                        let socket = lock(&state.0.voice_socket).clone();
                        if let Some(socket) = socket { deliver(&state, source, session_id, outgoing, &socket, true); }
                    },
                    Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => {},
                    _ => break,
                }
            },
            _ = state.shutdown_requested() => break,
            _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
                if !lock(&state.0.voice).get(&session_id).is_some_and(|s| s.alive_at(now()) && s.tunnel_id.as_deref() == Some(&id)) { break; }
            }
        }
    }
    if let Some(session) = lock(&state.0.voice)
        .get_mut(&session_id)
        .filter(|s| s.tunnel_id.as_deref() == Some(&id))
    {
        session.tunnel_sender = None;
        session.tunnel_id = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id;

    #[tokio::test]
    async fn candidate_path_probe_is_authenticated_and_never_moves_the_active_voice_binding() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::open(temp.path(), "path-probe", 0).unwrap();
        let active = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let candidate = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let receiver = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let address = socket.local_addr().unwrap();
        lock(&state.0.voice).insert(1, session(1, "room", active.local_addr().unwrap()));
        lock(&state.0.voice).insert(2, session(2, "room", receiver.local_addr().unwrap()));
        let task = tokio::spawn(run_udp(socket, state.clone()));
        let mut probe = vec![UDP_PATH_PROBE]; probe.extend(77u128.to_le_bytes());
        let good = encrypted(1, 1, [1; 32], &probe);
        let mut bad = good.clone(); *bad.last_mut().unwrap() ^= 1;
        candidate.send_to(&bad, address).await.unwrap();
        candidate.send_to(&good, address).await.unwrap();
        let mut bytes = [0; UDP_MAX_PACKET];
        let (n, from) = tokio::time::timeout(std::time::Duration::from_secs(1), candidate.recv_from(&mut bytes)).await.unwrap().unwrap();
        assert_eq!(from, address);
        let seq = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
        let clear = ChaCha20Poly1305::new_from_slice(&[1; 32]).unwrap().decrypt(
            Nonce::from_slice(&nonce(1, seq)), Payload { msg: &bytes[16..n], aad: &bytes[..16] }).unwrap();
        assert_eq!(clear, probe);
        assert_eq!(lock(&state.0.voice).get(&1).unwrap().remote, Some(active.local_addr().unwrap()));
        candidate.send_to(&good, address).await.unwrap();
        assert!(tokio::time::timeout(std::time::Duration::from_millis(60), candidate.recv_from(&mut bytes)).await.is_err(), "replay gets no echo");
        // The original path still delivers voice immediately after probing.
        let mut audio = vec![1]; audio.extend(0u64.to_le_bytes()); audio.extend([0xf8,0xff,0xfe]);
        active.send_to(&encrypted(1, 2, [1; 32], &audio), address).await.unwrap();
        let (n, _) = tokio::time::timeout(std::time::Duration::from_secs(1), receiver.recv_from(&mut bytes)).await.unwrap().unwrap();
        assert!(n > UDP_HEADER_LEN);
        assert_eq!(u64::from_le_bytes(bytes[..8].try_into().unwrap()), 2);
        state.request_shutdown(); task.abort(); let _ = task.await;
    }

    #[tokio::test]
    async fn voice_delivery_and_notices_do_not_wait_for_a_database_backup() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::open(temp.path(), "udp-cache", 0).unwrap();
        let first = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let second = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let address = server.local_addr().unwrap();
        lock(&state.0.voice).insert(1, session(1, "room", first.local_addr().unwrap()));
        lock(&state.0.voice).insert(2, session(2, "room", second.local_addr().unwrap()));
        let task = tokio::spawn(run_udp(server, state.clone()));
        tokio::task::yield_now().await;
        let db_state = state.clone();
        let (ready, wait) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _db = lock(&db_state.0.db);
            ready.send(()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(500));
        });
        wait.recv().unwrap();
        // A blocked refresh is coalesced and never runs in the receive loop.
        for _ in 0..100 {
            state.notify("members", None, None, None);
        }
        let mut payload = vec![1];
        payload.extend(0u64.to_le_bytes());
        payload.extend([0xf8, 0xff, 0xfe]);
        let mut bytes = [0; UDP_MAX_PACKET];
        let start = std::time::Instant::now();
        let result = tokio::time::timeout(std::time::Duration::from_millis(50), async {
            for sequence in 0..20 {
                first
                    .send_to(&encrypted(1, sequence, [1; 32], &payload), address)
                    .await
                    .unwrap();
                let (length, _) = second.recv_from(&mut bytes).await.unwrap();
                assert!(length > UDP_HEADER_LEN);
                assert_eq!(u64::from_le_bytes(bytes[..8].try_into().unwrap()), 2);
            }
        })
        .await;
        let elapsed = start.elapsed();
        holder.join().unwrap();
        task.abort();
        let _ = task.await;
        assert!(result.is_ok(), "Voice stopped behind SQLite: {elapsed:?}");
        println!("20 authenticated UDP deliveries while DB was locked: {elapsed:?}");
    }

    #[test]
    fn voice_flows_while_database_is_locked() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::open(temp.path(), "cache", 0).unwrap();
        let source = "127.0.0.1:11001".parse().unwrap();
        let destination = "127.0.0.1:11002".parse().unwrap();
        lock(&state.0.voice).insert(1, session(1, "room", source));
        lock(&state.0.voice).insert(2, session(2, "room", destination));
        let other = state.clone();
        let (ready, wait) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _db = lock(&other.0.db);
            ready.send(()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(500));
        });
        wait.recv().unwrap();
        let start = std::time::Instant::now();
        let mut payload = vec![1];
        payload.extend(0u64.to_le_bytes());
        payload.extend([0xf8, 0xff, 0xfe]);
        for sequence in 0..20 {
            assert_eq!(
                relay_packet(&state, source, &encrypted(1, sequence, [1; 32], &payload)).len(),
                1
            );
        }
        let elapsed = start.elapsed();
        holder.join().unwrap();
        assert!(
            elapsed < std::time::Duration::from_millis(50),
            "{elapsed:?}"
        );
    }

    #[tokio::test]
    async fn revoked_speaker_is_silenced_after_notice() {
        use crate::tests::{new_invite, request, signup};
        use serde_json::json;
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::open(temp.path(), "stage-cache", 0).unwrap();
        let app = crate::router(state.clone());
        let admin = signup(&app, "Admin", "").await;
        let member = signup(&app, "Member", &new_invite(&app, &admin).await).await;
        let (_, room) = request(
            &app,
            "POST",
            "/api/conversations",
            Some(&admin.token),
            json!({"name":"Talk","kind":"stage"}),
        )
        .await;
        let room = room["channel_id"].as_str().unwrap();
        let source = "127.0.0.1:11001".parse().unwrap();
        for (id, user, token, address) in [
            (1, &member.user, &member.token, source),
            (
                2,
                &admin.user,
                &admin.token,
                "127.0.0.1:11002".parse().unwrap(),
            ),
        ] {
            let mut ticket = session(id as u8, room, address);
            ticket.user = user.clone();
            ticket.token_hash = crate::session_hash(token);
            ticket.token_digest = crate::token_hash(token);
            lock(&state.0.voice).insert(id, ticket);
        }
        assert_eq!(
            request(
                &app,
                "PUT",
                &format!("/api/stages/{room}"),
                Some(&admin.token),
                json!({"user_id":member.user.id,"speaker":true})
            )
            .await
            .0,
            axum::http::StatusCode::OK
        );
        lock(&state.0.voice).get_mut(&1).unwrap().state.muted = false;
        let mut payload = vec![1];
        payload.extend(0u64.to_le_bytes());
        payload.extend([0xf8, 0xff, 0xfe]);
        assert_eq!(
            relay_packet(&state, source, &encrypted(1, 0, [1; 32], &payload)).len(),
            1
        );
        assert_eq!(
            request(
                &app,
                "PUT",
                &format!("/api/stages/{room}"),
                Some(&admin.token),
                json!({"user_id":member.user.id,"speaker":false})
            )
            .await
            .0,
            axum::http::StatusCode::OK
        );
        assert!(relay_packet(&state, source, &encrypted(1, 1, [1; 32], &payload)).is_empty());
        assert!(!lock(&state.0.voice).get(&1).unwrap().can_speak);
    }

    #[test]
    fn tailscale_udp_source_filter_keeps_loopback_and_rejects_lan_and_public_sources() {
        for source in [
            "127.0.0.1",
            "127.0.0.2",
            "::1",
            "100.64.0.1",
            "100.127.255.254",
        ] {
            assert!(
                is_tailscale_voice_source(source.parse().unwrap()),
                "{source}"
            );
        }
        for source in [
            "0.0.0.0",
            "100.63.255.255",
            "100.128.0.0",
            "192.168.1.2",
            "10.0.0.2",
            "172.16.0.2",
            "8.8.8.8",
            "169.254.1.2",
            "224.0.0.1",
            "::",
            "fd00::2",
            "2001:db8::2",
            "::ffff:100.64.0.1",
        ] {
            assert!(
                !is_tailscale_voice_source(source.parse().unwrap()),
                "{source}"
            );
        }
    }

    #[tokio::test]
    async fn direct_admission_requires_two_opted_in_authorized_peers_and_fresh_authenticated_address(
    ) {
        use crate::tests::{create_test_channel, new_invite, signup};
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::open(temp.path(), "direct", 0).unwrap();
        let app = crate::router(state.clone());
        let admin = signup(&app, "Admin", "").await;
        let member = signup(&app, "Member", &new_invite(&app, &admin).await).await;
        let room = create_test_channel(&app, &admin, "voice", false).await;
        let first: SocketAddr = "127.0.0.1:12101".parse().unwrap();
        let second: SocketAddr = "127.0.0.1:12102".parse().unwrap();
        let mut a = session(1, &room, first);
        a.user = admin.user.clone();
        let mut b = session(2, &room, second);
        b.user = member.user.clone();
        lock(&state.0.voice).insert(1, a);
        lock(&state.0.voice).insert(2, b);
        let roster = |id: u64, sequence: u64, enabled: bool| {
            let key = [id as u8; 32];
            let address = if id == 1 { first } else { second };
            let output = relay_packet(
                &state,
                address,
                &encrypted(
                    id,
                    sequence,
                    key,
                    &direct_voice::request(sequence, enabled, 10_000),
                ),
            );
            assert_eq!(output.len(), 1);
            let packet = &output[0].1;
            let seq = u64::from_le_bytes(packet[8..16].try_into().unwrap());
            let clear = ChaCha20Poly1305::new_from_slice(&key)
                .unwrap()
                .decrypt(
                    Nonce::from_slice(&nonce(1, seq)),
                    Payload {
                        msg: &packet[16..],
                        aad: &packet[..16],
                    },
                )
                .unwrap();
            assert_eq!(clear[0], direct_voice::ROSTER);
            serde_json::from_slice::<Roster>(&clear[1..]).unwrap()
        };
        assert!(roster(1, 0, true).peer.is_none());
        let b_peer = roster(2, 0, true).peer.unwrap();
        let a_peer = roster(1, 1, true).peer.unwrap();
        assert_eq!(a_peer.address, second);
        assert_eq!(b_peer.address, first);
        assert_eq!(a_peer.key, b_peer.key);
        assert!(a_peer.send_allowed && a_peer.receive_allowed);
        assert!(
            relay_packet(
                &state,
                second,
                &encrypted(1, 2, [1; 32], &direct_voice::request(2, true, 10_000))
            )
            .is_empty(),
            "request cannot redirect the authenticated NAT binding"
        );
        assert!(
            relay_packet(
                &state,
                first,
                &encrypted(1, 1, [1; 32], &direct_voice::request(1, true, 10_000))
            )
            .is_empty(),
            "replay rejected"
        );
        lock(&state.0.voice).get_mut(&1).unwrap().state.muted = true;
        assert!(!roster(1, 2, true).peer.unwrap().send_allowed);
        lock(&state.0.voice).get_mut(&2).unwrap().state.deafened = true;
        assert!(!roster(1, 3, true).peer.unwrap().send_allowed);
        let generation = lock(&state.0.voice).get(&2).unwrap().direct.generation;
        assert!(roster(2, 1, false).peer.is_none());
        assert!(roster(1, 4, true).peer.is_none());
        roster(2, 2, true);
        assert_ne!(
            generation,
            lock(&state.0.voice).get(&2).unwrap().direct.generation
        );
        lock(&state.0.voice).get_mut(&2).unwrap().direct.seen =
            Some(std::time::Instant::now() - std::time::Duration::from_millis(501));
        assert!(roster(1, 5, true).peer.is_none());
        roster(2, 3, true);
        lock(&state.0.voice).insert(3, session(3, &room, "127.0.0.1:12103".parse().unwrap()));
        assert!(roster(1, 6, true).peer.is_none(), "groups stay on relay");
        lock(&state.0.voice).remove(&3);
        lock(&state.0.voice).get_mut(&2).unwrap().channel_id = "other-room".into();
        assert!(roster(1, 7, true).peer.is_none());
        lock(&state.0.voice).remove(&1);
        assert!(
            relay_packet(
                &state,
                first,
                &encrypted(1, 8, [1; 32], &direct_voice::request(8, true, 10_000))
            )
            .is_empty(),
            "revoked session cannot get a lease"
        );
    }

    fn private_fixture(user: &str) -> Vec<u8> {
        let mut payload = vec![AUDIO_PACKET_V3];
        payload.extend(99u64.to_le_bytes());
        payload.extend(1u64.to_le_bytes());
        payload.extend(480u64.to_le_bytes());
        payload.push(1);
        payload.extend(1u64.to_le_bytes());
        payload.push(user.len() as u8);
        payload.push(6);
        payload.extend([42; 16]);
        payload.extend(1u64.to_le_bytes());
        payload.extend(user.as_bytes());
        payload.extend(b"device");
        payload.extend([93; 20]);
        payload
    }
    #[test]
    fn whisper_routes_only_selected_room_peer_and_preserves_private_ciphertext() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::open(temp.path(), "whisper", 0).unwrap();
        let source = "127.0.0.1:11001".parse().unwrap();
        let sender = session(1, "room", source);
        let payload = private_fixture(&sender.user.id);
        let recipient = session(2, "room", "127.0.0.1:11002".parse().unwrap());
        let target = recipient.user.id.clone();
        lock(&state.0.voice).insert(1, sender);
        lock(&state.0.voice).insert(2, recipient);
        lock(&state.0.voice).insert(3, session(3, "room", "127.0.0.1:11003".parse().unwrap()));
        let private = relay_protocol::wrap_whisper(&target, &payload).unwrap();
        let output = relay_packet(&state, source, &encrypted(1, 0, [1; 32], &private));
        assert_eq!(output.len(), 1);
        assert_eq!(
            output[0].0,
            "127.0.0.1:11002".parse::<SocketAddr>().unwrap()
        );
        let packet = &output[0].1;
        let sequence = u64::from_le_bytes(packet[8..16].try_into().unwrap());
        let clear = ChaCha20Poly1305::new_from_slice(&[2; 32])
            .unwrap()
            .decrypt(
                Nonce::from_slice(&nonce(1, sequence)),
                Payload {
                    msg: &packet[16..],
                    aad: &packet[..16],
                },
            )
            .unwrap();
        let (_, inner) = relay_protocol::unwrap_whisper(&clear).unwrap();
        assert_eq!(&inner[9..], &payload[9..]);
        assert!(relay_packet(&state, source, &encrypted(1, 0, [1; 32], &private)).is_empty());
        lock(&state.0.voice).get_mut(&2).unwrap().state.deafened = true;
        assert!(relay_packet(&state, source, &encrypted(1, 1, [1; 32], &private)).is_empty());
    }

    #[tokio::test]
    async fn cross_room_whisper_requires_current_destination_join_permission() {
        use crate::tests::{create_test_channel, new_invite, request, signup};
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::open(temp.path(), "cross-room", 0).unwrap();
        let app = crate::router(state.clone());
        let admin = signup(&app, "Admin", "").await;
        let member = signup(&app, "Member", &new_invite(&app, &admin).await).await;
        let source_room = create_test_channel(&app, &admin, "voice", false).await;
        let destination_room = create_test_channel(&app, &admin, "voice", true).await;
        let source = "127.0.0.1:12001".parse().unwrap();
        let mut sender = session(1, &source_room, source);
        sender.user = member.user.clone();
        sender.token_hash = crate::session_hash(&member.token);
        sender.token_digest = crate::token_hash(&member.token);
        let mut recipient = session(2, &destination_room, "127.0.0.1:12002".parse().unwrap());
        recipient.user = admin.user.clone();
        recipient.token_hash = crate::session_hash(&admin.token);
        recipient.token_digest = crate::token_hash(&admin.token);
        let inner = private_fixture(&member.user.id);
        let payload = relay_protocol::wrap_whisper(&admin.user.id, &inner).unwrap();
        lock(&state.0.voice).insert(1, sender);
        lock(&state.0.voice).insert(2, recipient);
        assert!(relay_packet(&state, source, &encrypted(1, 0, [1; 32], &payload)).is_empty());
        request(&app, "PUT", &format!("/api/channels/{destination_room}/permissions"), Some(&admin.token), serde_json::json!({"entries":[{"subject_kind":"user","subject_id":member.user.id,"read":true,"send":false,"join":true,"stream":false}]})).await;
        assert_eq!(
            relay_packet(&state, source, &encrypted(1, 1, [1; 32], &payload)).len(),
            1
        );
        lock(&state.0.db).execute("DELETE FROM channel_permissions WHERE channel_id=?1 AND subject_kind='user' AND subject_id=?2", rusqlite::params![destination_room, member.user.id]).unwrap();
        state.enforce_voice_permissions().unwrap();
        assert!(relay_packet(&state, source, &encrypted(1, 2, [1; 32], &payload)).is_empty());
        assert!(
            relay_packet(&state, source, &encrypted(1, 3, [1; 32], &inner)).is_empty(),
            "ordinary room audio cannot cross rooms"
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn actual_tcp_and_udp_share_authentication_replay_and_nonce_counters() {
        use base64::Engine;
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::open(temp.path(), "tunnel", 0).unwrap();
        let udp1 = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let udp2 = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server_udp = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let udp_address = server_udp.local_addr().unwrap();
        let sender = session(1, "room", udp1.local_addr().unwrap());
        let payload = private_fixture(&sender.user.id);
        lock(&state.0.voice).insert(1, sender);
        lock(&state.0.voice).insert(2, session(2, "room", udp2.local_addr().unwrap()));
        let udp_state = state.clone();
        let udp_owner = tokio::spawn(run_udp(server_udp, udp_state));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = crate::router(state.clone());
        let http_owner = tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });
        let request = |id: u64, key: u8| {
            let mut request = format!("ws://{address}/api/voice/tunnel")
                .into_client_request()
                .unwrap();
            request
                .headers_mut()
                .insert("x-relay-voice-session", id.to_string().parse().unwrap());
            request.headers_mut().insert(
                "x-relay-voice-key",
                base64::engine::general_purpose::STANDARD
                    .encode([key; 32])
                    .parse()
                    .unwrap(),
            );
            request
        };
        assert!(tokio_tungstenite::connect_async(request(1, 9))
            .await
            .is_err());
        let (mut tcp1, _) = tokio_tungstenite::connect_async(request(1, 1))
            .await
            .unwrap();
        tcp1.send(Message::Binary(encrypted(1, 0, [1; 32], &[0]).into()))
            .await
            .unwrap();
        let packet = encrypted(1, 1, [1; 32], &payload);
        tcp1.send(Message::Binary(packet.clone().into()))
            .await
            .unwrap();
        let mut bytes = [0u8; UDP_MAX_PACKET];
        let (size, _) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            udp2.recv_from(&mut bytes),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(size > 16);
        let first_nonce = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
        udp1.send_to(&packet, udp_address).await.unwrap();
        assert!(tokio::time::timeout(
            std::time::Duration::from_millis(80),
            udp2.recv_from(&mut bytes)
        )
        .await
        .is_err());
        udp1.send_to(&encrypted(1, 2, [1; 32], &payload), udp_address)
            .await
            .unwrap();
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            udp2.recv_from(&mut bytes),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(u64::from_le_bytes(bytes[8..16].try_into().unwrap()) > first_nonce);
        let (mut tcp2, _) = tokio_tungstenite::connect_async(request(2, 2))
            .await
            .unwrap();
        let until = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while lock(&state.0.voice)[&2].tunnel_sender.is_none() && std::time::Instant::now() < until
        {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        udp1.send_to(&encrypted(1, 3, [1; 32], &payload), udp_address)
            .await
            .unwrap();
        let received = tokio::time::timeout(std::time::Duration::from_secs(2), tcp2.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(matches!(received, Message::Binary(_)));
        assert_eq!(lock(&state.0.voice)[&1].in_sequence, Some(3));
        lock(&state.0.voice).remove(&1);
        assert!(tokio_tungstenite::connect_async(request(1, 1))
            .await
            .is_err());
        udp_owner.abort();
        http_owner.abort();
        let _ = udp_owner.await;
        let _ = http_owner.await;
    }
    #[test]
    fn encrypted_probe_echoes_only_authenticated_source_with_unique_directional_nonce() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::open(temp.path(), "test", 0).unwrap();
        let source = "127.0.0.1:11001".parse().unwrap();
        let other = "127.0.0.1:11002".parse().unwrap();
        let mut sender = session(1, "voice", source);
        sender.state.muted = true;
        sender.state.deafened = true;
        lock(&state.0.voice).insert(1, sender);
        lock(&state.0.voice).insert(2, session(2, "voice", other));
        let mut probe = vec![UDP_PING];
        probe.extend(44_u64.to_le_bytes());
        let first = relay_packet(&state, source, &encrypted(1, 1, [1; 32], &probe));
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].0, source);
        let sequence = u64::from_le_bytes(first[0].1[8..16].try_into().unwrap());
        let opened = ChaCha20Poly1305::new_from_slice(&[1; 32])
            .unwrap()
            .decrypt(
                Nonce::from_slice(&nonce(1, sequence)),
                Payload {
                    msg: &first[0].1[16..],
                    aad: &first[0].1[..16],
                },
            )
            .unwrap();
        assert_eq!(opened, probe);
        assert!(relay_packet(&state, source, &encrypted(1, 1, [1; 32], &probe)).is_empty());
        assert!(relay_packet(&state, other, &encrypted(1, 2, [1; 32], &probe)).is_empty());
        let second = relay_packet(&state, source, &encrypted(1, 2, [1; 32], &probe));
        assert_eq!(second.len(), 1);
        assert!(u64::from_le_bytes(second[0].1[8..16].try_into().unwrap()) > sequence);
        lock(&state.0.voice).remove(&1);
        assert!(relay_packet(&state, source, &encrypted(1, 3, [1; 32], &probe)).is_empty());
    }

    #[test]
    fn authenticated_replay_window_accepts_reordering_but_not_duplicate_or_old_packets() {
        let mut window = ReplayWindow::default();
        assert!(window.accept(20));
        assert!(window.accept(18));
        assert!(window.accept(19));
        assert!(!window.accept(18));
        assert!(window.accept(200));
        assert!(!window.accept(20));
        assert!(window.accept(199));
        assert!(window.accept(u64::MAX));
        assert!(!window.accept(u64::MAX));
    }

    #[test]
    fn timed_voice_preserves_source_clock_and_reordering_without_trusting_sender_identity() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::open(temp.path(), "test", 0).unwrap();
        let source = "127.0.0.1:11001".parse().unwrap();
        let destination = "127.0.0.1:11002".parse().unwrap();
        lock(&state.0.voice).insert(1, session(1, "voice", source));
        lock(&state.0.voice).insert(2, session(2, "voice", destination));
        let mut payload = vec![AUDIO_PACKET_V2];
        payload.extend(999u64.to_le_bytes());
        payload.extend(5u64.to_le_bytes());
        payload.extend(3840u64.to_le_bytes());
        payload.push(1);
        payload.extend([0xf8, 0xff, 0xfe]);
        for sequence in [10, 8, 9] {
            let output = relay_packet(&state, source, &encrypted(1, sequence, [1; 32], &payload));
            assert_eq!(output.len(), 1);
            let transport_sequence = u64::from_le_bytes(output[0].1[8..16].try_into().unwrap());
            let plain = ChaCha20Poly1305::new_from_slice(&[2; 32])
                .unwrap()
                .decrypt(
                    Nonce::from_slice(&nonce(1, transport_sequence)),
                    Payload {
                        msg: &output[0].1[16..],
                        aad: &output[0].1[..16],
                    },
                )
                .unwrap();
            assert_eq!(u64::from_le_bytes(plain[1..9].try_into().unwrap()), 1);
            assert_eq!(&plain[9..], &payload[9..]);
        }
        assert!(relay_packet(&state, source, &encrypted(1, 9, [1; 32], &payload)).is_empty());
        payload[25] = 2;
        assert!(relay_packet(&state, source, &encrypted(1, 11, [1; 32], &payload)).is_empty());
        assert_eq!(lock(&state.0.voice)[&1].in_sequence, Some(10));
    }

    #[test]
    fn opaque_voice_preserves_inner_envelope_and_rejects_forged_account_nonce_and_bounds() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::open(temp.path(), "opaque", 0).unwrap();
        let source: SocketAddr = "127.0.0.1:51001".parse().unwrap();
        let receiver: SocketAddr = "127.0.0.1:51002".parse().unwrap();
        let sender = session(1, "room", source);
        let user = sender.user.id.clone();
        lock(&state.0.voice).insert(1, sender);
        lock(&state.0.voice).insert(2, session(2, "room", receiver));
        lock(&state.0.voice).insert(
            3,
            session(3, "elsewhere", "127.0.0.1:51003".parse().unwrap()),
        );
        let media_sequence = 17u64;
        let mut payload = vec![AUDIO_PACKET_V3];
        payload.extend(999u64.to_le_bytes());
        payload.extend(media_sequence.to_le_bytes());
        payload.extend(16320u64.to_le_bytes());
        payload.push(1);
        payload.extend(5u64.to_le_bytes());
        payload.push(user.len() as u8);
        payload.push(6);
        payload.extend([42; 16]);
        payload.extend(media_sequence.to_le_bytes());
        payload.extend(user.as_bytes());
        payload.extend(b"device");
        payload.extend([93; 20]); // Opaque ciphertext and tag, never an Opus frame on the server.
        let output = relay_packet(&state, source, &encrypted(1, 0, [1; 32], &payload));
        assert_eq!(output.len(), 1);
        assert_eq!(output[0].0, receiver);
        let packet = &output[0].1;
        let cipher = ChaCha20Poly1305::new_from_slice(&[2; 32]).unwrap();
        let outer_sequence = u64::from_le_bytes(packet[8..16].try_into().unwrap());
        let plain = cipher
            .decrypt(
                Nonce::from_slice(&nonce(1, outer_sequence)),
                Payload {
                    msg: &packet[16..],
                    aad: &packet[..16],
                },
            )
            .unwrap();
        assert_eq!(&plain[1..9], &1u64.to_le_bytes());
        assert_eq!(
            &plain[9..],
            &payload[9..],
            "the inner authenticated envelope remains byte-identical"
        );
        assert!(relay_packet(&state, source, &encrypted(1, 0, [1; 32], &payload)).is_empty());
        lock(&state.0.voice).get_mut(&1).unwrap().device_id = Some("different-device".into());
        assert!(
            relay_packet(&state, source, &encrypted(1, 1, [1; 32], &payload)).is_empty(),
            "an opaque device claim must match the authenticated selected device"
        );
        lock(&state.0.voice).get_mut(&1).unwrap().device_id = Some("device".into());
        for mutate in [25, 52, 60, 60 + user.len()] {
            let mut invalid = payload.clone();
            invalid[mutate] = if mutate == 25 { 2 } else { 0xff };
            assert!(relay_packet(&state, source, &encrypted(1, 1, [1; 32], &invalid)).is_empty());
        }
        let mut no_device = payload.clone();
        no_device[35] = 0;
        assert!(relay_packet(&state, source, &encrypted(1, 1, [1; 32], &no_device)).is_empty());
        assert!(relay_packet(&state, source, &encrypted(1, 1, [1; 32], &payload[..60])).is_empty());
        assert!(relay_packet(
            &state,
            "127.0.0.1:51099".parse().unwrap(),
            &encrypted(1, 1, [1; 32], &payload)
        )
        .is_empty());
        lock(&state.0.voice).get_mut(&1).unwrap().state.muted = true;
        assert!(relay_packet(&state, source, &encrypted(1, 1, [1; 32], &payload)).is_empty());
        lock(&state.0.voice).get_mut(&1).unwrap().state.muted = false;
        lock(&state.0.voice).get_mut(&2).unwrap().state.deafened = true;
        state.enforce_voice_permissions().unwrap();
        assert!(relay_packet(&state, source, &encrypted(1, 2, [1; 32], &payload)).is_empty());
    }

    fn session(key: u8, channel: &str, remote: SocketAddr) -> VoiceSession {
        VoiceSession {
            private_call_id: None,
            recovery_deadline: None,
            user: User {
                id: id(),
                username: "test".into(),
                role: "member".into(),
                profile: Default::default(),
            },
            channel_id: channel.into(),
            device_id: None,
            token_hash: "test".into(),
            token_digest: "test".into(),
            key: [key; 32],
            state: VoiceState::default(),
            remote: Some(remote),
            tunnel_sender: None,
            tunnel_id: None,
            in_sequence: None,
            replay: ReplayWindow::default(),
            out_sequence: 0,
            last_seen: now(),
            expires: now() + 60,
            direct: Default::default(),
            can_speak: true,
            can_stream: true,
            whisper_channels: Default::default(),
        }
    }
    fn encrypted(id: u64, sequence: u64, key: [u8; 32], payload: &[u8]) -> Vec<u8> {
        let mut header = id.to_le_bytes().to_vec();
        header.extend(sequence.to_le_bytes());
        let encrypted = ChaCha20Poly1305::new_from_slice(&key)
            .unwrap()
            .encrypt(
                Nonce::from_slice(&nonce(0, sequence)),
                Payload {
                    msg: payload,
                    aad: &header,
                },
            )
            .unwrap();
        header.extend(encrypted);
        header
    }
    #[test]
    fn relay_authenticates_identity_replay_and_channel_audience() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::open(temp.path(), "test", 0).unwrap();
        let source: SocketAddr = "127.0.0.1:11001".parse().unwrap();
        let destination: SocketAddr = "127.0.0.1:11002".parse().unwrap();
        let other: SocketAddr = "127.0.0.1:11003".parse().unwrap();
        lock(&state.0.voice).insert(1, session(1, "voice-a", source));
        lock(&state.0.voice).insert(2, session(2, "voice-a", destination));
        lock(&state.0.voice).insert(3, session(3, "voice-b", other));
        let mut payload = vec![1];
        payload.extend(999u64.to_le_bytes());
        payload.extend([0xf8, 0xff, 0xfe]);
        let packet = encrypted(1, 0, [1; 32], &payload);
        let output = relay_packet(&state, source, &packet);
        assert_eq!(output.len(), 1);
        assert_eq!(output[0].0, destination);
        let bytes = &output[0].1;
        let seq = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
        let plaintext = ChaCha20Poly1305::new_from_slice(&[2; 32])
            .unwrap()
            .decrypt(
                Nonce::from_slice(&nonce(1, seq)),
                Payload {
                    msg: &bytes[16..],
                    aad: &bytes[..16],
                },
            )
            .unwrap();
        assert_eq!(u64::from_le_bytes(plaintext[1..9].try_into().unwrap()), 1);
        assert_eq!(&plaintext[9..], &payload[9..]);
        assert!(relay_packet(&state, source, &packet).is_empty());
        let mut corrupted = encrypted(1, 1, [1; 32], &payload);
        *corrupted.last_mut().unwrap() ^= 1;
        assert!(relay_packet(&state, source, &corrupted).is_empty());
        assert_eq!(
            relay_packet(&state, source, &encrypted(1, 1, [1; 32], &payload)).len(),
            1,
            "invalid packet must not burn sequence"
        );
        lock(&state.0.voice).get_mut(&1).unwrap().state.muted = true;
        assert!(relay_packet(&state, source, &encrypted(1, 2, [1; 32], &payload)).is_empty());
        lock(&state.0.voice).get_mut(&1).unwrap().state.muted = false;
        lock(&state.0.voice).get_mut(&2).unwrap().state.deafened = true;
        assert!(relay_packet(&state, source, &encrypted(1, 3, [1; 32], &payload)).is_empty());
    }
    #[test]
    fn keepalive_requires_authentication_before_address_change() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::open(temp.path(), "test", 0).unwrap();
        let old = "127.0.0.1:1".parse().unwrap();
        let new = "127.0.0.1:2".parse().unwrap();
        lock(&state.0.voice).insert(1, session(1, "voice", old));
        relay_packet(&state, new, &encrypted(1, 0, [2; 32], &[0]));
        assert_eq!(lock(&state.0.voice)[&1].remote, Some(old));
        relay_packet(&state, new, &encrypted(1, 0, [1; 32], &[0]));
        assert_eq!(lock(&state.0.voice)[&1].remote, Some(new));
    }
    #[test]
    fn expired_idle_tickets_cannot_revive_and_output_nonces_never_wrap() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::open(temp.path(), "test", 0).unwrap();
        let source = "127.0.0.1:11001".parse().unwrap();
        let destination = "127.0.0.1:11002".parse().unwrap();
        let mut expired = session(1, "voice", source);
        expired.last_seen = now() - 60;
        lock(&state.0.voice).insert(1, expired);
        assert!(relay_packet(&state, source, &encrypted(1, 0, [1; 32], &[0])).is_empty());
        assert_eq!(
            lock(&state.0.voice)[&1].in_sequence,
            None,
            "idle keepalive must not revive ticket before cleanup"
        );
        let mut expired = session(1, "voice", source);
        expired.expires = now();
        lock(&state.0.voice).insert(1, expired);
        relay_packet(&state, source, &encrypted(1, 0, [1; 32], &[0]));
        assert_eq!(lock(&state.0.voice)[&1].in_sequence, None);
        lock(&state.0.voice).insert(1, session(1, "voice", source));
        let mut recipient = session(2, "voice", destination);
        recipient.out_sequence = u64::MAX - 1;
        lock(&state.0.voice).insert(2, recipient);
        let mut payload = vec![1];
        payload.extend(1u64.to_le_bytes());
        payload.push(0xf8);
        assert_eq!(
            relay_packet(&state, source, &encrypted(1, 0, [1; 32], &payload)).len(),
            1
        );
        assert_eq!(lock(&state.0.voice)[&2].out_sequence, u64::MAX);
        assert!(relay_packet(&state, source, &encrypted(1, 1, [1; 32], &payload)).is_empty());
        assert_eq!(lock(&state.0.voice)[&2].out_sequence, u64::MAX);
    }
}
