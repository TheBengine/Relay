//! Which of the ways to reach a community Relay should use.
//!
//! An invitation may offer several routes: a direct address, Tailscale, and an
//! account-free (iroh) route. In Automatic mode the friend never chooses. Relay
//! tries the cheap ones in parallel, checks that each answers as the right
//! community, and uses the fastest. The decisions here are pure and tested;
//! only `HealthProber` touches the network, and only with a health request that
//! carries no secret.
use crate::{
    community::normalize_url, connection_path, connection_setup::ServerSummary,
    invitation::Invitation,
};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// What the user chose in Settings → Connections.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionMode {
    /// Relay picks the fastest route that works.
    #[default]
    Automatic,
    Tailscale,
    Direct,
    Iroh,
}

impl ConnectionMode {
    pub const ALL: [ConnectionMode; 4] = [
        ConnectionMode::Automatic,
        ConnectionMode::Tailscale,
        ConnectionMode::Direct,
        ConnectionMode::Iroh,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Automatic => "Automatic (recommended)",
            Self::Tailscale => "Tailscale",
            Self::Direct => "Direct address",
            Self::Iroh => "iroh (no account)",
        }
    }

    pub fn explanation(self) -> &'static str {
        match self {
            Self::Automatic => "Relay tries every route your friend offers and uses the fastest one that works. You never have to choose.",
            Self::Tailscale => "Needs a free Tailscale account for you and your friend. Works almost everywhere.",
            Self::Direct => "The fastest when it works: your friend's address on the home network or the internet. It needs your friend's router to allow it.",
            Self::Iroh => "No account and no address. Relay finds your friend by a key and connects directly when it can.",
        }
    }
}

/// Saved with the other preferences.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionPrefs {
    #[serde(default)]
    pub mode: ConnectionMode,
    /// The user accepted that a login to a public address without HTTPS can be
    /// read on its way. Off by default; Automatic never needs it.
    #[serde(default)]
    pub allow_plain_public_login: bool,
    /// Host: further addresses to offer in new invitations, besides the main one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub offer: Vec<String>,
}

/// The host's list, one address per line. Every line must be valid, so a typo
/// is shown instead of being left out of an invitation without a word.
pub fn parse_offer_lines(text: &str) -> Result<Vec<String>, String> {
    let mut found: Vec<String> = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let address = crate::invitation_routes::valid_direct_address(line).map_err(|_| {
            "Each line must be an https:// address, or an http:// address ending in :4580."
                .to_string()
        })?;
        if !found.contains(&address) {
            found.push(address);
        }
    }
    if found.len() > crate::invitation_routes::MAX_ALTERNATIVES {
        return Err(format!(
            "At most {} extra addresses fit in one invitation.",
            crate::invitation_routes::MAX_ALTERNATIVES
        ));
    }
    Ok(found)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteKind {
    Tailscale,
    Direct,
    Iroh,
}

impl RouteKind {
    fn name(self) -> &'static str {
        match self {
            Self::Tailscale => "Tailscale",
            Self::Direct => "direct address",
            Self::Iroh => "iroh",
        }
    }

    /// For a sentence such as "Using a direct connection (12 ms)".
    fn phrase(self) -> &'static str {
        match self {
            Self::Tailscale => "Tailscale",
            Self::Direct => "a direct connection",
            Self::Iroh => "iroh",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub kind: RouteKind,
    pub address: String,
}

/// What this build of Relay can use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    pub iroh: bool,
}

impl Capabilities {
    pub fn current() -> Self {
        Self {
            iroh: crate::iroh_route::available(),
        }
    }
}

/// The pseudo address an iroh route is known by inside Relay. It is never looked
/// up in DNS: Relay always maps it to its own loopback gateway.
pub fn iroh_address(endpoint_id: &str) -> String {
    format!("http://{endpoint_id}.iroh:4580")
}

