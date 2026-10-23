//! How the connection to the community is carried, and how quickly it answers.
//!
//! This is a status display. It reads no secret, sends no content and never
//! chooses a route. A measurement runs on its own short-lived thread at a calm
//! interval, and only while the interface asks for it (a call or the debug page).
use eframe::egui;
use std::{
    collections::HashMap,
    net::IpAddr,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

/// How the community is reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    /// Relay's built-in private network (Tailscale).
    Tailscale,
    /// An ordinary address: this home network, IPv6 or a public address.
    Direct,
    /// Reserved for the account-free route.
    Iroh,
}

/// What lies between the two PCs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathKind {
    /// The two PCs talk to each other without a middleman.
    Direct,
    /// A machine in the private network forwards the traffic.
    PeerRelay,
    /// A public relay server forwards the traffic. This adds delay.
    Relay,
    Unknown,
}

/// One measurement. It never holds an address of the other PC.
#[derive(Clone, Debug, PartialEq)]
pub struct PathReport {
    pub transport: Transport,
    pub kind: PathKind,
    /// Short region code of a public relay, such as `fra`.
    pub region: Option<String>,
    pub rtt_ms: Option<f32>,
    /// The other PC is on this home network (private or loopback address).
    pub same_network: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quality {
    Good,
    Fair,
    Poor,
    Unknown,
}

impl PathReport {
    pub fn quality(&self) -> Quality {
        match self.rtt_ms {
            None => Quality::Unknown,
            Some(ms) if ms <= 60.0 => Quality::Good,
            Some(ms) if ms <= 120.0 => Quality::Fair,
            Some(_) => Quality::Poor,
        }
    }

    fn transport_name(&self) -> &'static str {
        match self.transport {
            Transport::Tailscale => "Tailscale",
            Transport::Iroh => "iroh",
            Transport::Direct => {
                if self.same_network {
                    "Home network"
                } else {
                    "Direct address"
                }
            }
        }
    }

    /// One short line, for example "Tailscale · direct · 12 ms".
    pub fn headline(&self) -> String {
        let mut parts = vec![self.transport_name().to_owned()];
        match self.kind {
            PathKind::Direct => {
                if self.transport != Transport::Direct {
                    parts.push("direct".into());
                }
            }
            PathKind::PeerRelay => parts.push("via your relay machine".into()),
            PathKind::Relay => parts.push(match self.region.as_deref() {
                Some(code) => format!("via relay ({})", region_name(code)),
                None => "via relay".into(),
            }),
            PathKind::Unknown => parts.push("measuring…".into()),
        }
        if let Some(ms) = self.rtt_ms {
            parts.push(format!("{} ms", ms.round() as u32));
        }
        parts.join(" · ")
    }

    /// One sentence of advice, only when something can be done about it.
    pub fn hint(&self) -> Option<&'static str> {
        match (self.transport, self.kind) {
            (Transport::Tailscale, PathKind::Relay) | (Transport::Iroh, PathKind::Relay) => Some(
                "Your connection goes through a relay server, which adds delay. A direct connection is faster: turn on UPnP or NAT-PMP in both routers, and avoid two routers in a row.",
            ),
            (Transport::Tailscale, PathKind::PeerRelay) => Some(
                "Traffic goes through your own relay machine. Keep it online and close to you both.",
            ),
            (Transport::Direct, _) if self.quality() == Quality::Poor && !self.same_network => Some(
                "This is a slow connection to the host. Check the host's internet speed and Wi-Fi.",
            ),
            _ => None,
        }
    }
}

/// Plain names for the public relay regions Tailscale uses. An unknown code is
/// shown in capitals, never as free text from the network.
pub fn region_name(code: &str) -> String {
    match code {
        "fra" => "Frankfurt",
        "ams" => "Amsterdam",
        "cph" => "Copenhagen",
        "lhr" => "London",
        "par" => "Paris",
        "sto" | "arn" => "Stockholm",
        "waw" => "Warsaw",
        "mad" => "Madrid",
        "nyc" => "New York",
        "sfo" => "San Francisco",
        "sin" => "Singapore",
        "tok" | "nrt" => "Tokyo",
        "syd" => "Sydney",
        other => {
            return other
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
                .take(16)
                .collect::<String>()
                .to_ascii_uppercase()
        }
    }
    .to_owned()
}

