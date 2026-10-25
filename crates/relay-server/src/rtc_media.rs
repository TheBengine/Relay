//! Fixed-destination native WebRTC media over authenticated HTTPS WebSockets.
//! No address is accepted from a peer. Admission, revocation and handover use
//! the same room/session checks as signaling; this does not change call E2EE.
use super::*;
#[cfg(test)]
use serde_json::json;
use serde_json::Value;
use tokio::net::UdpSocket;

const MAX_PACKET: usize = 1600;
const IDLE: Duration = Duration::from_secs(90);

pub(super) async fn upgrade(
    State(state): State<AppState>,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> ApiResult<Response> {
    if uri.query().is_some()
        || headers.contains_key(header::ORIGIN)
        || headers.contains_key("sec-fetch-site")
    {
        return Err(forbidden());
    }
    let config = state.0.config.rtc.as_ref().ok_or_else(unavailable)?;
    let port = config.media_port.ok_or_else(unavailable)?;
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(unauthorized)?;
    let claims = config.verify(token)?;
    rtc::validate_media_claims(&state, &claims)?;
    let user = claims["sub"].as_str().ok_or_else(unauthorized)?;
    let permit = state
        .0
        .rtc_media_connections
        .try_acquire(user)
        .ok_or_else(|| {
            ApiError::cooldown(
                "service_busy",
                "Too many media connections; close another call and retry",
                1000,
                None,
            )
        })?;
    let socket = UdpSocket::bind("127.0.0.1:0")
        .await
        .map_err(|_| unavailable())?;
    socket
        .connect((std::net::Ipv4Addr::LOCALHOST, port))
        .await
        .map_err(|_| unavailable())?;
    let authority = state.0.handover.watch_transports();
    if *state.0.shutdown.borrow() || !state.0.handover.writes_allowed() {
        return Err(unavailable());
    }
    Ok(ws
        .max_frame_size(MAX_PACKET + 2)
        .max_message_size(MAX_PACKET + 2)
        .on_upgrade(move |ws| async move {
            let _permit = permit;
            pump(state, claims, ws, socket, authority).await
        }))
}
fn unavailable() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "The secure media route is unavailable",
    )
}

async fn pump(
    state: AppState,
    claims: Value,
    ws: WebSocket,
    udp: UdpSocket,
    mut authority: tokio::sync::watch::Receiver<()>,
) {
    let stopped = state.clone();
    let active = state.clone();
    let checking = state.clone();
    forward_packets(
        ws,
        udp,
        move || {
            let state = checking.clone();
            let claims = claims.clone();
            async move {
                let checked = state.clone();
                let job = tokio::task::spawn_blocking(move || {
                    rtc::validate_media_claims(&checked, &claims)
                });
                rtc::wait_for_validation(&state, job).await
            }
        },
        move || active.0.handover.writes_allowed() && !*active.0.shutdown.borrow(),
        |_| {},
        async move {
            tokio::select! {_=authority.changed()=>{},_=stopped.shutdown_requested()=>{}}
        },
    )
    .await;
}

