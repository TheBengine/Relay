//! Opt-in answer to "who runs a Relay community on this network?".
//!
//! Off unless the host starts the server with `--lan-discovery`. It listens on
//! one UDP port (`relay_protocol::lan_discovery::PORT`), answers only well-formed
//! requests from private or link-local IPv4 senders, and replies to the sender
//! with the community name, the HTTP port and the server id: nothing the
//! community does not already show anyone who reaches its address. It carries no
//! address, credential, invitation, user or count, opens no firewall and
//! changes no setting. Answers are rate limited, so a request cannot be used to
//! make the server send much more than it receives, and only to the sender.
use super::*;
use relay_protocol::lan_discovery as wire;
use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

const REPLIES_PER_SECOND: u32 = 40;
const REPLIES_PER_SOURCE_PER_SECOND: u32 = 4;
const MAX_TRACKED_SOURCES: usize = 256;
const ADVERT_CACHE_SECS: u64 = 30;

/// Discovery is only useful, and only allowed, when the HTTP server can be
/// reached on the LAN: an unspecified IPv4 bind or a private/link-local IPv4.
pub fn bind_is_lan_reachable(bind: std::net::IpAddr) -> bool {
    match bind {
        std::net::IpAddr::V4(ip) => ip.is_unspecified() || wire::is_lan_ipv4(ip),
        std::net::IpAddr::V6(_) => false,
    }
}

/// A per-second budget in total and per sender.
#[derive(Default)]
struct Limiter {
    second: u64,
    total: u32,
    sources: HashMap<Ipv4Addr, u32>,
}

impl Limiter {
    fn allow(&mut self, source: Ipv4Addr, second: u64) -> bool {
        if second != self.second {
            self.second = second;
            self.total = 0;
            self.sources.clear();
        }
        if self.total >= REPLIES_PER_SECOND {
            return false;
        }
        if self.sources.len() >= MAX_TRACKED_SOURCES && !self.sources.contains_key(&source) {
            return false;
        }
        let count = self.sources.entry(source).or_default();
        if *count >= REPLIES_PER_SOURCE_PER_SECOND {
            return false;
        }
        *count += 1;
        self.total += 1;
        true
    }
}

struct Responder {
    state: AppState,
    http_port: u16,
    allow: fn(Ipv4Addr) -> bool,
    limiter: Limiter,
    cached: Option<(u64, wire::Advert)>,
}

impl Responder {
    fn advert(&mut self, second: u64) -> Option<wire::Advert> {
        if let Some((at, advert)) = &self.cached {
            if second.saturating_sub(*at) < ADVERT_CACHE_SECS {
                return Some(advert.clone());
            }
        }
        let db = lock(&self.state.0.db);
        let name = server_settings::presentation(&db, &self.state.0.name)
            .map(|presentation| presentation.name)
            .unwrap_or_else(|_| self.state.0.name.clone());
        let advert = wire::Advert {
            port: self.http_port,
            name: wire::sanitize_name(&name),
            server_id: community::server_id(&db).ok()?,
        };
        drop(db);
        self.cached = Some((second, advert.clone()));
        Some(advert)
    }

    /// The reply to one datagram, or nothing. Never answers a request it cannot
    /// parse exactly, a sender outside the local network, or beyond the budget.
    fn answer(&mut self, packet: &[u8], source: SocketAddr, second: u64) -> Option<Vec<u8>> {
        let SocketAddr::V4(source) = source else {
            return None;
        };
        if !(self.allow)(*source.ip()) {
            return None;
        }
        let nonce = wire::parse_request(packet)?;
        if !self.limiter.allow(*source.ip(), second) {
            return None;
        }
        let advert = self.advert(second)?;
        wire::encode_reply(&nonce, &advert)
    }
}