/// Whether an address is on this home network or a public one.
pub fn is_same_network(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => ip.is_loopback() || ip.is_private() || ip.is_link_local(),
        Ok(IpAddr::V6(ip)) => {
            let first = ip.segments()[0];
            ip.is_loopback() || (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80
        }
        Err(_) => false,
    }
}

/// The transport an address belongs to.
pub fn transport_for(address: &str) -> Transport {
    let host = url::Url::parse(address)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned));
    match host {
        Some(h) if crate::tailscale_connect::valid_address(&h) => Transport::Tailscale,
        Some(_) if crate::iroh_route::is_iroh(address) => Transport::Iroh,
        _ => Transport::Direct,
    }
}

/// The helper's answer, checked before it reaches the interface.
#[derive(serde::Deserialize)]
pub struct HelperPath {
    pub kind: String,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub rtt_ms: f64,
}

impl HelperPath {
    pub fn into_report(self) -> PathReport {
        let kind = match self.kind.as_str() {
            "direct" => PathKind::Direct,
            "peer_relay" => PathKind::PeerRelay,
            "derp" => PathKind::Relay,
            _ => PathKind::Unknown,
        };
        let region = self.region.filter(|r| {
            !r.is_empty()
                && r.len() <= 16
                && r.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        });
        let rtt_ms = (self.rtt_ms.is_finite() && self.rtt_ms > 0.0 && self.rtt_ms <= 60_000.0)
            .then_some(self.rtt_ms as f32);
        PathReport {
            transport: Transport::Tailscale,
            kind,
            region: if kind == PathKind::Relay {
                region
            } else {
                None
            },
            rtt_ms,
            same_network: false,
        }
    }
}

/// An ordinary address answers a health request. This includes server processing
/// and measures the chat/API endpoint, not the voice or video path.
pub fn direct_report(address: &str, rtt_ms: Option<f32>) -> PathReport {
    let same_network = url::Url::parse(address)
        .ok()
        .and_then(|u| u.host_str().map(is_same_network))
        .unwrap_or(false);
    PathReport {
        transport: Transport::Direct,
        kind: if rtt_ms.is_some() {
            PathKind::Direct
        } else {
            PathKind::Unknown
        },
        region: None,
        rtt_ms,
        same_network,
    }
}

const REFRESH: Duration = Duration::from_secs(5);
const MAX_AGE: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleState {
    Checking,
    Current,
    Stale,
    Failed,
}

#[derive(Clone, Debug)]
pub struct PathSample {
    pub report: Option<PathReport>,
    /// Time since the last successful check, never since a failed attempt.
    pub age: Option<Duration>,
    pub state: SampleState,
}

#[derive(Default)]
struct Entry {
    report: Option<PathReport>,
    started: Option<Instant>,
    finished: Option<Instant>,
    measured: Option<Instant>,
    failed: bool,
}

/// Remembers the latest measurement per address and decides when one is due.
#[derive(Default)]
pub struct PathCache {
    entries: HashMap<String, Entry>,
}

impl PathCache {
    /// True when a new measurement should start now; marks it as running.
    pub fn begin_if_due(&mut self, address: &str, now: Instant) -> bool {
        if self.entries.len() >= 16 && !self.entries.contains_key(address) {
            // Keep the cache small: drop the entry measured longest ago.
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.finished.or(e.started))
                .map(|(k, _)| k.clone());
            if let Some(oldest) = oldest {
                self.entries.remove(&oldest);
            }
        }
        let entry = self.entries.entry(address.to_owned()).or_default();
        let running = match (entry.started, entry.finished) {
            (Some(started), Some(finished)) => started > finished,
            (Some(_), None) => true,
            _ => false,
        };
        if running {
            // A measurement which never reports back is retried after a longer wait.
            if entry
                .started
                .is_some_and(|s| now.duration_since(s) < Duration::from_secs(15))
            {
                return false;
            }
        } else if entry
            .finished
            .is_some_and(|f| now.duration_since(f) < REFRESH)
        {
            return false;
        }
        entry.started = Some(now);
        true
    }

    pub fn finish(&mut self, address: &str, report: PathReport, now: Instant) {
        if let Some(entry) = self.entries.get_mut(address) {
            entry.report = Some(report);
            entry.finished = Some(now);
            entry.measured = Some(now);
            entry.failed = false;
        }
    }

    /// Retain when the last successful check happened, but retire its live values.
    pub fn fail(&mut self, address: &str, now: Instant) {
        if let Some(entry) = self.entries.get_mut(address) {
            entry.finished = Some(now);
            entry.failed = true;
        }
    }

    pub fn sample(&self, address: &str, now: Instant) -> Option<PathSample> {
        let entry = self.entries.get(address)?;
        let age = entry.measured.map(|at| now.saturating_duration_since(at));
        let state = if entry.failed {
            SampleState::Failed
        } else if age.is_some_and(|age| age < MAX_AGE) {
            SampleState::Current
        } else if age.is_some() {
            SampleState::Stale
        } else {
            SampleState::Checking
        };
        let mut report = entry.report.clone();
        if state != SampleState::Current {
            if let Some(report) = report.as_mut() {
                report.kind = PathKind::Unknown;
                report.rtt_ms = None;
                report.region = None;
            }
        }
        Some(PathSample { report, age, state })
    }

    pub fn latest(&self, address: &str) -> Option<PathReport> {
        self.sample(address, Instant::now()).and_then(|s| s.report)
    }
}

