//! "Find on local network": ask the local network which Relay communities
//! answer, for a few seconds, then stop.
//!
//! One explicit click starts one finite search (`SEARCH_WINDOW`) that can be
//! cancelled at any moment. It sends a 24-byte question with a fresh random
//! nonce to the limited broadcast address and to the broadcast address of each
//! of this PC's own private IPv4 subnets (at most `MAX_TARGETS`); it scans no
//! address range. Only well-formed replies to this question, from private or
//! link-local IPv4 senders, count. The host of a result is the packet's source
//! address and the port is the advertised one (at least 1024); a packet never
//! supplies an address or a URL. Results are de-duplicated, capped per sender
//! and in total, and nothing is trusted or joined: choosing one only fills the
//! ordinary server address field, and the ordinary connection check still has
//! to see the same server id that was advertised.
use crate::community::SavedCommunity;
use crate::invitation_probe::Subnet;
use relay_protocol::lan_discovery as wire;
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc,
};
use std::time::{Duration, Instant};

pub const SEARCH_WINDOW: Duration = Duration::from_millis(2500);
pub const MAX_RESULTS: usize = 16;
pub const MAX_PER_ADDRESS: usize = 4;
const MAX_TARGETS: usize = 8;
const READ_STEP: Duration = Duration::from_millis(100);
/// When the question is sent (and sent again, since UDP may drop it).
const SENDS: [Duration; 3] = [
    Duration::ZERO,
    Duration::from_millis(400),
    Duration::from_millis(1000),
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub name: String,
    pub server_id: String,
    pub ip: Ipv4Addr,
    pub port: u16,
}

impl Found {
    pub fn url(&self) -> String {
        format!("http://{}:{}", self.ip, self.port)
    }
    pub fn address(&self) -> String {
        format!("{}:{}", self.ip, self.port)
    }
}

/// Where to ask: the limited broadcast, then the broadcast address of each
/// private IPv4 subnet this PC is on (prefix 8 to 30). Never a range scan.
pub fn targets(subnets: &[Subnet]) -> Vec<SocketAddrV4> {
    let mut out = vec![SocketAddrV4::new(Ipv4Addr::BROADCAST, wire::PORT)];
    for subnet in subnets {
        let Subnet(IpAddr::V4(ip), bits) = *subnet else {
            continue;
        };
        if !wire::is_lan_ipv4(ip) || !(8..=30).contains(&bits) {
            continue;
        }
        let mask = u32::MAX << (32 - u32::from(bits));
        let target = SocketAddrV4::new(Ipv4Addr::from(u32::from(ip) | !mask), wire::PORT);
        if out.len() < MAX_TARGETS && !out.contains(&target) {
            out.push(target);
        }
    }
    out
}

/// Whether a result can be joined with the current sign-in. A Relay account
/// joins only over HTTPS, iroh or this PC's loopback (`account_destination`),
/// and a plain-HTTP network address is none of them: it can be found and
/// checked, but not joined with that account. This does not relax that rule.
pub fn joinable(relay_account_signed_in: bool, found: &Found) -> bool {
    !relay_account_signed_in || crate::community::account_destination(&found.url()).is_ok()
}

/// A secure route this PC already saved for the very server that answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavedRoute {
    /// The saved, normalized address (HTTPS, or an iroh address with its credentials).
    pub url: String,
    /// The server id that was pinned when it was saved.
    pub server_id: String,
    /// The name stored with the saved community (the answer's own name is unauthenticated).
    pub name: String,
}

impl SavedRoute {
    /// The stored name, fit to show: no control characters, at most 64 characters.
    pub fn display_name(&self) -> String {
        self.name
            .chars()
            .filter(|c| !c.is_control())
            .take(64)
            .collect::<String>()
            .trim()
            .to_string()
    }
}

/// What the user picked from a result.
pub enum Choice {
    /// The network address that answered (plain HTTP).
    Network(Found),
    /// The route saved earlier for the same pinned server id.
    Saved(Found),
    /// Explicit first-use pairing with a host-shared code; no credential on HTTP.
    Pair(Found),
}

/// Whether the iroh helper is installed. Checked once: it looks for a file and
/// starts nothing, so drawing result rows never touches the network or the helper.
pub fn helper_available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(crate::iroh_route::available)
}

