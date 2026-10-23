//! One admitted QUIC stream carries one bounded WebRTC UDP allocation.
//! The destination is the host's configured SFU port, never a peer-supplied address.
use crate::host::Body;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{body::Incoming, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UdpSocket,
    sync::Semaphore,
    time::timeout,
};

pub const PATH: &str = "/_relay/media";
const MAX_PACKET: usize = 1600;
const IDLE: Duration = Duration::from_secs(90);

fn reply(status: StatusCode, text: String) -> Response<Body> {
    let mut result = Response::new(
        Full::new(Bytes::from(text))
            .map_err(|never| match never {})
            .boxed(),
    );
    *result.status_mut() = status;
    result
        .headers_mut()
        .insert("cache-control", "no-store".parse().unwrap());
    result
}

pub async fn handle(
    mut request: Request<Incoming>,
    port: u16,
    slots: Arc<Semaphore>,
) -> Response<Body> {
    if port == 0
        || request.uri().query().is_some()
        || request.headers().contains_key("origin")
        || request.headers().contains_key("sec-fetch-site")
        || request.uri().authority().is_some()
        || request.uri().scheme().is_some()
    {
        return reply(StatusCode::FORBIDDEN, "Video is not available".into());
    }
    if request.method() == hyper::Method::GET {
        return reply(
            StatusCode::OK,
            format!("{{\"version\":1,\"media_udp_port\":{port}}}"),
        );
    }
    if request.method() != hyper::Method::POST
        || !crate::policy::is_upgrade(request.headers())
        || request
            .headers()
            .get("upgrade")
            .and_then(|v| v.to_str().ok())
            != Some("relay-media-v1")
        || request
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            != Some("0")
        || request.headers().contains_key("transfer-encoding")
    {
        return reply(StatusCode::BAD_REQUEST, "Invalid video connection".into());
    }
    let Ok(permit) = slots.try_acquire_owned() else {
        return reply(
            StatusCode::TOO_MANY_REQUESTS,
            "Video connection limit reached".into(),
        );
    };
    let Ok(socket) = UdpSocket::bind("127.0.0.1:0").await else {
        return reply(StatusCode::SERVICE_UNAVAILABLE, "Video is not ready".into());
    };
    if socket
        .connect((std::net::Ipv4Addr::LOCALHOST, port))
        .await
        .is_err()
    {
        return reply(StatusCode::SERVICE_UNAVAILABLE, "Video is not ready".into());
    }
    let upgraded = hyper::upgrade::on(&mut request);
    tokio::spawn(async move {
        let _permit = permit;
        let Ok(Ok(stream)) = timeout(Duration::from_secs(5), upgraded).await else {
            return;
        };
        let (mut reader, mut writer) = tokio::io::split(TokioIo::new(stream));
        let outgoing = async {
            let mut packet = [0; MAX_PACKET];
            loop {
                let size = timeout(IDLE, reader.read_u16()).await.ok()?.ok()? as usize;
                if size == 0 || size > MAX_PACKET {
                    return None::<()>;
                }
                timeout(IDLE, reader.read_exact(&mut packet[..size]))
                    .await
                    .ok()?
                    .ok()?;
                socket.send(&packet[..size]).await.ok()?;
            }
        };
        let incoming = async {
            let mut packet = [0; MAX_PACKET + 1];
            loop {
                let size = timeout(IDLE, socket.recv(&mut packet)).await.ok()?.ok()?;
                if size == 0 {
                    return None::<()>;
                }
                if size > MAX_PACKET {
                    continue;
                }
                // Bound how long old video may accumulate behind a slow stream.
                timeout(Duration::from_millis(300), async {
                    writer.write_u16(size as u16).await?;
                    writer.write_all(&packet[..size]).await
                })
                .await
                .ok()?
                .ok()?;
            }
        };
        tokio::select! { _ = outgoing => {}, _ = incoming => {} }
    });
    let mut result = reply(StatusCode::SWITCHING_PROTOCOLS, String::new());
    result
        .headers_mut()
        .insert("connection", "upgrade".parse().unwrap());
    result
        .headers_mut()
        .insert("upgrade", "relay-media-v1".parse().unwrap());
    result
}