fn cache() -> &'static Mutex<PathCache> {
    static CACHE: OnceLock<Mutex<PathCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(PathCache::default()))
}

/// Ask for a fresh measurement of `address` if one is due. Never blocks the caller.
pub fn observe(address: &str, ctx: &egui::Context) {
    let now = Instant::now();
    let due = cache()
        .lock()
        .map(|mut c| c.begin_if_due(address, now))
        .unwrap_or(false);
    if !due {
        return;
    }
    let address = address.to_owned();
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        let result = measure(&address);
        if let Ok(mut c) = cache().lock() {
            match result {
                Ok(report) => c.finish(&address, report, Instant::now()),
                Err(_) => c.fail(&address, Instant::now()),
            }
        }
        ctx.request_repaint();
    });
}

pub fn latest(address: &str) -> Option<PathReport> {
    cache().lock().ok().and_then(|c| c.latest(address))
}

pub fn sample(address: &str) -> Option<PathSample> {
    cache()
        .lock()
        .ok()
        .and_then(|c| c.sample(address, Instant::now()))
}

fn measure(address: &str) -> Result<PathReport, String> {
    match transport_for(address) {
        Transport::Iroh => crate::iroh_route::path(address),
        Transport::Tailscale => crate::embedded_network::path(address),
        _ => health_round_trip(address).map(|ms| direct_report(address, Some(ms))),
    }
}

/// Two requests on one connection: the second one measures a single round trip.
fn health_round_trip(address: &str) -> Result<f32, String> {
    health_probe(address).map(|(ms, _)| ms)
}

/// Asks an address for its public health summary. Returns the round trip in
/// milliseconds and the first 16 KiB of the answer. Sends no secret.
pub fn health_probe(address: &str) -> Result<(f32, Vec<u8>), String> {
    health_probe_pinned(address, None)
}