/// A saved iroh route is usable only if the record really holds one: an address
/// with the key and the fixed port, credentials for that same key that the
/// existing route code accepts, and the helper installed.
fn iroh_route_usable(community: &SavedCommunity, url: &str, helper: bool) -> bool {
    helper
        && crate::iroh_route::endpoint_id(url).is_some_and(|key| {
            community.iroh.as_ref().is_some_and(|saved| {
                saved.endpoint_id == key && saved.to_invitation().is_ok() && saved.address() == url
            })
        })
}

/// A saved HTTPS route, or an iroh route that is really available, for the
/// server that answered. The match is the server id pinned when the route was
/// saved; the route itself always comes from the saved record and never from the
/// packet. A loopback address is not a route to another PC. Nothing here makes a
/// LAN HTTP address acceptable to the account rules.
pub fn secure_route(
    saved: &[SavedCommunity],
    found: &Found,
    iroh_helper_installed: bool,
) -> Option<SavedRoute> {
    saved.iter().find_map(|community| {
        if community.server_id.is_empty()
            || !community.server_id.eq_ignore_ascii_case(&found.server_id)
        {
            return None;
        }
        let url = crate::community::account_destination(&community.url).ok()?;
        let parsed = url::Url::parse(&url).ok()?;
        let host = parsed.host_str()?.to_ascii_lowercase();
        let host = host.trim_matches(['[', ']']);
        if host == "127.0.0.1" || host == "::1" || host == "localhost" {
            return None;
        }
        // account_destination lets only HTTPS, loopback and iroh addresses through;
        // what is neither HTTPS nor loopback here is an iroh address.
        if parsed.scheme() != "https" && !iroh_route_usable(community, &url, iroh_helper_installed)
        {
            return None;
        }
        Some(SavedRoute {
            url,
            server_id: community.server_id.clone(),
            name: community.name.clone(),
        })
    })
}

/// One search on an open socket. `emit` gets each new, valid result; the
/// search ends at the window, at `MAX_RESULTS` or when `cancel` is set.
/// Returns whether any question was actually sent.
pub(crate) fn run(
    socket: &UdpSocket,
    targets: &[SocketAddrV4],
    nonce: &wire::Nonce,
    cancel: &AtomicBool,
    window: Duration,
    allow: impl Fn(Ipv4Addr) -> bool,
    mut emit: impl FnMut(Found),
) -> bool {
    let request = wire::encode_request(nonce);
    // Whether the question left this PC at all, so that "no answers" is not
    // reported when nothing could be sent.
    let mut delivered = false;
    let started = Instant::now();
    let mut sent = 0;
    let mut seen: HashSet<(Ipv4Addr, u16)> = HashSet::new();
    let mut per_address: HashMap<Ipv4Addr, usize> = HashMap::new();
    let mut count = 0;
    let mut failures = 0;
    // One byte more than the largest packet, so an oversized one is recognised.
    let mut buffer = [0u8; wire::MAX_PACKET + 1];
    while !cancel.load(Ordering::Relaxed) && started.elapsed() < window && count < MAX_RESULTS {
        if sent < SENDS.len() && started.elapsed() >= SENDS[sent] {
            for target in targets {
                delivered |= socket.send_to(&request, target).is_ok();
            }
            sent += 1;
        }
        match socket.recv_from(&mut buffer) {
            Ok((length, SocketAddr::V4(source))) => {
                if length > wire::MAX_PACKET || !allow(*source.ip()) {
                    continue;
                }
                let Some(advert) = wire::parse_reply(&buffer[..length], nonce) else {
                    continue;
                };
                let key = (*source.ip(), advert.port);
                let used = per_address.entry(*source.ip()).or_default();
                if *used >= MAX_PER_ADDRESS || !seen.insert(key) {
                    continue;
                }
                *used += 1;
                count += 1;
                emit(Found {
                    name: advert.name,
                    server_id: advert.server_id,
                    ip: *source.ip(),
                    port: advert.port,
                });
            }
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        // Windows reports an unreachable earlier send this way.
                        | std::io::ErrorKind::ConnectionReset
                ) => {}
            // A datagram the OS could not deliver whole (too large for the buffer,
            // for example) must not end the search; a socket that keeps failing does.
            Err(_) => {
                failures += 1;
                if failures > 20 {
                    break;
                }
            }
        }
    }
    delivered
}

