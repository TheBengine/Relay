//! Shared pieces of the end-to-end tests: a stand-in community and plain HTTP.
#![allow(dead_code)]
use futures_util::{SinkExt, StreamExt};
use http_body_util::Full;
use hyper::{body::Bytes, service::service_fn, Request, Response};
use hyper_util::rt::TokioIo;
use std::{
    convert::Infallible,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream, UdpSocket},
    time::{sleep, timeout},
};

pub const INGRESS: &str = "fixture-network-ingress-capability-distinct-from-control";

/// What the stand-in community saw, in order.
pub type Seen = Arc<Mutex<Vec<String>>>;

pub struct Community {
    pub tcp: u16,
    pub udp: u16,
    pub seen: Seen,
}

pub async fn community() -> Community {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let tcp = listener.local_addr().unwrap().port();
    let seen: Seen = Arc::default();
    let log = seen.clone();
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            let log = log.clone();
            tokio::spawn(async move {
                // A WebSocket is accepted by tungstenite itself; everything else is plain HTTP.
                let mut head = [0u8; 16];
                let mut got = 0;
                for _ in 0..200 {
                    got = socket.peek(&mut head).await.unwrap_or(0);
                    if got >= 12 {
                        break;
                    }
                    sleep(Duration::from_millis(5)).await;
                }
                if head[..got].starts_with(b"GET /api/ws") {
                    log.lock().unwrap().push("GET /api/ws".into());
                    let Ok(mut ws) = tokio_tungstenite::accept_async(socket).await else {
                        return;
                    };
                    while let Some(Ok(message)) = ws.next().await {
                        if message.is_close() {
                            let _ = ws.flush().await;
                            break;
                        }
                        let _ = ws.send(message).await;
                    }
                    return;
                }
                let service = service_fn(move |request: Request<hyper::body::Incoming>| {
                    let log = log.clone();
                    async move {
                        let header = |name: &str| {
                            request
                                .headers()
                                .get(name)
                                .and_then(|v| v.to_str().ok())
                                .unwrap_or("")
                                .to_owned()
                        };
                        log.lock().unwrap().push(format!(
                            "{} {}",
                            request.method(),
                            request.uri().path()
                        ));
                        let body = serde_json::json!({
                            "ok": true,
                            "ingress": header("x-relay-network-ingress"),
                            "peer": header("x-relay-network-peer"),
                            "authorization": header("authorization"),
                            "forwarded": header("x-forwarded-for"),
                            "host": header("host"),
                        });
                        Ok::<_, Infallible>(Response::new(Full::new(Bytes::from(body.to_string()))))
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(socket), service)
                    .await;
            });
        }
    });
    let udp_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let udp = udp_socket.local_addr().unwrap().port();
    tokio::spawn(async move {
        let mut buffer = [0u8; 2048];
        while let Ok((n, from)) = udp_socket.recv_from(&mut buffer).await {
            let _ = udp_socket.send_to(&buffer[..n], from).await;
        }
    });
    Community { tcp, udp, seen }
}

/// One plain HTTP exchange; returns the status line and the body.
pub async fn http(to: SocketAddr, request: &str) -> (String, String) {
    let mut socket = TcpStream::connect(to).await.unwrap();
    socket.write_all(request.as_bytes()).await.unwrap();
    let mut answer = Vec::new();
    timeout(Duration::from_secs(10), socket.read_to_end(&mut answer))
        .await
        .expect("the answer arrives")
        .unwrap();
    let text = String::from_utf8_lossy(&answer).to_string();
    let status = text.lines().next().unwrap_or("").to_owned();
    let body = text.split("\r\n\r\n").nth(1).unwrap_or("").to_owned();
    (status, body)
}

pub fn get(path: &str, extra: &str) -> String {
    format!("GET {path} HTTP/1.1\r\nHost: friend.example\r\nConnection: close\r\n{extra}\r\n")
}