/// Every route an invitation offers, in the order the host listed them.
pub fn candidates(invitation: &Invitation) -> Vec<Candidate> {
    let mut found: Vec<Candidate> = invitation
        .routes()
        .into_iter()
        .map(|address| Candidate {
            kind: match connection_path::transport_for(&address) {
                connection_path::Transport::Tailscale => RouteKind::Tailscale,
                _ => RouteKind::Direct,
            },
            address,
        })
        .collect();
    if let Some(iroh) = &invitation.iroh {
        found.push(Candidate {
            kind: RouteKind::Iroh,
            address: iroh_address(&iroh.endpoint_id),
        });
    }
    found
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Safety {
    /// HTTPS, Tailscale or iroh: the login is encrypted on its way.
    Encrypted,
    /// Plain HTTP, but inside this home network.
    HomeNetwork,
    /// Plain HTTP over the internet: passwords and sessions can be read.
    PlainOverInternet,
}

pub fn safety(candidate: &Candidate) -> Safety {
    match candidate.kind {
        RouteKind::Tailscale | RouteKind::Iroh => Safety::Encrypted,
        RouteKind::Direct => match url::Url::parse(&candidate.address) {
            Ok(url) if url.scheme() == "https" => Safety::Encrypted,
            Ok(url) if url.host_str().is_some_and(connection_path::is_same_network) => {
                Safety::HomeNetwork
            }
            _ => Safety::PlainOverInternet,
        },
    }
}

fn allowed(candidate: &Candidate, prefs: &ConnectionPrefs) -> bool {
    safety(candidate) != Safety::PlainOverInternet || prefs.allow_plain_public_login
}

pub const PLAIN_PUBLIC_MESSAGE: &str = "The only address which answers sends your password without encryption over the internet. Ask your friend for an https address or Tailscale, or allow it in Settings → Connections.";

#[derive(Debug, PartialEq, Eq)]
pub enum Plan {
    /// One route: nothing to measure.
    Use(Candidate),
    /// Several: measure them, then `choose`.
    Probe(Vec<Candidate>),
    Unavailable(String),
}

pub fn plan(all: &[Candidate], mode: ConnectionMode, caps: Capabilities) -> Plan {
    let wanted: Vec<Candidate> = all
        .iter()
        .filter(|c| match mode {
            ConnectionMode::Automatic => c.kind != RouteKind::Iroh || caps.iroh,
            ConnectionMode::Tailscale => c.kind == RouteKind::Tailscale,
            ConnectionMode::Direct => c.kind == RouteKind::Direct,
            ConnectionMode::Iroh => c.kind == RouteKind::Iroh,
        })
        .cloned()
        .collect();
    if mode == ConnectionMode::Iroh && !caps.iroh {
        return Plan::Unavailable(
            "This version of Relay cannot use iroh yet. Choose Automatic in Settings → Connections."
                .into(),
        );
    }
    match wanted.len() {
        0 if mode == ConnectionMode::Automatic => Plan::Unavailable(
            "This invitation offers no route this version of Relay can use. Update Relay and try again."
                .into(),
        ),
        0 => Plan::Unavailable(format!(
            "This invitation has no {} route. Ask your friend to turn it on, or choose Automatic in Settings → Connections.",
            match mode {
                ConnectionMode::Tailscale => "Tailscale",
                ConnectionMode::Direct => "direct address",
                _ => "iroh",
            }
        )),
        1 => Plan::Use(wanted.into_iter().next().unwrap()),
        _ if mode == ConnectionMode::Tailscale => Plan::Use(wanted.into_iter().next().unwrap()),
        _ => Plan::Probe(wanted),
    }
}

/// The only route is used as is, but never against the safety setting.
pub fn check_single(candidate: &Candidate, prefs: &ConnectionPrefs) -> Result<(), String> {
    if allowed(candidate, prefs) {
        Ok(())
    } else {
        Err(PLAIN_PUBLIC_MESSAGE.into())
    }
}

#[derive(Clone, Debug)]
pub struct Probe {
    pub candidate: Candidate,
    /// The round trip in milliseconds, or why the route cannot be used.
    pub outcome: Result<f32, String>,
}

fn rank(candidate: &Candidate) -> u8 {
    match (candidate.kind, safety(candidate)) {
        (RouteKind::Direct, Safety::HomeNetwork) => 0,
        (RouteKind::Direct, Safety::Encrypted) => 1,
        (RouteKind::Iroh, _) => 2,
        (RouteKind::Tailscale, _) => 3,
        (RouteKind::Direct, Safety::PlainOverInternet) => 4,
    }
}

/// The fastest reachable route which is allowed. Routes within a few
/// milliseconds of each other count as equal, and the better kind wins, so the
/// choice does not flip between two almost identical paths.
pub fn choose(probes: &[Probe], prefs: &ConnectionPrefs) -> Result<Candidate, String> {
    let reachable: Vec<(&Candidate, f32)> = probes
        .iter()
        .filter(|p| p.candidate.kind != RouteKind::Tailscale)
        .filter_map(|p| match &p.outcome {
            Ok(rtt) if rtt.is_finite() && *rtt >= 0.0 => Some((&p.candidate, *rtt)),
            _ => None,
        })
        .collect();
    let usable: Vec<&(&Candidate, f32)> = reachable
        .iter()
        .filter(|(c, _)| allowed(c, prefs))
        .collect();
    if let Some(fastest) = usable.iter().map(|(_, rtt)| *rtt).min_by(f32::total_cmp) {
        let window = fastest + (fastest * 0.25).max(5.0);
        let best = usable
            .iter()
            .filter(|(_, rtt)| *rtt <= window)
            .min_by(|a, b| rank(a.0).cmp(&rank(b.0)).then(a.1.total_cmp(&b.1)))
            .expect("the fastest route is inside its own window");
        return Ok(best.0.clone());
    }
    // Nothing else answers: Tailscale needs a sign-in, so it is the last resort.
    if let Some(tailscale) = probes
        .iter()
        .find(|p| p.candidate.kind == RouteKind::Tailscale)
    {
        return Ok(tailscale.candidate.clone());
    }
    if !reachable.is_empty() {
        return Err(PLAIN_PUBLIC_MESSAGE.into());
    }
    Err(unreachable_message(probes))
}

fn unreachable_message(probes: &[Probe]) -> String {
    let mut parts: Vec<String> = probes
        .iter()
        .filter_map(|p| {
            p.outcome
                .as_ref()
                .err()
                .map(|reason| format!("{}: {reason}", p.candidate.kind.name()))
        })
        .collect();
    parts.dedup();
    if parts.is_empty() {
        "Cannot reach this community. Ask your friend to keep Relay open.".into()
    } else {
        format!(
            "Cannot reach this community. Ask your friend to keep Relay open. ({})",
            parts.join("; ")
        )
    }
}

pub trait Prober: Sync {
    fn probe(&self, candidate: &Candidate) -> Result<f32, String>;
}

/// Measures every route at once. No route waits for another, so the time spent
/// is the slowest single probe, not the sum.
pub fn run_probes(prober: &dyn Prober, candidates: &[Candidate]) -> Vec<Probe> {
    std::thread::scope(|scope| {
        let handles: Vec<_> = candidates
            .iter()
            .map(|candidate| scope.spawn(move || prober.probe(candidate)))
            .collect();
        candidates
            .iter()
            .zip(handles)
            .map(|(candidate, handle)| Probe {
                candidate: candidate.clone(),
                outcome: handle
                    .join()
                    .unwrap_or_else(|_| Err("the check failed".into())),
            })
            .collect()
    })
}

/// Asks each address for its public health summary and insists that it is the
/// community the invitation is for. Sends no secret.
pub struct HealthProber {
    pub expected_server_id: String,
    /// The invitation's account-free route, which can be measured too.
    pub iroh: Option<crate::iroh_route::SavedIroh>,
    pub primary_port: u16,
    pub subnets: Vec<crate::invitation_probe::Subnet>,
}

impl HealthProber {
    /// Measures what an invitation offers, including its account-free route.
    pub fn for_invitation(invitation: &Invitation) -> Self {
        Self {
            expected_server_id: invitation.server_id.clone(),
            primary_port: url::Url::parse(&invitation.server)
                .ok()
                .and_then(|u| u.port_or_known_default())
                .unwrap_or(4580),
            subnets: crate::invitation_probe::local_subnets(),
            iroh: invitation
                .iroh
                .as_ref()
                .map(crate::iroh_route::SavedIroh::from_invitation),
        }
    }
}

impl Prober for HealthProber {
    fn probe(&self, candidate: &Candidate) -> Result<f32, String> {
        match candidate.kind {
            RouteKind::Tailscale => Err("needs sign-in".into()),
            RouteKind::Iroh => match &self.iroh {
                Some(saved) => crate::iroh_route::probe(saved),
                None => Err("not available in this version".into()),
            },
            RouteKind::Direct => {
                let address = normalize_url(&candidate.address)?;
                let addresses =
                    crate::invitation_probe::resolve(&address, self.primary_port, &self.subnets)?;
                let (rtt, body) = connection_path::health_probe_pinned(&address, Some(&addresses))
                    .map_err(|_| "no answer".to_string())?;
                let summary: ServerSummary = serde_json::from_slice(&body)
                    .map_err(|_| "this is not a Relay community".to_string())?;
                summary
                    .validate(Some(&self.expected_server_id))
                    .map_err(|_| "a different community answered".to_string())?;
                Ok(rtt)
            }
        }
    }
}

/// The finished routing decision for one invitation. No `Debug`: the invitation
/// holds secrets.
pub struct Outcome {
    /// The invitation with the chosen route as its primary address.
    pub invitation: Result<Invitation, String>,
    /// One line for the user, for example "Using your home network (2 ms)".
    pub summary: String,
    /// The only address which answers would send the password in plain text over
    /// the internet. The user may still accept that for this one invitation.
    pub needs_consent: bool,
}

/// The whole decision, run on a background thread by the caller.
pub fn decide(
    invitation: &Invitation,
    prefs: &ConnectionPrefs,
    caps: Capabilities,
    prober: &dyn Prober,
) -> Outcome {
    let all = candidates(invitation);
    let fail = |message: String| Outcome {
        needs_consent: message == PLAIN_PUBLIC_MESSAGE,
        invitation: Err(message.clone()),
        summary: message,
    };
    let (chosen, rtt) = match plan(&all, prefs.mode, caps) {
        Plan::Unavailable(message) => return fail(message),
        Plan::Use(candidate) => {
            if let Err(message) = check_single(&candidate, prefs) {
                return fail(message);
            }
            (candidate, None)
        }
        Plan::Probe(list) => {
            let probes = run_probes(prober, &list);
            match choose(&probes, prefs) {
                Ok(candidate) => {
                    let rtt = probes
                        .iter()
                        .find(|p| p.candidate == candidate)
                        .and_then(|p| p.outcome.as_ref().ok().copied());
                    (candidate, rtt)
                }
                Err(message) => return fail(message),
            }
        }
    };
    let summary = match (chosen.kind, rtt) {
        (RouteKind::Direct, Some(ms)) if safety(&chosen) == Safety::HomeNetwork => {
            format!("Using your home network ({} ms)", ms.round() as u32)
        }
        (RouteKind::Iroh, Some(ms)) => {
            format!("Using iroh · connected in {} ms", ms.round() as u32)
        }
        (kind, Some(ms)) => format!("Using {} ({} ms)", kind.phrase(), ms.round() as u32),
        (kind, None) => format!("Using {}", kind.phrase()),
    };
    match invitation.using_route(&chosen.address) {
        Ok(invitation) => Outcome {
            invitation: Ok(invitation),
            summary,
            needs_consent: false,
        },
        // An iroh route has no address in the invitation: it is used as the
        // primary address through its pseudo address by the caller.
        Err(_) if chosen.kind == RouteKind::Iroh => {
            let mut moved = invitation.clone();
            moved.alt = invitation.routes();
            moved.server = chosen.address.clone();
            Outcome {
                invitation: Ok(moved),
                summary,
                needs_consent: false,
            }
        }
        Err(message) => fail(message),
    }
}

/// A decision running on its own thread. The interface polls it each frame.
pub struct Job {
    /// What the user pasted, kept so a refusal can offer to continue anyway.
    pub original: Invitation,
    rx: std::sync::mpsc::Receiver<Outcome>,
    started: std::time::Instant,
    /// Held only by a display-only preview, so it keeps waiting.
    _idle: Option<std::sync::mpsc::Sender<Outcome>>,
}

impl Job {
    /// A decision which never answers, for offscreen captures of the waiting
    /// state. Nothing is measured or sent.
    pub fn preview(original: Invitation) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let now = std::time::Instant::now();
        Self {
            original,
            rx,
            started: now.checked_sub(Duration::from_secs(3)).unwrap_or(now),
            _idle: Some(tx),
        }
    }

    pub fn start(
        original: Invitation,
        prefs: ConnectionPrefs,
        caps: Capabilities,
        wake: impl Fn() + Send + 'static,
    ) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let invitation = original.clone();
        std::thread::spawn(move || {
            let prober = HealthProber::for_invitation(&invitation);
            let _ = tx.send(decide(&invitation, &prefs, caps, &prober));
            wake();
        });
        Self {
            original,
            rx,
            started: std::time::Instant::now(),
            _idle: None,
        }
    }

    /// The finished decision, once. A thread which ended without an answer is a failure.
    pub fn poll(&mut self) -> Option<Outcome> {
        match self.rx.try_recv() {
            Ok(outcome) => Some(outcome),
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(Outcome {
                invitation: Err("Relay could not check the connection. Try again.".into()),
                summary: String::new(),
                needs_consent: false,
            }),
        }
    }

    pub fn waited(&self) -> Duration {
        self.started.elapsed()
    }
}