enum Event {
    Found(Found),
    Done { searched: bool },
}

struct Search {
    cancel: Arc<AtomicBool>,
    events: mpsc::Receiver<Event>,
}

/// Search state for one form. Dropping it cancels a running search.
#[derive(Default)]
pub struct Finder {
    search: Option<Search>,
    pub found: Vec<Found>,
    /// A search ran to its end (not cancelled).
    pub finished: bool,
    /// The PC could not send the question at all.
    pub failed: bool,
}

fn search_on_this_pc(cancel: &AtomicBool, events: &mpsc::Sender<Event>) -> bool {
    let mut nonce = [0u8; wire::NONCE_LEN];
    if getrandom::getrandom(&mut nonce).is_err() {
        return false;
    }
    let Ok(socket) = UdpSocket::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))) else {
        return false;
    };
    if socket.set_broadcast(true).is_err() || socket.set_read_timeout(Some(READ_STEP)).is_err() {
        return false;
    }
    let targets = targets(&crate::invitation_probe::local_subnets());
    run(
        &socket,
        &targets,
        &nonce,
        cancel,
        SEARCH_WINDOW,
        wire::is_lan_ipv4,
        |found| {
            let _ = events.send(Event::Found(found));
        },
    )
}

impl Finder {
    pub fn searching(&self) -> bool {
        self.search.is_some()
    }

    /// Begin one search, replacing any earlier one and its results.
    pub fn start(&mut self) {
        self.cancel();
        self.found.clear();
        self.finished = false;
        self.failed = false;
        let cancel = Arc::new(AtomicBool::new(false));
        let (sender, events) = mpsc::channel();
        let flag = cancel.clone();
        let spawned = std::thread::Builder::new()
            .name("lan-discovery".into())
            .spawn(move || {
                let searched = search_on_this_pc(&flag, &sender);
                let _ = sender.send(Event::Done { searched });
            });
        if spawned.is_ok() {
            self.search = Some(Search { cancel, events });
        } else {
            self.failed = true;
        }
    }

    /// Stop now. Results found so far stay on screen.
    pub fn cancel(&mut self) {
        if let Some(search) = self.search.take() {
            search.cancel.store(true, Ordering::Relaxed);
        }
    }

    /// Take what the search has found since the last call.
    pub fn poll(&mut self) {
        let Some(search) = &self.search else {
            return;
        };
        let mut done = None;
        loop {
            match search.events.try_recv() {
                Ok(Event::Found(found)) => {
                    let duplicate = self
                        .found
                        .iter()
                        .any(|known| known.ip == found.ip && known.port == found.port);
                    if !duplicate && self.found.len() < MAX_RESULTS {
                        self.found.push(found);
                    }
                }
                Ok(Event::Done { searched }) => {
                    done = Some(searched);
                    break;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    done = Some(true);
                    break;
                }
            }
        }
        if let Some(searched) = done {
            self.search = None;
            self.finished = true;
            self.failed = !searched;
        }
    }
}

impl Drop for Finder {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID_A: &str = "4be44aef-8c98-4db6-a9d5-81c2eb311330";
    const ID_B: &str = "0e1a9f6c-1111-4222-8333-444455556666";

    fn advert(port: u16, name: &str, id: &str) -> wire::Advert {
        wire::Advert {
            port,
            name: name.into(),
            server_id: id.into(),
        }
    }

