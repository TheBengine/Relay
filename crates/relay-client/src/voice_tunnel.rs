//! Optional bounded voice transport over the authenticated TLS WebSocket endpoint.
use std::{
    collections::VecDeque,
    net::{Shutdown, SocketAddr, TcpStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
use tungstenite::{client::IntoClientRequest, Message};
#[derive(Clone)]
pub struct Config {
    pub url: String,
    pub address: SocketAddr,
    pub session: u64,
    pub key: String,
}
pub struct Tunnel {
    ready: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    send: mpsc::SyncSender<(Vec<u8>, Option<u64>)>,
    incoming: Arc<Mutex<VecDeque<Vec<u8>>>>,
    owner: Option<thread::JoinHandle<()>>,
    watchdog: Option<thread::JoinHandle<()>>,
}
impl Tunnel {
    pub fn new(
        config: Config,
        call_stop: Arc<AtomicBool>,
        audio_allowed: impl Fn(u64) -> bool + Send + 'static,
    ) -> Self {
        let ready = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let incoming = Arc::new(Mutex::new(VecDeque::new()));
        let (send, receiver) = mpsc::sync_channel::<(Vec<u8>, Option<u64>)>(32);
        // A socket shutdown interrupts even a slow-drip TLS/HTTP handshake.
        let connection = Arc::new(Mutex::new(None::<(TcpStream, Instant)>));
        let monitor_connection = connection.clone();
        let monitor_stop = stop.clone();
        let monitor_call_stop = call_stop.clone();
        let monitor_ready = ready.clone();
        let watchdog = thread::spawn(move || loop {
            let stopping =
                monitor_stop.load(Ordering::Acquire) || monitor_call_stop.load(Ordering::Acquire);
            if let Ok(connection) = monitor_connection.lock() {
                if let Some((stream, deadline)) = connection.as_ref() {
                    if stopping
                        || (!monitor_ready.load(Ordering::Acquire) && Instant::now() >= *deadline)
                    {
                        let _ = stream.shutdown(Shutdown::Both);
                    }
                }
            }
            if stopping {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        });
        let flags = (ready.clone(), stop.clone(), incoming.clone());
        // Keep the original service URL across retries. Once this call used
        // embedded networking it can never revert to the OS private route.
        let private = crate::embedded_network::routed(&config.url);
        let owner = thread::spawn(move || {
            let (ready, stop, incoming) = flags;
            while !stop.load(Ordering::Acquire) && !call_stop.load(Ordering::Acquire) {
                let connected = (|| {
                    let (url, address) = reconnect_target(&config, private)?;
                    let stream =
                        TcpStream::connect_timeout(&address, Duration::from_millis(200)).ok()?;
                    if stop.load(Ordering::Acquire) || call_stop.load(Ordering::Acquire) {
                        return None;
                    }
                    {
                        let mut registered = connection.lock().ok()?;
                        if stop.load(Ordering::Acquire) || call_stop.load(Ordering::Acquire) {
                            return None;
                        }
                        *registered = Some((
                            stream.try_clone().ok()?,
                            Instant::now() + Duration::from_secs(2),
                        ));
                    }
                    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
                    stream
                        .set_write_timeout(Some(Duration::from_secs(2)))
                        .ok()?;
                    let mut request = url.as_str().into_client_request().ok()?;
                    request.headers_mut().insert(
                        "x-relay-voice-session",
                        config.session.to_string().parse().ok()?,
                    );
                    request
                        .headers_mut()
                        .insert("x-relay-voice-key", config.key.parse().ok()?);
                    let (mut socket, _) =
                        tungstenite::client_tls_with_config(request, stream, None, None).ok()?;
                    match socket.get_mut() {
                        tungstenite::stream::MaybeTlsStream::Plain(stream) => {
                            let _ = stream.set_read_timeout(Some(Duration::from_millis(10)));
                            let _ = stream.set_write_timeout(Some(Duration::from_millis(100)));
                        }
                        tungstenite::stream::MaybeTlsStream::Rustls(stream) => {
                            let _ = stream
                                .sock
                                .set_read_timeout(Some(Duration::from_millis(10)));
                            let _ = stream
                                .sock
                                .set_write_timeout(Some(Duration::from_millis(100)));
                        }
                        _ => return None,
                    }
                    ready.store(true, Ordering::Release);
                    while !stop.load(Ordering::Acquire) && !call_stop.load(Ordering::Acquire) {
                        for _ in 0..32 {
                            match receiver.try_recv() {
                                Ok((packet, audio)) => {
                                    if audio.is_some_and(|generation| !audio_allowed(generation)) {
                                        continue;
                                    }
                                    if socket.send(Message::Binary(packet)).is_err() {
                                        return None;
                                    }
                                }
                                Err(_) => break,
                            }
                        }
                        match socket.read() {
                            Ok(Message::Binary(packet))
                                if packet.len() <= relay_protocol::UDP_MAX_PACKET =>
                            {
                                if let Ok(mut queue) = incoming.lock() {
                                    if queue.len() >= 32 {
                                        queue.pop_front();
                                    }
                                    queue.push_back(packet);
                                }
                            }
                            Ok(Message::Close(_)) => break,
                            Ok(_) => {}
                            Err(tungstenite::Error::Io(error))
                                if matches!(
                                    error.kind(),
                                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                                ) => {}
                            Err(_) => break,
                        }
                    }
                    let _ = socket.close(None);
                    Some(())
                })();
                let _ = connected;
                ready.store(false, Ordering::Release);
                if let Ok(mut connection) = connection.lock() {
                    connection.take();
                }
                while receiver.try_recv().is_ok() {} // Never replay a stale send queue after reconnect.
                thread::sleep(Duration::from_millis(100));
            }
            ready.store(false, Ordering::Release);
        });
        Self {
            ready,
            stop,
            send,
            incoming,
            owner: Some(owner),
            watchdog: Some(watchdog),
        }
    }
    pub fn ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }
    pub fn send(&self, packet: Vec<u8>, audio: Option<u64>) -> bool {
        self.ready() && self.send.try_send((packet, audio)).is_ok()
    }
    pub fn receive(&self) -> Option<Vec<u8>> {
        self.incoming.lock().ok()?.pop_front()
    }
}

fn reconnect_target(config: &Config, private: bool) -> Option<(String, SocketAddr)> {
    if !private {
        return Some((config.url.clone(), config.address));
    }
    let mapped = crate::embedded_network::endpoint(&config.url);
    let endpoint = url::Url::parse(&mapped).ok()?;
    if endpoint.scheme() != "ws"
        || endpoint.host_str() != Some("127.0.0.1")
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
    {
        return None;
    }
    let port = endpoint.port().filter(|port| *port != 0)?;
    Some((
        mapped,
        SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), port),
    ))
}
impl Drop for Tunnel {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(watchdog) = self.watchdog.take() {
            let _ = watchdog.join();
        }
        if let Some(owner) = self.owner.take() {
            let _ = owner.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
    };
    fn config(address: SocketAddr) -> Config {
        Config {
            url: format!("ws://{address}/voice"),
            address,
            session: 7,
            key: "owned-fixture".into(),
        }
    }
    #[test]
    fn reconnect_keeps_normal_address_and_private_calls_never_use_os_fallback() {
        let address = "127.0.0.1:31234".parse().unwrap();
        let normal = config(address);
        assert_eq!(
            reconnect_target(&normal, false),
            Some((normal.url.clone(), address))
        );
        let private = Config {
            url: "ws://100.70.1.2:4580/api/voice/tunnel".into(),
            address,
            session: 7,
            key: "owned-fixture".into(),
        };
        // There is no registered embedded route in this fixture. Even though
        // an old local address is supplied, a private call cannot reuse it.
        assert!(reconnect_target(&private, true).is_none());
    }
    #[test]
    fn cancellation_interrupts_incomplete_handshake_and_bounds_owner_stop() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (accepted, ready) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            accepted.send(()).unwrap();
            let mut bytes = [0u8; 1024];
            while stream.read(&mut bytes).is_ok_and(|size| size != 0) {}
        });
        let tunnel = Tunnel::new(config(address), Arc::new(AtomicBool::new(false)), |_| true);
        ready.recv_timeout(Duration::from_secs(2)).unwrap();
        let start = Instant::now();
        drop(tunnel);
        assert!(start.elapsed() < Duration::from_millis(500));
        server.join().unwrap();
    }
    #[test]
    fn slow_drip_handshake_has_total_deadline_even_with_continuous_reads() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_write_timeout(Some(Duration::from_millis(100)))
                .unwrap();
            stream
                .write_all(b"HTTP/1.1 101 Switching Protocols\r\nX-Fill: ")
                .unwrap();
            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(4) {
                if stream.write_all(b"a").is_err() {
                    return start.elapsed();
                }
                thread::sleep(Duration::from_millis(40));
            }
            panic!("slow handshake exceeded deadline");
        });
        let tunnel = Tunnel::new(config(address), Arc::new(AtomicBool::new(false)), |_| true);
        let duration = server.join().unwrap();
        assert!(duration < Duration::from_secs(3));
        assert!(!tunnel.ready());
        drop(tunnel);
    }
    #[test]
    fn queued_audio_generation_is_rechecked_but_keepalive_remains_available() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (received, frames) = mpsc::channel();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            if let Message::Binary(frame) = socket.read().unwrap() {
                received.send(frame).unwrap();
            }
        });
        let tunnel = Tunnel::new(
            config(address),
            Arc::new(AtomicBool::new(false)),
            |generation| generation == 2,
        );
        let until = Instant::now() + Duration::from_secs(2);
        while !tunnel.ready() && Instant::now() < until {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(tunnel.send(vec![99], Some(1)));
        assert!(tunnel.send(vec![0], None));
        assert_eq!(
            frames.recv_timeout(Duration::from_secs(2)).unwrap(),
            vec![0]
        );
        drop(tunnel);
        server.join().unwrap();
    }
}