/// Start the responder: listen on `bind` (`0.0.0.0:PORT` in production, so
/// broadcasts arrive) and answer from the address the HTTP server listens on.
/// With a specific `http_bind` replies are sent from a socket bound to exactly
/// that address, so the source the client derives the host from is an address
/// where HTTP really listens, whichever interface the question came in on. The
/// caller decides whether a failure is fatal; the server itself is unaffected.
pub fn start(
    state: AppState,
    http_port: u16,
    http_bind: Ipv4Addr,
    bind: SocketAddrV4,
) -> std::io::Result<(SocketAddr, tokio::task::JoinHandle<()>)> {
    start_with(state, http_port, http_bind, bind, wire::is_lan_ipv4)
}

fn start_with(
    state: AppState,
    http_port: u16,
    http_bind: Ipv4Addr,
    bind: SocketAddrV4,
    allow: fn(Ipv4Addr) -> bool,
) -> std::io::Result<(SocketAddr, tokio::task::JoinHandle<()>)> {
    let socket = std::net::UdpSocket::bind(bind)?;
    socket.set_nonblocking(true)?;
    let address = socket.local_addr()?;
    let socket = tokio::net::UdpSocket::from_std(socket)?;
    let reply_socket = if http_bind.is_unspecified() {
        None
    } else {
        let reply = std::net::UdpSocket::bind(SocketAddrV4::new(http_bind, 0))?;
        reply.set_nonblocking(true)?;
        Some(tokio::net::UdpSocket::from_std(reply)?)
    };
    let mut responder = Responder {
        state: state.clone(),
        http_port,
        allow,
        limiter: Limiter::default(),
        cached: None,
    };
    let task = tokio::spawn(async move {
        // One byte more than a request, so a longer packet is seen as too long.
        let mut buffer = [0u8; wire::REQUEST_LEN + 8];
        loop {
            tokio::select! {
                _ = state.shutdown_requested() => break,
                received = socket.recv_from(&mut buffer) => {
                    let (length, source) = match received {
                        Ok(received) => received,
                        Err(error) => {
                            // Windows reports an unreachable earlier reply as a reset.
                            if error.kind() != std::io::ErrorKind::ConnectionReset {
                                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                            }
                            continue;
                        }
                    };
                    let second = now();
                    if length > wire::REQUEST_LEN {
                        continue;
                    }
                    if let Some(reply) = responder.answer(&buffer[..length], source, second) {
                        let sender = reply_socket.as_ref().unwrap_or(&socket);
                        let _ = sender.send_to(&reply, source).await;
                    }
                }
            }
        }
    });
    Ok((address, task))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn loopback_ok(ip: Ipv4Addr) -> bool {
        ip.is_loopback()
    }

    #[test]
    fn only_a_lan_reachable_bind_may_advertise() {
        for ok in [
            "0.0.0.0",
            "192.168.1.5",
            "10.1.2.3",
            "172.20.0.9",
            "169.254.1.1",
        ] {
            assert!(bind_is_lan_reachable(ok.parse().unwrap()), "{ok}");
        }
        for no in ["127.0.0.1", "8.8.8.8", "100.64.0.1", "::", "::1", "fe80::1"] {
            assert!(!bind_is_lan_reachable(no.parse().unwrap()), "{no}");
        }
    }

    #[test]
    fn replies_are_budgeted_in_total_and_per_sender() {
        let mut limiter = Limiter::default();
        let one: Ipv4Addr = "192.168.1.2".parse().unwrap();
        let answered = (0..20).filter(|_| limiter.allow(one, 5)).count();
        assert_eq!(answered as u32, REPLIES_PER_SOURCE_PER_SECOND);
        // A new second starts a new budget.
        assert!(limiter.allow(one, 6));
        // Many senders cannot exceed the total, or grow the table without bound.
        let mut limiter = Limiter::default();
        let mut total = 0;
        for n in 0..1000u32 {
            let ip = Ipv4Addr::from(0xC0A8_0000 + n);
            if limiter.allow(ip, 1) {
                total += 1;
            }
        }
        assert!(total <= REPLIES_PER_SECOND as usize);
        assert!(limiter.sources.len() <= MAX_TRACKED_SOURCES);
    }

    #[tokio::test]
    async fn it_answers_only_exact_requests_with_public_community_facts() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::open(dir.path(), "Home\nServer", 0).unwrap();
        let server_id = community::server_id(&lock(&state.0.db)).unwrap();
        let (address, task) = start_with(
            state.clone(),
            4580,
            Ipv4Addr::LOCALHOST,
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0),
            loopback_ok,
        )
        .unwrap();
        let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let nonce = [9u8; wire::NONCE_LEN];
        client
            .send_to(&wire::encode_request(&nonce), address)
            .await
            .unwrap();
        let mut buffer = [0u8; 512];
        let (length, from) =
            tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buffer))
                .await
                .expect("a reply")
                .unwrap();
        // A specific HTTP bind answers from a socket bound to exactly that address.
        assert_eq!(from.ip(), address.ip());
        assert_ne!(from.port(), address.port());
        let advert = wire::parse_reply(&buffer[..length], &nonce).expect("a valid reply");
        assert_eq!(advert.port, 4580);
        assert_eq!(advert.server_id, server_id);
        assert!(wire::valid_name(&advert.name));
        // Nothing of the database's secrets or people travels in it.
        let text = String::from_utf8_lossy(&buffer[..length]).to_string();
        for forbidden in ["password", "token", "invite", "@", "http"] {
            assert!(!text.contains(forbidden), "{forbidden}");
        }
        // The same reply is useless for any other question.
        assert!(wire::parse_reply(&buffer[..length], &[1u8; wire::NONCE_LEN]).is_none());

        // Malformed, longer, wrong-magic and reply-shaped packets get no answer.
        let mut longer = wire::encode_request(&nonce).to_vec();
        longer.push(0);
        let mut wrong = wire::encode_request(&nonce);
        wrong[0] = b'Z';
        let reply_shaped = wire::encode_reply(&nonce, &advert).unwrap();
        for packet in [longer, wrong.to_vec(), reply_shaped, vec![0u8; 3], vec![]] {
            client.send_to(&packet, address).await.unwrap();
        }
        let silent =
            tokio::time::timeout(Duration::from_millis(300), client.recv_from(&mut buffer)).await;
        assert!(silent.is_err(), "no reply to anything but an exact request");

        // The shutdown request ends the task.
        state.request_shutdown();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("stops")
            .unwrap();
    }

    #[tokio::test]
    async fn an_all_interfaces_bind_answers_from_the_listening_socket() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::open(dir.path(), "Home", 0).unwrap();
        let (address, task) = start_with(
            state.clone(),
            4580,
            Ipv4Addr::UNSPECIFIED,
            SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0),
            loopback_ok,
        )
        .unwrap();
        let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let nonce = [4u8; wire::NONCE_LEN];
        client
            .send_to(
                &wire::encode_request(&nonce),
                SocketAddr::from((Ipv4Addr::LOCALHOST, address.port())),
            )
            .await
            .unwrap();
        let mut buffer = [0u8; 512];
        let (length, from) =
            tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buffer))
                .await
                .expect("a reply")
                .unwrap();
        assert_eq!(from.port(), address.port());
        assert!(wire::parse_reply(&buffer[..length], &nonce).is_some());
        state.request_shutdown();
        let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
    }

    #[tokio::test]
    async fn a_sender_outside_the_local_network_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::open(dir.path(), "Home", 0).unwrap();
        // Production filter: loopback is not a LAN address, so this request is ignored.
        let (address, task) = start(
            state.clone(),
            4580,
            Ipv4Addr::LOCALHOST,
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0),
        )
        .unwrap();
        let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client
            .send_to(&wire::encode_request(&[2u8; wire::NONCE_LEN]), address)
            .await
            .unwrap();
        let mut buffer = [0u8; 64];
        let got =
            tokio::time::timeout(Duration::from_millis(300), client.recv_from(&mut buffer)).await;
        assert!(got.is_err());
        state.request_shutdown();
        let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
    }
}