    /// A stand-in community on loopback: answers a request with the given packets.
    fn responder(replies: impl Fn(&wire::Nonce) -> Vec<Vec<u8>> + Send + 'static) -> SocketAddrV4 {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let SocketAddr::V4(address) = socket.local_addr().unwrap() else {
            unreachable!()
        };
        std::thread::spawn(move || {
            let mut buffer = [0u8; 64];
            let started = Instant::now();
            while started.elapsed() < Duration::from_secs(4) {
                if let Ok((length, from)) = socket.recv_from(&mut buffer) {
                    if let Some(nonce) = wire::parse_request(&buffer[..length]) {
                        for packet in replies(&nonce) {
                            let _ = socket.send_to(&packet, from);
                        }
                    }
                }
            }
        });
        address
    }

    fn client() -> UdpSocket {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        socket
    }

    fn search(target: SocketAddrV4, nonce: wire::Nonce, allow: fn(Ipv4Addr) -> bool) -> Vec<Found> {
        let mut found = Vec::new();
        run(
            &client(),
            &[target],
            &nonce,
            &AtomicBool::new(false),
            Duration::from_millis(600),
            allow,
            |f| found.push(f),
        );
        found
    }

    fn loopback(ip: Ipv4Addr) -> bool {
        ip.is_loopback()
    }

    #[test]
    fn valid_replies_become_results_with_the_host_taken_from_the_packet_source() {
        let target =
            responder(
                |nonce| vec![wire::encode_reply(nonce, &advert(4580, "Home", ID_A)).unwrap()],
            );
        let found = search(target, [5u8; wire::NONCE_LEN], loopback);
        assert_eq!(
            found,
            [Found {
                name: "Home".into(),
                server_id: ID_A.into(),
                ip: Ipv4Addr::LOCALHOST,
                port: 4580
            }]
        );
        assert_eq!(found[0].url(), "http://127.0.0.1:4580");
    }

    #[test]
    fn stale_duplicate_malformed_and_oversized_replies_are_ignored() {
        let target = responder(|nonce| {
            let good = wire::encode_reply(nonce, &advert(4580, "Home", ID_A)).unwrap();
            let other_question =
                wire::encode_reply(&[0xEE; wire::NONCE_LEN], &advert(4581, "Old", ID_B)).unwrap();
            let low_port = {
                let mut packet = good.clone();
                packet[8 + wire::NONCE_LEN] = 0;
                packet[9 + wire::NONCE_LEN] = 22;
                packet
            };
            vec![
                other_question,
                good.clone(),
                good, // a duplicate of the same host and port
                low_port,
                vec![0u8; wire::MAX_PACKET + 40],
                b"not a relay packet".to_vec(),
                vec![],
            ]
        });
        let found = search(target, [6u8; wire::NONCE_LEN], loopback);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].name, "Home");
    }

    #[test]
    fn a_sender_outside_the_local_network_never_counts() {
        let target =
            responder(
                |nonce| vec![wire::encode_reply(nonce, &advert(4580, "Home", ID_A)).unwrap()],
            );
        // The production filter: loopback is not a local-network address.
        assert!(search(target, [7u8; wire::NONCE_LEN], wire::is_lan_ipv4).is_empty());
    }

    #[test]
    fn results_are_capped_per_address_and_in_total() {
        let target = responder(|nonce| {
            (0..40u16)
                .map(|n| wire::encode_reply(nonce, &advert(5000 + n, "Spam", ID_A)).unwrap())
                .collect()
        });
        let found = search(target, [8u8; wire::NONCE_LEN], loopback);
        assert_eq!(
            found.len(),
            MAX_PER_ADDRESS,
            "one address cannot fill the list"
        );
        assert!(found.len() <= MAX_RESULTS);
    }

    #[test]
    fn a_cancelled_search_stops_at_once_and_sends_nothing_more() {
        let cancel = AtomicBool::new(true);
        let started = Instant::now();
        let mut found = Vec::new();
        run(
            &client(),
            &[SocketAddrV4::new(Ipv4Addr::LOCALHOST, 9)],
            &[1u8; wire::NONCE_LEN],
            &cancel,
            Duration::from_secs(5),
            loopback,
            |f| found.push(f),
        );
        assert!(started.elapsed() < Duration::from_millis(500));
        assert!(found.is_empty());
    }

    #[test]
    fn a_search_reports_honestly_whether_anything_could_be_sent() {
        let run_to = |socket: &UdpSocket, target: SocketAddrV4| {
            run(
                socket,
                &[target],
                &[4u8; wire::NONCE_LEN],
                &AtomicBool::new(false),
                Duration::from_millis(200),
                loopback,
                |_| {},
            )
        };
        // Sending works to a loopback port even when nobody listens.
        assert!(run_to(&client(), SocketAddrV4::new(Ipv4Addr::LOCALHOST, 9)));
        // Without permission to broadcast, the question never leaves: not "no servers".
        assert!(!run_to(
            &client(),
            SocketAddrV4::new(Ipv4Addr::BROADCAST, wire::PORT)
        ));
    }

    #[test]
    fn a_relay_account_cannot_join_a_plain_http_network_address() {
        let found = |ip: &str| Found {
            name: "Home".into(),
            server_id: ID_A.into(),
            ip: ip.parse().unwrap(),
            port: 4580,
        };
        // Without a Relay account the ordinary server sign-in is available.
        assert!(joinable(false, &found("192.168.1.5")));
        // With one, the existing HTTPS/iroh/loopback rule decides, unchanged.
        assert!(!joinable(true, &found("192.168.1.5")));
        assert!(!joinable(true, &found("10.0.0.7")));
        assert!(joinable(true, &found("127.0.0.1")));
    }

    #[test]
    fn a_saved_secure_route_is_offered_only_for_the_same_pinned_server_id() {
        let saved = |url: &str, id: &str| SavedCommunity {
            url: url.into(),
            server_id: id.into(),
            name: "Friends' house".into(),
            ..Default::default()
        };
        let found = Found {
            name: "Beacon name".into(),
            server_id: ID_A.into(),
            ip: "192.168.1.5".parse().unwrap(),
            port: 4580,
        };
        // An HTTPS route saved for this id is offered, from the saved record, with its stored name.
        let route = secure_route(&[saved("HTTPS://Home.Example/", ID_A)], &found, false).unwrap();
        assert_eq!(route.url, "https://home.example");
        assert_eq!(route.server_id, ID_A);
        assert_eq!(route.display_name(), "Friends' house");
        assert!(secure_route(
            &[saved("https://home.example", &ID_A.to_uppercase())],
            &found,
            false
        )
        .is_some());
        // Nothing else is: another id, no pin, plain HTTP, this PC's own loopback.
        for (url, id) in [
            ("https://home.example", ID_B),
            ("https://home.example", ""),
            ("http://192.168.1.5:4580", ID_A),
            ("http://203.0.113.9:4580", ID_A),
            ("http://127.0.0.1:4580", ID_A),
            ("https://localhost", ID_A),
        ] {
            assert_eq!(
                secure_route(&[saved(url, id)], &found, true),
                None,
                "{url} {id}"
            );
        }
        assert_eq!(secure_route(&[], &found, true), None);
        // The first saved record that qualifies wins; an unusable one before it is skipped.
        let both = [
            saved("http://192.168.1.5:4580", ID_A),
            saved("https://home.example", ID_A),
        ];
        assert_eq!(
            secure_route(&both, &found, true).unwrap().url,
            "https://home.example"
        );
        // The stored name is shown without control characters.
        let mut odd = saved("https://home.example", ID_A);
        odd.name = format!("A\nB{}", "x".repeat(100));
        let shown = secure_route(&[odd], &found, true).unwrap().display_name();
        assert!(!shown.contains('\n') && shown.chars().count() <= 64);
    }

    #[test]
    fn a_saved_iroh_route_needs_real_credentials_and_an_installed_helper() {
        let key = "ab".repeat(32);
        let address = format!("http://{key}.iroh:4580");
        let token = "abcdefghijklmnopqrstuvwxyzABCDEF0123456789_-";
        let credentials = |key: &str, token: &str, relay: Option<&str>| {
            Some(crate::iroh_route::SavedIroh {
                endpoint_id: key.into(),
                relay: relay.map(Into::into),
                token: token.into(),
            })
        };
        let record = |url: &str, iroh| SavedCommunity {
            url: url.into(),
            server_id: ID_A.into(),
            name: "Home".into(),
            iroh,
            ..Default::default()
        };
        let found = Found {
            name: "Beacon".into(),
            server_id: ID_A.into(),
            ip: "192.168.1.5".parse().unwrap(),
            port: 4580,
        };
        // A complete record with the helper installed is offered, with its stored name.
        let good = record(&address, credentials(&key, token, None));
        let route = secure_route(&[good.clone()], &found, true).unwrap();
        assert_eq!(
            (route.url.as_str(), route.display_name().as_str()),
            (address.as_str(), "Home")
        );
        // The helper is not installed: nothing to offer, whatever the record says.
        assert_eq!(secure_route(&[good], &found, false), None);
        // Missing, mismatched or malformed credentials, or an address that is not the route.
        let other_key = "cd".repeat(32);
        for bad in [
            record(&address, None),
            record(&address, credentials(&other_key, token, None)),
            record(&address, credentials(&key, "short", None)),
            record(
                &address,
                credentials(&key, token, Some("http://relay.example")),
            ),
            record(
                &format!("http://{key}.iroh"),
                credentials(&key, token, None),
            ),
            record(
                &format!("http://{key}.iroh:4581"),
                credentials(&key, token, None),
            ),
        ] {
            assert_eq!(
                secure_route(&[bad.clone()], &found, true),
                None,
                "{:?}",
                bad.url
            );
        }
        // An unusable iroh record before a usable HTTPS one is skipped, not preferred.
        let list = [record(&address, None), record("https://home.example", None)];
        assert_eq!(
            secure_route(&list, &found, true).unwrap().url,
            "https://home.example"
        );
    }

    #[test]
    fn a_search_always_ends_at_its_window() {
        let started = Instant::now();
        run(
            &client(),
            &[SocketAddrV4::new(Ipv4Addr::LOCALHOST, 9)],
            &[1u8; wire::NONCE_LEN],
            &AtomicBool::new(false),
            Duration::from_millis(250),
            loopback,
            |_| {},
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(SEARCH_WINDOW <= Duration::from_secs(5));
    }

    #[test]
    fn questions_go_only_to_the_broadcast_of_our_own_private_subnets() {
        let subnets = [
            Subnet("192.168.1.20".parse().unwrap(), 24),
            Subnet("192.168.1.99".parse().unwrap(), 24), // same subnet: once
            Subnet("10.0.0.5".parse().unwrap(), 16),
            Subnet("8.8.8.8".parse().unwrap(), 24), // public: never
            Subnet("127.0.0.1".parse().unwrap(), 8),
            Subnet("192.168.5.5".parse().unwrap(), 31), // too small to broadcast on
            Subnet("192.168.6.6".parse().unwrap(), 7),  // too wide to be a home network
            Subnet("fe80::1".parse().unwrap(), 64),
        ];
        let list: Vec<String> = targets(&subnets).iter().map(|t| t.to_string()).collect();
        assert_eq!(
            list,
            [
                "255.255.255.255:47932",
                "192.168.1.255:47932",
                "10.0.255.255:47932"
            ]
        );
        // Many subnets cannot widen the list beyond its cap.
        let many: Vec<Subnet> = (0..40u8)
            .map(|n| Subnet(format!("192.168.{n}.2").parse().unwrap(), 24))
            .collect();
        assert!(targets(&many).len() <= MAX_TARGETS);
    }

    #[test]
    fn the_finder_collects_without_blocking_and_cancel_keeps_what_was_found() {
        let mut finder = Finder::default();
        assert!(!finder.searching() && finder.found.is_empty());
        // A fake finished search feeds the same channel the real one uses.
        let (sender, events) = mpsc::channel();
        let found = |port| Found {
            name: "Home".into(),
            server_id: ID_A.into(),
            ip: "192.168.1.5".parse().unwrap(),
            port,
        };
        sender.send(Event::Found(found(4580))).unwrap();
        sender.send(Event::Found(found(4580))).unwrap(); // duplicate
        sender.send(Event::Found(found(4581))).unwrap();
        finder.search = Some(Search {
            cancel: Arc::new(AtomicBool::new(false)),
            events,
        });
        finder.poll();
        assert_eq!(finder.found.len(), 2);
        assert!(finder.searching() && !finder.finished);
        sender.send(Event::Done { searched: true }).unwrap();
        finder.poll();
        assert!(!finder.searching() && finder.finished && !finder.failed);
        // Cancelling a running search keeps the list and never marks it finished.
        let (_sender, events) = mpsc::channel();
        let flag = Arc::new(AtomicBool::new(false));
        finder.search = Some(Search {
            cancel: flag.clone(),
            events,
        });
        finder.finished = false;
        finder.cancel();
        assert!(flag.load(Ordering::Relaxed) && !finder.searching() && !finder.finished);
        assert_eq!(finder.found.len(), 2);
        // A search that could not even send says so.
        let (sender, events) = mpsc::channel();
        sender.send(Event::Done { searched: false }).unwrap();
        finder.search = Some(Search {
            cancel: Arc::new(AtomicBool::new(false)),
            events,
        });
        finder.poll();
        assert!(finder.failed && finder.finished);
    }
}