/// The same bounded fixed-destination framing serves calls and isolated
/// preflight. Each caller retains its own admission, revocation and lifetime.
pub(super) async fn forward_packets<C, CF, A, N, S>(
    mut ws: WebSocket,
    udp: UdpSocket,
    validate: C,
    active: A,
    notice: N,
    stopped: S,
) where
    C: Fn() -> CF + Send + Sync,
    CF: std::future::Future<Output = bool> + Send,
    A: Fn() -> bool + Send + Sync,
    N: Fn(bool) + Send + Sync,
    S: std::future::Future<Output = ()> + Send,
{
    tokio::pin!(stopped);
    let mut packet = [0u8; MAX_PACKET + 1];
    let mut check = tokio::time::interval(Duration::from_secs(1));
    let mut last = std::time::Instant::now();
    loop {
        if !active() {
            break;
        }
        tokio::select! {
            biased;
            _=&mut stopped=>break,
            next=ws.recv()=>{
                let Some(Ok(next))=next else{break;};
                match next {
                    WsMessage::Binary(frame)=>{
                        if frame.len()<3 || frame.len()>MAX_PACKET+2
                            || u16::from_be_bytes([frame[0],frame[1]]) as usize!=frame.len()-2 || !active(){break;}
                        if !matches!(tokio::time::timeout(Duration::from_secs(2),udp.send(&frame[2..])).await,Ok(Ok(_))){break;}
                        notice(true);last=std::time::Instant::now();
                    }
                    WsMessage::Ping(bytes)=>{if !matches!(tokio::time::timeout(Duration::from_secs(2),ws.send(WsMessage::Pong(bytes))).await,Ok(Ok(()))){break;}}
                    WsMessage::Pong(_)=>{},_=>break,
                }
            }
            received=udp.recv(&mut packet)=>{
                let Ok(n)=received else{break;};if n==0||n>MAX_PACKET{break;}
                let mut frame=Vec::with_capacity(n+2);frame.extend_from_slice(&(n as u16).to_be_bytes());frame.extend_from_slice(&packet[..n]);
                if !matches!(tokio::time::timeout(Duration::from_secs(3),ws.send(WsMessage::Binary(frame.into()))).await,Ok(Ok(()))){break;}
                notice(false);last=std::time::Instant::now();
            }
            _=check.tick()=>{
                if last.elapsed()>IDLE{break;}
                let valid=tokio::select!{biased;_=&mut stopped=>false,
                    result=tokio::time::timeout(Duration::from_secs(3),validate())=>matches!(result,Ok(true))};
                if !valid{break;}
            }
        }
    }
    let _ = tokio::time::timeout(Duration::from_secs(1), ws.send(WsMessage::Close(None))).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{request, signup};
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};

    #[tokio::test]
    async fn fixed_media_roundtrip_requires_the_room_ticket_and_stops_after_voice_revocation() {
        let private=format!("ws://{}.iroh:4580/","a".repeat(64));
        for public in [private.as_str(),"ws://untrusted.example/","ws://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.iroh:4590/"] {
            let configured=RtcConfig::new(public.to_string(),"http://127.0.0.1:9".into(),"owned".into(),"owned-secret-0123456789".into()).unwrap().with_media_bridge(7882);
            assert_eq!(configured.is_ok(),public==private,"only the authenticated private namespace may use loopback WS");
        }
        let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = udp.local_addr().unwrap().port();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let at = listener.local_addr().unwrap();
        let config = RtcConfig::new(
            format!("ws://{at}"),
            "http://127.0.0.1:9".into(),
            "owned-sfu".into(),
            "owned-sfu-secret-not-a-user-key".into(),
        )
        .unwrap()
        .with_media_bridge(port)
        .unwrap();
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::open_with_config(
            temp.path(),
            "Media",
            0,
            ServerConfig {
                rtc: Some(config),
                ..Default::default()
            },
        )
        .unwrap();
        let app = router(state.clone());
        let user = signup(&app, "Owner", "").await;
        let (code, channel) = request(
            &app,
            "POST",
            "/api/channels",
            Some(&user.token),
            json!({"name":"voice","kind":"voice","private":false}),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{channel}");
        let channel = channel["id"].as_str().unwrap();
        assert_eq!(
            request(
                &app,
                "POST",
                "/api/voice/join",
                Some(&user.token),
                json!({"channel_id":channel})
            )
            .await
            .0,
            StatusCode::OK
        );
        let (code, ticket) = request(
            &app,
            "POST",
            "/api/rtc/token",
            Some(&user.token),
            json!({"channel_id":channel}),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{ticket}");
        assert_eq!(ticket["media_bridge"]["peer_port"], port);
        let token = ticket["media_bridge"]["token"].as_str().unwrap();
        assert!(
            request(&app, "GET", "/api/rtc/authorize", Some(token), Value::Null)
                .await
                .0
                .is_client_error(),
            "media-only access cannot join signaling"
        );
        let config = state.0.config.rtc.as_ref().unwrap();
        let claims = config.verify(token).unwrap();
        assert!(claims["exp"].as_u64().unwrap() > now() + 60);
        assert!(claims["exp"].as_u64().unwrap() <= now() + VOICE_LIFETIME);
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
        });
        struct Serving(tokio::task::JoinHandle<std::io::Result<()>>);
        impl Drop for Serving {
            fn drop(&mut self) {
                self.0.abort();
            }
        }
        let _task = Serving(task);
        let endpoint = format!("ws://{at}/api/rtc/media");
        let make = |bearer: &str| {
            let mut r = endpoint.clone().into_client_request().unwrap();
            r.headers_mut().insert(
                header::AUTHORIZATION,
                format!("Bearer {bearer}").parse().unwrap(),
            );
            r
        };
        assert!(
            tokio_tungstenite::connect_async(make(&user.token))
                .await
                .is_err(),
            "account bearer is not an SFU ticket"
        );
        assert!(
            tokio_tungstenite::connect_async(make(ticket["token"].as_str().unwrap()))
                .await
                .is_err(),
            "signaling admission cannot replace a media capability"
        );
        let mut browser = make(token);
        browser
            .headers_mut()
            .insert(header::ORIGIN, "https://unrelated.example".parse().unwrap());
        assert!(tokio_tungstenite::connect_async(browser).await.is_err());
        let (mut ws, _) = tokio_tungstenite::connect_async(make(token)).await.unwrap();
        let mut extra = Vec::new();
        for _ in 0..7 {
            extra.push(
                tokio_tungstenite::connect_async(make(token))
                    .await
                    .unwrap()
                    .0,
            );
        }
        assert!(
            tokio_tungstenite::connect_async(make(token)).await.is_err(),
            "per-user media allocation bound"
        );
        for mut socket in extra {
            let _ = socket.close(None).await;
        }
        let payload = vec![0x81; MAX_PACKET];
        let mut frame = (payload.len() as u16).to_be_bytes().to_vec();
        frame.extend_from_slice(&payload);
        ws.send(Message::Binary(frame.clone().into()))
            .await
            .unwrap();
        let mut buffer = [0u8; MAX_PACKET + 1];
        let (n, from) = tokio::time::timeout(Duration::from_secs(2), udp.recv_from(&mut buffer))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&buffer[..n], &payload);
        assert!(from.ip().is_loopback());
        udp.send_to(&payload, from).await.unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), ws.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            Message::Binary(frame.into())
        );
        lock(&state.0.voice).clear();
        let ended = tokio::time::timeout(Duration::from_secs(3), ws.next())
            .await
            .unwrap();
        assert!(ended.is_none_or(|r| matches!(r, Ok(Message::Close(_)) | Err(_))));
        assert!(
            tokio_tungstenite::connect_async(make(token)).await.is_err(),
            "revoked membership cannot reconnect with the old ticket"
        );
    }
}