/// A route which was refused only by the safety setting. The invitation is kept
/// so the user can accept the risk for this one invitation.
pub struct Refused {
    pub invitation: Invitation,
    pub message: String,
    pub overridable: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const ID: &str = "4be44aef-8c98-4db6-a9d5-81c2eb311330";

    fn c(kind: RouteKind, address: &str) -> Candidate {
        Candidate {
            kind,
            address: address.into(),
        }
    }
    fn ok(candidate: Candidate, ms: f32) -> Probe {
        Probe {
            candidate,
            outcome: Ok(ms),
        }
    }
    fn no(candidate: Candidate, why: &str) -> Probe {
        Probe {
            candidate,
            outcome: Err(why.into()),
        }
    }
    fn auto() -> ConnectionPrefs {
        ConnectionPrefs::default()
    }

    const HOME: &str = "http://192.168.1.50:4580";
    const PUBLIC_PLAIN: &str = "http://203.0.113.9:4580";
    const PUBLIC_TLS: &str = "https://chat.example.org";
    const TAILNET: &str = "http://100.70.1.2:4580";

    #[test]
    fn defaults_are_automatic_and_never_allow_a_plain_login_over_the_internet() {
        let prefs = ConnectionPrefs::default();
        assert_eq!(prefs.mode, ConnectionMode::Automatic);
        assert!(!prefs.allow_plain_public_login);
        // Old preference files without the new settings still load.
        let old: ConnectionPrefs = serde_json::from_str("{}").unwrap();
        assert_eq!(old, prefs);
        let saved = serde_json::to_string(&ConnectionPrefs {
            mode: ConnectionMode::Iroh,
            allow_plain_public_login: true,
            offer: Vec::new(),
        })
        .unwrap();
        assert_eq!(saved, r#"{"mode":"iroh","allow_plain_public_login":true}"#);
        for mode in ConnectionMode::ALL {
            assert!(!mode.label().is_empty() && !mode.explanation().is_empty());
        }
    }

    #[test]
    fn the_hosts_address_list_is_checked_line_by_line() {
        let list = parse_offer_lines(
            "  https://chat.example.org \n\n http://192.168.1.50:4580\nhttps://chat.example.org/\n",
        )
        .unwrap();
        assert_eq!(
            list,
            ["https://chat.example.org", "http://192.168.1.50:4580"]
        );
        assert!(parse_offer_lines("").unwrap().is_empty());
        for bad in [
            "chat.example.org",
            "http://192.168.1.50",
            "https://a.example.org\nftp://b",
            "http://203.0.113.9:22",
        ] {
            assert!(parse_offer_lines(bad).is_err(), "{bad}");
        }
        let five = (1..=5)
            .map(|i| format!("http://10.0.0.{i}:4580"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(parse_offer_lines(&five).unwrap_err().contains("At most 4"));
        // The list is saved only when there is one.
        let saved = serde_json::to_string(&ConnectionPrefs {
            offer: list.clone(),
            ..auto()
        })
        .unwrap();
        assert!(saved.contains("\"offer\""));
        let back: ConnectionPrefs = serde_json::from_str(&saved).unwrap();
        assert_eq!(back.offer, list);
    }

    #[test]
    fn safety_separates_encrypted_home_and_open_internet_logins() {
        assert_eq!(safety(&c(RouteKind::Direct, PUBLIC_TLS)), Safety::Encrypted);
        assert_eq!(safety(&c(RouteKind::Direct, HOME)), Safety::HomeNetwork);
        assert_eq!(
            safety(&c(RouteKind::Direct, "http://[fd12::1]:4580")),
            Safety::HomeNetwork
        );
        assert_eq!(
            safety(&c(RouteKind::Direct, PUBLIC_PLAIN)),
            Safety::PlainOverInternet
        );
        assert_eq!(
            safety(&c(RouteKind::Direct, "http://[2001:db8::1]:4580")),
            Safety::PlainOverInternet
        );
        assert_eq!(safety(&c(RouteKind::Tailscale, TAILNET)), Safety::Encrypted);
        assert_eq!(
            safety(&c(RouteKind::Iroh, &iroh_address(&"a".repeat(64)))),
            Safety::Encrypted
        );
    }

    #[test]
    fn the_fastest_reachable_route_wins_in_automatic_mode() {
        let probes = [
            ok(c(RouteKind::Direct, PUBLIC_TLS), 40.0),
            ok(c(RouteKind::Iroh, &iroh_address(&"b".repeat(64))), 90.0),
            no(c(RouteKind::Direct, HOME), "no answer"),
        ];
        assert_eq!(choose(&probes, &auto()).unwrap().address, PUBLIC_TLS);
        let probes = [
            ok(c(RouteKind::Direct, PUBLIC_TLS), 90.0),
            ok(c(RouteKind::Iroh, &iroh_address(&"b".repeat(64))), 30.0),
        ];
        assert_eq!(choose(&probes, &auto()).unwrap().kind, RouteKind::Iroh);
    }

    #[test]
    fn routes_within_a_few_milliseconds_are_equal_so_the_better_kind_wins() {
        let iroh = c(RouteKind::Iroh, &iroh_address(&"b".repeat(64)));
        // 2 ms on the home network against 4 ms by iroh: the home network.
        let probes = [ok(iroh.clone(), 2.0), ok(c(RouteKind::Direct, HOME), 4.0)];
        assert_eq!(choose(&probes, &auto()).unwrap().address, HOME);
        // 100 vs 120 ms is within a quarter: the better kind (https) wins.
        let probes = [
            ok(iroh.clone(), 100.0),
            ok(c(RouteKind::Direct, PUBLIC_TLS), 120.0),
        ];
        assert_eq!(choose(&probes, &auto()).unwrap().address, PUBLIC_TLS);
        // But a clearly faster route is not given up for a better kind.
        let probes = [ok(iroh, 20.0), ok(c(RouteKind::Direct, PUBLIC_TLS), 80.0)];
        assert_eq!(choose(&probes, &auto()).unwrap().kind, RouteKind::Iroh);
    }

    #[test]
    fn a_plain_login_over_the_internet_is_never_chosen_unless_the_user_allowed_it() {
        let only_plain = [ok(c(RouteKind::Direct, PUBLIC_PLAIN), 15.0)];
        assert!(choose(&only_plain, &auto())
            .unwrap_err()
            .contains("without encryption"));
        let allowed_prefs = ConnectionPrefs {
            allow_plain_public_login: true,
            ..auto()
        };
        assert_eq!(
            choose(&only_plain, &allowed_prefs).unwrap().address,
            PUBLIC_PLAIN
        );
        // Even when allowed, an encrypted route of similar speed is preferred.
        let both = [
            ok(c(RouteKind::Direct, PUBLIC_PLAIN), 15.0),
            ok(c(RouteKind::Direct, PUBLIC_TLS), 17.0),
        ];
        assert_eq!(choose(&both, &allowed_prefs).unwrap().address, PUBLIC_TLS);
        // A slower but safe route beats a faster plain one that is not allowed.
        let mixed = [
            ok(c(RouteKind::Direct, PUBLIC_PLAIN), 5.0),
            ok(c(RouteKind::Iroh, &iroh_address(&"b".repeat(64))), 70.0),
        ];
        assert_eq!(choose(&mixed, &auto()).unwrap().kind, RouteKind::Iroh);
    }

    #[test]
    fn tailscale_is_the_last_resort_because_it_needs_a_sign_in() {
        let tail = c(RouteKind::Tailscale, TAILNET);
        let probes = [
            no(c(RouteKind::Direct, HOME), "no answer"),
            no(tail.clone(), "needs sign-in"),
        ];
        assert_eq!(choose(&probes, &auto()).unwrap(), tail);
        // A reachable route is preferred over Tailscale, however fast Tailscale may be.
        let probes = [
            ok(c(RouteKind::Direct, HOME), 3.0),
            no(tail, "needs sign-in"),
        ];
        assert_eq!(choose(&probes, &auto()).unwrap().address, HOME);
    }

    #[test]
    fn when_nothing_answers_the_message_says_what_failed_without_addresses() {
        let probes = [
            no(c(RouteKind::Direct, HOME), "no answer"),
            no(c(RouteKind::Direct, PUBLIC_TLS), "no answer"),
            no(
                c(RouteKind::Iroh, &iroh_address(&"b".repeat(64))),
                "a different community answered",
            ),
        ];
        let message = choose(&probes, &auto()).unwrap_err();
        assert!(message.contains("keep Relay open"), "{message}");
        assert!(message.contains("direct address: no answer"), "{message}");
        assert!(
            message.contains("iroh: a different community answered"),
            "{message}"
        );
        assert!(
            !message.contains("192.168") && !message.contains("example.org"),
            "{message}"
        );
    }

    fn all_routes() -> Vec<Candidate> {
        vec![
            c(RouteKind::Direct, HOME),
            c(RouteKind::Direct, PUBLIC_TLS),
            c(RouteKind::Tailscale, TAILNET),
            c(RouteKind::Iroh, &iroh_address(&"b".repeat(64))),
        ]
    }

    #[test]
    fn plans_follow_the_chosen_mode_and_what_this_build_can_do() {
        let with = Capabilities { iroh: true };
        let without = Capabilities { iroh: false };
        let Plan::Probe(list) = plan(&all_routes(), ConnectionMode::Automatic, with) else {
            panic!()
        };
        assert_eq!(list.len(), 4);
        // A build without iroh quietly leaves that route out in Automatic mode.
        let Plan::Probe(list) = plan(&all_routes(), ConnectionMode::Automatic, without) else {
            panic!()
        };
        assert_eq!(list.len(), 3);
        assert!(list.iter().all(|c| c.kind != RouteKind::Iroh));
        // Forced modes use only their own kind.
        let Plan::Use(tailscale) = plan(&all_routes(), ConnectionMode::Tailscale, with) else {
            panic!()
        };
        assert_eq!(tailscale.kind, RouteKind::Tailscale);
        let Plan::Probe(direct) = plan(&all_routes(), ConnectionMode::Direct, with) else {
            panic!()
        };
        assert!(direct.iter().all(|c| c.kind == RouteKind::Direct) && direct.len() == 2);
        let Plan::Use(iroh) = plan(&all_routes(), ConnectionMode::Iroh, with) else {
            panic!()
        };
        assert_eq!(iroh.kind, RouteKind::Iroh);
        // A single route needs no measuring.
        assert!(matches!(
            plan(
                &[c(RouteKind::Direct, HOME)],
                ConnectionMode::Automatic,
                with
            ),
            Plan::Use(_)
        ));
    }

    #[test]
    fn a_missing_route_says_what_to_do_instead_of_failing_silently() {
        let home_only = [c(RouteKind::Direct, HOME)];
        let Plan::Unavailable(message) = plan(
            &home_only,
            ConnectionMode::Tailscale,
            Capabilities { iroh: true },
        ) else {
            panic!()
        };
        assert!(
            message.contains("no Tailscale route") && message.contains("Automatic"),
            "{message}"
        );
        let Plan::Unavailable(message) = plan(
            &home_only,
            ConnectionMode::Iroh,
            Capabilities { iroh: true },
        ) else {
            panic!()
        };
        assert!(message.contains("no iroh route"), "{message}");
        let Plan::Unavailable(message) = plan(
            &all_routes(),
            ConnectionMode::Iroh,
            Capabilities { iroh: false },
        ) else {
            panic!()
        };
        assert!(message.contains("cannot use iroh yet"), "{message}");
        let iroh_only = [c(RouteKind::Iroh, &iroh_address(&"b".repeat(64)))];
        let Plan::Unavailable(message) = plan(
            &iroh_only,
            ConnectionMode::Automatic,
            Capabilities { iroh: false },
        ) else {
            panic!()
        };
        assert!(message.contains("Update Relay"), "{message}");
    }

    #[test]
    fn the_only_route_is_still_held_to_the_safety_setting() {
        assert!(check_single(&c(RouteKind::Direct, HOME), &auto()).is_ok());
        assert!(check_single(&c(RouteKind::Direct, PUBLIC_TLS), &auto()).is_ok());
        assert!(check_single(&c(RouteKind::Direct, PUBLIC_PLAIN), &auto()).is_err());
        assert!(check_single(
            &c(RouteKind::Direct, PUBLIC_PLAIN),
            &ConnectionPrefs {
                allow_plain_public_login: true,
                ..auto()
            }
        )
        .is_ok());
    }

    struct FakeProber {
        delays: HashMap<String, Result<f32, String>>,
        started: std::sync::Mutex<Vec<String>>,
    }
    impl Prober for FakeProber {
        fn probe(&self, candidate: &Candidate) -> Result<f32, String> {
            self.started.lock().unwrap().push(candidate.address.clone());
            self.delays
                .get(&candidate.address)
                .cloned()
                .unwrap_or_else(|| Err("no answer".into()))
        }
    }

    #[test]
    fn probes_run_side_by_side_and_keep_their_order() {
        struct Slow;
        impl Prober for Slow {
            fn probe(&self, _: &Candidate) -> Result<f32, String> {
                std::thread::sleep(Duration::from_millis(300));
                Ok(1.0)
            }
        }
        let started = std::time::Instant::now();
        let probes = run_probes(&Slow, &all_routes());
        assert_eq!(probes.len(), 4);
        assert!(
            started.elapsed() < Duration::from_millis(900),
            "probes ran one after another: {:?}",
            started.elapsed()
        );
        assert_eq!(
            probes
                .iter()
                .map(|p| p.candidate.address.clone())
                .collect::<Vec<_>>(),
            all_routes()
                .iter()
                .map(|c| c.address.clone())
                .collect::<Vec<_>>()
        );
        struct Panics;
        impl Prober for Panics {
            fn probe(&self, _: &Candidate) -> Result<f32, String> {
                panic!("a probe failed");
            }
        }
        let probes = run_probes(&Panics, &all_routes()[..1]);
        assert!(
            probes[0].outcome.is_err(),
            "a panicking check is a failed check, not a crash"
        );
    }

    fn invitation() -> Invitation {
        Invitation::new(HOME, ID, "relay-secret")
            .unwrap()
            .with_alternatives(&[PUBLIC_TLS.to_owned(), TAILNET.to_owned()])
            .unwrap()
    }

    #[test]
    fn deciding_picks_the_fastest_route_and_says_so_in_plain_words() {
        let prober = FakeProber {
            delays: HashMap::from([
                (HOME.to_owned(), Err("no answer".into())),
                (PUBLIC_TLS.to_owned(), Ok(42.4)),
            ]),
            started: Default::default(),
        };
        let outcome = decide(
            &invitation(),
            &auto(),
            Capabilities { iroh: false },
            &prober,
        );
        let chosen = outcome.invitation.unwrap();
        assert_eq!(chosen.server, PUBLIC_TLS);
        assert_eq!(outcome.summary, "Using a direct connection (42 ms)");
        assert!(
            chosen.routes().contains(&HOME.to_owned()),
            "the other routes stay available"
        );
        // Tailscale needs a sign-in, so it never counts as measured.
        assert_eq!(prober.started.lock().unwrap().len(), 3);
    }

    #[test]
    fn an_offline_invitation_can_be_retried_unchanged_when_the_host_returns() {
        let original = Invitation::new(HOME, ID, "owned-retry-code")
            .unwrap()
            .with_alternatives(&[PUBLIC_TLS.to_owned()])
            .unwrap();
        let before = original.link();
        let mut prober = FakeProber {
            delays: HashMap::new(),
            started: Default::default(),
        };
        let prefs = auto();
        let failed = decide(&original, &prefs, Capabilities { iroh: false }, &prober);
        assert!(failed.invitation.is_err() && !failed.needs_consent);
        prober.delays.insert(PUBLIC_TLS.to_owned(), Ok(22.0));
        let retried = decide(&original, &prefs, Capabilities { iroh: false }, &prober);
        let chosen = retried.invitation.unwrap();
        assert_eq!(chosen.server, PUBLIC_TLS);
        assert!(chosen.server_id == original.server_id && chosen.code == original.code);
        assert!(original.link() == before);
        assert!(!prefs.allow_plain_public_login && !retried.needs_consent);
        assert_eq!(
            prober.started.lock().unwrap().len(),
            4,
            "two bounded attempts, two routes each"
        );
    }

    #[test]
    fn deciding_names_the_home_network_when_it_wins() {
        let prober = FakeProber {
            delays: HashMap::from([
                (HOME.to_owned(), Ok(1.6)),
                (PUBLIC_TLS.to_owned(), Ok(30.0)),
            ]),
            started: Default::default(),
        };
        let outcome = decide(
            &invitation(),
            &auto(),
            Capabilities { iroh: false },
            &prober,
        );
        assert_eq!(outcome.invitation.unwrap().server, HOME);
        assert_eq!(outcome.summary, "Using your home network (2 ms)");
    }

    #[test]
    fn deciding_falls_back_to_tailscale_when_nothing_else_answers() {
        let prober = FakeProber {
            delays: HashMap::new(),
            started: Default::default(),
        };
        let outcome = decide(
            &invitation(),
            &auto(),
            Capabilities { iroh: false },
            &prober,
        );
        assert_eq!(outcome.invitation.unwrap().server, TAILNET);
        assert_eq!(outcome.summary, "Using Tailscale");
    }

    #[test]
    fn a_forced_mode_never_falls_back_to_another_kind() {
        let prober = FakeProber {
            delays: HashMap::new(),
            started: Default::default(),
        };
        let direct = ConnectionPrefs {
            mode: ConnectionMode::Direct,
            ..auto()
        };
        let outcome = decide(
            &invitation(),
            &direct,
            Capabilities { iroh: false },
            &prober,
        );
        assert!(
            outcome.invitation.is_err(),
            "Direct was forced and nothing answered"
        );
        let tailscale = ConnectionPrefs {
            mode: ConnectionMode::Tailscale,
            ..auto()
        };
        let fresh = FakeProber {
            delays: HashMap::new(),
            started: Default::default(),
        };
        let outcome = decide(
            &invitation(),
            &tailscale,
            Capabilities { iroh: false },
            &fresh,
        );
        assert_eq!(outcome.invitation.unwrap().server, TAILNET);
        assert!(
            fresh.started.lock().unwrap().is_empty(),
            "a forced Tailscale route needs no measuring"
        );
    }

    #[test]
    fn an_invitation_with_one_plain_public_address_stops_before_any_login() {
        let invitation = Invitation::new(PUBLIC_PLAIN, ID, "relay-secret").unwrap();
        let prober = FakeProber {
            delays: HashMap::new(),
            started: Default::default(),
        };
        let outcome = decide(&invitation, &auto(), Capabilities { iroh: false }, &prober);
        assert!(outcome
            .invitation
            .err()
            .unwrap()
            .contains("without encryption"));
        let allowed_prefs = ConnectionPrefs {
            allow_plain_public_login: true,
            ..auto()
        };
        assert!(decide(
            &invitation,
            &allowed_prefs,
            Capabilities { iroh: false },
            &prober
        )
        .invitation
        .is_ok());
    }

    #[test]
    fn candidates_cover_every_route_of_an_invitation() {
        let iroh = crate::invitation_routes::IrohRoute::new(
            &"b".repeat(64),
            None,
            "abcdefghijklmnopqrstuvwxyzABCDEF0123456789_-",
        )
        .unwrap();
        let list = candidates(&invitation().with_iroh(iroh));
        assert_eq!(
            list.iter().map(|c| c.kind).collect::<Vec<_>>(),
            [
                RouteKind::Direct,
                RouteKind::Direct,
                RouteKind::Tailscale,
                RouteKind::Iroh
            ]
        );
        assert_eq!(list[3].address, iroh_address(&"b".repeat(64)));
    }

    #[test]
    fn a_background_job_delivers_one_decision_and_a_lost_thread_is_a_failure() {
        let prober_free = Invitation::new(HOME, ID, "relay-secret").unwrap();
        let woken = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = woken.clone();
        // A single route needs no network, so this finishes on its own.
        let mut job = Job::start(
            prober_free.clone(),
            auto(),
            Capabilities { iroh: false },
            move || flag.store(true, std::sync::atomic::Ordering::SeqCst),
        );
        let started = std::time::Instant::now();
        let outcome = loop {
            if let Some(outcome) = job.poll() {
                break outcome;
            }
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "the job never finished"
            );
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(outcome.invitation.unwrap().server, HOME);
        assert!(
            woken.load(std::sync::atomic::Ordering::SeqCst),
            "the interface is woken when the answer is ready"
        );
        assert!(job.original == prober_free);
        // A plain public address stops with an offer to continue, not with a dead end.
        let plain = Invitation::new(PUBLIC_PLAIN, ID, "relay-secret").unwrap();
        let mut job = Job::start(plain, auto(), Capabilities { iroh: false }, || {});
        let outcome = loop {
            if let Some(outcome) = job.poll() {
                break outcome;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(outcome.needs_consent && outcome.invitation.is_err());
        // A thread which ended without sending is reported, not waited for forever.
        let (tx, rx) = std::sync::mpsc::channel::<Outcome>();
        drop(tx);
        let mut lost = Job {
            original: prober_free,
            rx,
            started: std::time::Instant::now(),
            _idle: None,
        };
        // A preview keeps waiting instead.
        let mut waiting = Job::preview(lost.original.clone());
        assert!(waiting.poll().is_none() && waiting.waited() >= Duration::from_secs(2));
        let outcome = lost.poll().expect("a closed channel is a failure");
        assert!(outcome.invitation.is_err() && !outcome.needs_consent);
    }

    #[test]
    fn a_real_server_is_accepted_only_as_the_expected_community() {
        use std::io::{Read, Write};
        let serve = |server_id: &'static str| {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address = format!("http://{}", listener.local_addr().unwrap());
            let handle = std::thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut buffer = [0u8; 2048];
                for _ in 0..2 {
                    if socket.read(&mut buffer).unwrap_or(0) == 0 {
                        return;
                    }
                    let body = format!(
                        r#"{{"ok":true,"server_name":"Test","server_id":"{server_id}","protocol_version":{},"rtc_enabled":true}}"#,
                        relay_protocol::API_PROTOCOL_VERSION
                    );
                    let reply = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    );
                    if socket.write_all(reply.as_bytes()).is_err() {
                        return;
                    }
                }
            });
            (address, handle)
        };
        let (address, server) = serve("4be44aef-8c98-4db6-a9d5-81c2eb311330");
        let mut prober = HealthProber {
            expected_server_id: ID.into(),
            iroh: None,
            primary_port: url::Url::parse(&address).unwrap().port().unwrap(),
            subnets: Vec::new(),
        };
        let rtt = prober
            .probe(&c(RouteKind::Direct, &address))
            .expect("the right community answers");
        server.join().unwrap();
        assert!((0.0..2_000.0).contains(&rtt));
        let (address, server) = serve("11111111-2222-3333-4444-555555555555");
        prober.primary_port = url::Url::parse(&address).unwrap().port().unwrap();
        let error = prober.probe(&c(RouteKind::Direct, &address)).unwrap_err();
        server.join().unwrap();
        assert_eq!(error, "a different community answered");
        assert_eq!(
            prober.probe(&c(RouteKind::Tailscale, TAILNET)).unwrap_err(),
            "needs sign-in"
        );
        assert!(prober
            .probe(&c(RouteKind::Iroh, &iroh_address(&"b".repeat(64))))
            .is_err());
    }
}