/// Invitation probes resolve/check once, then pin those IPs for both requests.
pub fn health_probe_pinned(
    address: &str,
    addresses: Option<&[std::net::SocketAddr]>,
) -> Result<(f32, Vec<u8>), String> {
    use std::io::Read;
    let base = url::Url::parse(address).map_err(|_| "The address is not valid.")?;
    if !matches!(base.scheme(), "http" | "https" | "ws" | "wss") {
        return Err("The address is not valid.".into());
    }
    let mut health = base.clone();
    let _ = health.set_scheme(if matches!(base.scheme(), "https" | "wss") {
        "https"
    } else {
        "http"
    });
    // Reverse-proxied services such as Friends keep their base path. Replacing
    // it with /api/health would probe the account gateway instead of /people.
    let path = format!("{}/api/health", base.path().trim_end_matches('/'));
    health.set_path(&path);
    health.set_query(None);
    health.set_fragment(None);
    let mut builder = reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .redirect(reqwest::redirect::Policy::none());
    if let (Some(name), Some(addresses)) = (base.domain(), addresses) {
        builder = builder.resolve_to_addrs(name, addresses);
    }
    let client = builder
        .build()
        .map_err(|_| "Cannot measure the connection.")?;
    client
        .get(health.clone())
        .send()
        .map_err(|_| "The host did not answer.")?;
    let started = Instant::now();
    let response = client
        .get(health)
        .send()
        .map_err(|_| "The host did not answer.")?;
    if !response.status().is_success() {
        return Err("The host did not answer.".into());
    }
    let mut body = Vec::new();
    response
        .take(16_385)
        .read_to_end(&mut body)
        .map_err(|_| "The host did not answer.")?;
    let ms = started.elapsed().as_secs_f32() * 1000.0;
    if body.len() > 16_384 {
        return Err("The host answered with too much data.".into());
    }
    Ok((ms, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(
        transport: Transport,
        kind: PathKind,
        region: Option<&str>,
        rtt: Option<f32>,
    ) -> PathReport {
        PathReport {
            transport,
            kind,
            region: region.map(str::to_owned),
            rtt_ms: rtt,
            same_network: false,
        }
    }

    #[test]
    fn headline_names_the_route_and_the_delay_in_plain_words() {
        assert_eq!(
            report(Transport::Tailscale, PathKind::Direct, None, Some(11.6)).headline(),
            "Tailscale · direct · 12 ms"
        );
        assert_eq!(
            report(
                Transport::Tailscale,
                PathKind::Relay,
                Some("fra"),
                Some(41.0)
            )
            .headline(),
            "Tailscale · via relay (Frankfurt) · 41 ms"
        );
        assert_eq!(
            report(Transport::Iroh, PathKind::Relay, None, Some(48.0)).headline(),
            "iroh · via relay · 48 ms"
        );
        assert_eq!(
            report(Transport::Tailscale, PathKind::Unknown, None, None).headline(),
            "Tailscale · measuring…"
        );
        let mut home = report(Transport::Direct, PathKind::Direct, None, Some(2.0));
        home.same_network = true;
        assert_eq!(home.headline(), "Home network · 2 ms");
        assert_eq!(
            report(Transport::Direct, PathKind::Direct, None, Some(31.0)).headline(),
            "Direct address · 31 ms"
        );
    }

    #[test]
    fn advice_appears_only_when_something_can_be_done() {
        assert!(report(
            Transport::Tailscale,
            PathKind::Relay,
            Some("fra"),
            Some(40.0)
        )
        .hint()
        .unwrap()
        .contains("UPnP"));
        assert!(
            report(Transport::Tailscale, PathKind::Direct, None, Some(10.0))
                .hint()
                .is_none()
        );
        assert!(
            report(Transport::Direct, PathKind::Direct, None, Some(10.0))
                .hint()
                .is_none()
        );
        assert!(
            report(Transport::Direct, PathKind::Direct, None, Some(300.0))
                .hint()
                .is_some()
        );
        let mut home = report(Transport::Direct, PathKind::Direct, None, Some(300.0));
        home.same_network = true;
        assert!(home.hint().is_none());
        assert!(
            report(Transport::Tailscale, PathKind::PeerRelay, None, Some(20.0))
                .hint()
                .is_some()
        );
    }

    #[test]
    fn quality_follows_the_delay_and_unknown_never_looks_good() {
        assert_eq!(
            report(Transport::Direct, PathKind::Direct, None, Some(60.0)).quality(),
            Quality::Good
        );
        assert_eq!(
            report(Transport::Direct, PathKind::Direct, None, Some(60.1)).quality(),
            Quality::Fair
        );
        assert_eq!(
            report(Transport::Direct, PathKind::Direct, None, Some(120.1)).quality(),
            Quality::Poor
        );
        assert_eq!(
            report(Transport::Direct, PathKind::Direct, None, None).quality(),
            Quality::Unknown
        );
        assert_eq!(
            report(Transport::Tailscale, PathKind::Unknown, None, None).quality(),
            Quality::Unknown
        );
    }

    #[test]
    fn region_names_never_pass_free_text_through() {
        assert_eq!(region_name("fra"), "Frankfurt");
        assert_eq!(region_name("zzz"), "ZZZ");
        assert_eq!(region_name("a b<script>"), "ABSCRIPT");
        assert_eq!(region_name(&"x".repeat(40)).len(), 16);
    }

    #[test]
    fn home_network_addresses_are_recognised() {
        for host in [
            "192.168.1.50",
            "10.0.0.4",
            "172.16.9.1",
            "127.0.0.1",
            "169.254.1.2",
            "localhost",
            "[::1]",
            "fd12:3456::1",
            "fe80::1",
        ] {
            assert!(is_same_network(host), "{host}");
        }
        for host in [
            "8.8.8.8",
            "203.0.113.9",
            "2001:db8::1",
            "example.org",
            "100.100.5.6",
        ] {
            assert!(!is_same_network(host), "{host}");
        }
    }

    #[test]
    fn transport_is_chosen_from_the_address() {
        assert_eq!(
            transport_for("http://100.100.5.6:4580"),
            Transport::Tailscale
        );
        assert_eq!(transport_for("http://192.168.1.50:4580"), Transport::Direct);
        assert_eq!(transport_for("https://chat.example.org"), Transport::Direct);
        assert_eq!(transport_for("not a url"), Transport::Direct);
    }

    #[test]
    fn helper_answers_are_checked_before_they_are_shown() {
        let ok: HelperPath =
            serde_json::from_str(r#"{"kind":"derp","region":"fra","rtt_ms":41.2}"#).unwrap();
        let r = ok.into_report();
        assert_eq!(
            (r.kind, r.region.as_deref(), r.transport),
            (PathKind::Relay, Some("fra"), Transport::Tailscale)
        );
        let hostile: HelperPath =
            serde_json::from_str(r#"{"kind":"derp","region":"FRA <b>","rtt_ms":-3}"#).unwrap();
        let r = hostile.into_report();
        assert_eq!((r.kind, r.region, r.rtt_ms), (PathKind::Relay, None, None));
        let unknown = HelperPath {
            kind: "made-up".into(),
            region: Some("fra".into()),
            rtt_ms: f64::INFINITY,
        };
        let r = unknown.into_report();
        assert_eq!(
            (r.kind, r.region, r.rtt_ms),
            (PathKind::Unknown, None, None)
        );
        let direct: HelperPath =
            serde_json::from_str(r#"{"kind":"direct","region":"fra","rtt_ms":9}"#).unwrap();
        assert_eq!(
            direct.into_report().region,
            None,
            "a region belongs to a relay only"
        );
    }

    #[test]
    fn measurements_are_throttled_and_a_failure_never_looks_like_a_good_result() {
        let mut cache = PathCache::default();
        let start = Instant::now();
        let a = "http://100.100.5.6:4580";
        assert!(cache.begin_if_due(a, start));
        assert!(
            !cache.begin_if_due(a, start + Duration::from_secs(1)),
            "one measurement at a time"
        );
        cache.finish(
            a,
            report(Transport::Tailscale, PathKind::Direct, None, Some(10.0)),
            start + Duration::from_secs(1),
        );
        assert!(
            !cache.begin_if_due(a, start + Duration::from_secs(3)),
            "calm interval"
        );
        assert!(cache.begin_if_due(a, start + Duration::from_secs(7)));
        cache.fail(a, start + Duration::from_secs(8));
        let latest = cache.latest(a).unwrap();
        assert_eq!((latest.kind, latest.rtt_ms), (PathKind::Unknown, None));
        // A measurement that never answered is retried, but not at once.
        let b = "http://192.168.1.2:4580";
        assert!(cache.begin_if_due(b, start));
        assert!(!cache.begin_if_due(b, start + Duration::from_secs(10)));
        assert!(cache.begin_if_due(b, start + Duration::from_secs(16)));
    }

    #[test]
    fn the_cache_is_bounded() {
        let mut cache = PathCache::default();
        let now = Instant::now();
        for i in 0..16 {
            assert!(cache.begin_if_due(&format!("http://10.0.0.{i}:4580"), now));
        }
        // A seventeenth address replaces the oldest one instead of growing the cache.
        assert!(cache.begin_if_due("http://10.0.1.1:4580", now + Duration::from_secs(1)));
        assert_eq!(cache.entries.len(), 16);
        assert!(cache.entries.contains_key("http://10.0.1.1:4580"));
    }

    #[test]
    fn stale_success_and_failed_refresh_do_not_publish_a_current_ping() {
        let mut cache = PathCache::default();
        let start = Instant::now();
        let address = "https://accounts.invalid/people";
        assert!(cache.begin_if_due(address, start));
        cache.finish(address, direct_report(address, Some(12.0)), start);
        let current = cache
            .sample(address, start + Duration::from_secs(2))
            .unwrap();
        assert_eq!(current.state, SampleState::Current);
        assert_eq!(current.report.unwrap().rtt_ms, Some(12.0));
        let stale = cache.sample(address, start + MAX_AGE).unwrap();
        assert_eq!(stale.state, SampleState::Stale);
        assert_eq!(stale.report.unwrap().rtt_ms, None);
        assert!(cache.begin_if_due(address, start + MAX_AGE));
        cache.fail(address, start + MAX_AGE + Duration::from_secs(1));
        let failed = cache
            .sample(address, start + MAX_AGE + Duration::from_secs(2))
            .unwrap();
        assert_eq!(failed.state, SampleState::Failed);
        assert_eq!(failed.age, Some(MAX_AGE + Duration::from_secs(2)));
        assert_eq!(failed.report.unwrap().kind, PathKind::Unknown);
    }

    #[test]
    fn a_first_failed_check_is_not_reported_as_still_measuring() {
        let mut cache = PathCache::default();
        let now = Instant::now();
        let address = "https://accounts.invalid/people";
        cache.begin_if_due(address, now);
        cache.fail(address, now);
        let failed = cache.sample(address, now).unwrap();
        assert_eq!(failed.state, SampleState::Failed);
        assert!(failed.report.is_none());
        assert!(failed.age.is_none());
    }

    #[test]
    fn an_ordinary_address_reports_a_direct_path_with_its_round_trip() {
        let r = direct_report("http://192.168.1.50:4580", Some(3.0));
        assert_eq!(
            (r.kind, r.same_network, r.transport),
            (PathKind::Direct, true, Transport::Direct)
        );
        let r = direct_report("http://203.0.113.9:4580", None);
        assert_eq!((r.kind, r.same_network), (PathKind::Unknown, false));
    }

    /// A tiny local server standing in for a community: it answers health requests
    /// on one kept-alive connection, as the real server does.
    fn serve_health(status: &'static str) -> (String, std::thread::JoinHandle<()>) {
        serve_health_at(status, "/api/health")
    }

    fn serve_health_at(
        status: &'static str,
        path: &'static str,
    ) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut buffer = [0u8; 2048];
            for _ in 0..2 {
                let n = socket.read(&mut buffer).unwrap_or(0);
                if n == 0 || !buffer[..n].starts_with(format!("GET {path} ").as_bytes()) {
                    return;
                }
                let body = if status.starts_with("200") {
                    "{\"ok\":true}"
                } else {
                    "no"
                };
                let reply = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                );
                if socket.write_all(reply.as_bytes()).is_err() {
                    return;
                }
            }
        });
        (address, handle)
    }

    #[test]
    fn a_running_server_gives_a_measured_round_trip() {
        let (address, server) = serve_health("200 OK");
        let report = measure(&address).expect("local server answers");
        server.join().unwrap();
        assert_eq!(
            (report.transport, report.kind, report.same_network),
            (Transport::Direct, PathKind::Direct, true)
        );
        let ms = report.rtt_ms.expect("a delay was measured");
        assert!(ms > 0.0 && ms < 2_000.0, "{ms}");
        assert!(report.headline().starts_with("Home network · "));
    }

    #[test]
    fn a_service_base_path_is_preserved_without_forwarding_query_or_fragment() {
        for suffix in ["/people", "/people/"] {
            let (address, server) = serve_health_at("200 OK", "/people/api/health");
            let result = health_probe(&format!("{address}{suffix}?unused=must-not-leak#private"));
            server.join().unwrap();
            let (_, body) = result.expect("the service health endpoint answers");
            assert_eq!(body.as_slice(), br#"{"ok":true}"#);
        }
    }

    #[test]
    fn a_server_that_refuses_or_is_missing_gives_no_delay_at_all() {
        let (address, server) = serve_health("503 Service Unavailable");
        assert!(measure(&address).is_err());
        server.join().unwrap();
        // Nothing listens on a port which was just released.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        assert!(measure(&format!("http://127.0.0.1:{port}")).is_err());
        assert!(measure("ftp://127.0.0.1:1").is_err());
    }
}

/// Display-only suggestion; never enables direct voice or exposes an address.
pub(crate) fn suggest_direct(
    path_relayed: bool,
    tcp_active: bool,
    direct_enabled: bool,
    private_network: bool,
) -> bool {
    (path_relayed || tcp_active) && !direct_enabled && !private_network
}
#[cfg(test)]
mod latency_hint_tests {
    #[test]
    fn direct_hint_is_only_shown_when_it_can_help() {
        for bits in 0..16 {
            let (relay, tcp, enabled, private) =
                (bits & 1 != 0, bits & 2 != 0, bits & 4 != 0, bits & 8 != 0);
            assert_eq!(
                super::suggest_direct(relay, tcp, enabled, private),
                (relay || tcp) && !enabled && !private
            );
        }
    }
}
