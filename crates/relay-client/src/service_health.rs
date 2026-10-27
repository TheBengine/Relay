//! Live health per service, kept apart: community, Relay account, Friends and video.
//!
//! Two kinds of evidence are recorded and never mixed:
//! - An HTTP check: the service's public health address answered (or not).
//!   It proves the service's web side is reachable. It says nothing about
//!   calls, voice or video.
//! - Media evidence: video frames actually arrived on this PC. Only this can
//!   show that video works. Video never gets an HTTP check here.
//!
//! Checks run only while the Help page's status is on screen (or on "Check
//! now"), at most every 30 seconds per service, in a background thread with a
//! 3 second timeout. Nothing is sent but a plain GET; no token or name.
//! Samples older than the window are dropped, so the support report can say
//! exactly which period it covers.
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub(super) const WINDOW: Duration = Duration::from_secs(15 * 60);
pub(super) const CHECK_EVERY: Duration = Duration::from_secs(30);
/// Frames older than this do not count as "arriving now".
pub(super) const FRAME_FRESH: Duration = Duration::from_secs(3);
const MAX_SAMPLES: usize = 512;

pub(super) const HTTP_NOTE: &str = "An HTTP check shows the service answers. It does not test calls, voice or video.";
pub(super) const MEDIA_NOTE: &str = "Video counts as working only when frames arrive on this PC. No HTTP check is used for video.";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Service {
    Community,
    Account,
    Friends,
    Video,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Media {
    /// Frames from this many streams arrived within `FRAME_FRESH`.
    Receiving { streams: u8 },
    /// In a call with video expected, but no fresh frames.
    NoFrames,
    /// Not in a call, or nobody is sharing video. Nothing to measure.
    NothingToMeasure,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Outcome {
    Answered { ms: u32 },
    Failed,
    /// This route cannot be checked over HTTP (for example the account-free
    /// route). Not a failure and not a success.
    NotCheckable,
    Media(Media),
}

impl Outcome {
    fn is_http(self) -> bool {
        matches!(self, Self::Answered { .. } | Self::Failed)
    }
}

#[derive(Clone, Copy, Debug)]
struct Sample {
    service: Service,
    at: Instant,
    outcome: Outcome,
}

/// Counts for one service within the window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Summary {
    pub answered: u16,
    pub failed: u16,
    pub median_ms: Option<u32>,
    pub last: Option<(Outcome, Duration)>,
    pub media_samples: u16,
    pub media_seen: u16,
}

impl Summary {
    pub(super) fn checks(&self) -> u16 {
        self.answered.saturating_add(self.failed)
    }
}

type Finished = Arc<Mutex<Vec<(Service, Outcome, Instant)>>>;

#[derive(Default)]
pub(super) struct HealthLog {
    samples: VecDeque<Sample>,
    started: HashMap<Service, Instant>,
    running: HashSet<Service>,
    finished: Finished,
}

impl HealthLog {
    pub(super) fn record(&mut self, service: Service, outcome: Outcome, now: Instant) {
        self.prune(now);
        self.samples.push_back(Sample { service, at: now, outcome });
        while self.samples.len() > MAX_SAMPLES {
            self.samples.pop_front();
        }
    }

    fn prune(&mut self, now: Instant) {
        while self
            .samples
            .front()
            .is_some_and(|s| now.saturating_duration_since(s.at) > WINDOW)
        {
            self.samples.pop_front();
        }
    }

    /// Whole minutes from the oldest kept sample to `now`, or None when
    /// nothing was measured. Never more than the window.
    pub(super) fn window_minutes(&self, now: Instant) -> Option<u16> {
        let oldest = self
            .samples
            .iter()
            .find(|s| now.saturating_duration_since(s.at) <= WINDOW)?;
        let span = now.saturating_duration_since(oldest.at).min(WINDOW);
        Some(u16::try_from(span.as_secs().div_ceil(60)).unwrap_or(15).max(1))
    }

    pub(super) fn summary(&self, service: Service, now: Instant) -> Summary {
        let mut summary = Summary::default();
        let mut times = Vec::new();
        for sample in self
            .samples
            .iter()
            .filter(|s| s.service == service && now.saturating_duration_since(s.at) <= WINDOW)
        {
            match sample.outcome {
                Outcome::Answered { ms } => {
                    summary.answered = summary.answered.saturating_add(1);
                    times.push(ms);
                }
                Outcome::Failed => summary.failed = summary.failed.saturating_add(1),
                // Nothing shared means nothing measured: not a sample at all.
                Outcome::NotCheckable | Outcome::Media(Media::NothingToMeasure) => {}
                Outcome::Media(media) => {
                    summary.media_samples = summary.media_samples.saturating_add(1);
                    if matches!(media, Media::Receiving { .. }) {
                        summary.media_seen = summary.media_seen.saturating_add(1);
                    }
                }
            }
            summary.last = Some((sample.outcome, now.saturating_duration_since(sample.at)));
        }
        times.sort_unstable();
        summary.median_ms = times.get(times.len() / 2).copied();
        summary
    }

    pub(super) fn due(&self, service: Service, now: Instant) -> bool {
        !self.running.contains(&service)
            && self
                .started
                .get(&service)
                .is_none_or(|at| now.saturating_duration_since(*at) >= CHECK_EVERY)
    }

    /// Run one HTTP check in the background. Video is refused: it is never
    /// judged by an HTTP answer.
    pub(super) fn start(
        &mut self,
        service: Service,
        now: Instant,
        job: impl FnOnce() -> Outcome + Send + 'static,
    ) -> bool {
        if service == Service::Video || !self.due(service, now) {
            return false;
        }
        self.started.insert(service, now);
        self.running.insert(service);
        let finished = self.finished.clone();
        let spawned = std::thread::Builder::new()
            .name("relay-service-health".into())
            .spawn(move || {
                let mut outcome = job();
                if !outcome.is_http() && outcome != Outcome::NotCheckable {
                    outcome = Outcome::Failed;
                }
                if let Ok(mut list) = finished.lock() {
                    list.push((service, outcome, Instant::now()));
                }
            });
        if spawned.is_err() {
            self.running.remove(&service);
            self.record(service, Outcome::Failed, now);
        }
        true
    }

    /// Move finished checks into the log. Returns true when something arrived.
    pub(super) fn collect(&mut self) -> bool {
        let done = self
            .finished
            .lock()
            .map(|mut list| std::mem::take(&mut *list))
            .unwrap_or_default();
        let any = !done.is_empty();
        for (service, outcome, at) in done {
            self.running.remove(&service);
            self.record(service, outcome, at);
        }
        any
    }

    pub(super) fn running(&self, service: Service) -> bool {
        self.running.contains(&service)
    }

    /// Record what video frames show, at most once per `CHECK_EVERY`.
    pub(super) fn sample_media(&mut self, media: Media, now: Instant) -> bool {
        if !self.due(Service::Video, now) {
            return false;
        }
        self.started.insert(Service::Video, now);
        self.record(Service::Video, Outcome::Media(media), now);
        true
    }

    /// Forget the schedule so the next draw checks everything again.
    pub(super) fn check_now(&mut self) {
        self.started.clear();
    }
}

/// Video evidence from the tiles the app already draws: how many remote
/// streams delivered a frame within `FRAME_FRESH`.
pub(super) fn media_evidence(
    in_call: bool,
    expected_streams: usize,
    frame_ages: impl IntoIterator<Item = Duration>,
) -> Media {
    let fresh = frame_ages.into_iter().filter(|age| *age <= FRAME_FRESH).count();
    if fresh > 0 {
        Media::Receiving { streams: u8::try_from(fresh).unwrap_or(u8::MAX) }
    } else if in_call && expected_streams > 0 {
        Media::NoFrames
    } else {
        Media::NothingToMeasure
    }
}

fn ago(age: Duration) -> String {
    match age.as_secs() {
        0..=4 => "just now".into(),
        s @ 5..=59 => format!("{s} s ago"),
        s => format!("{} min ago", s / 60),
    }
}

/// One line for the Help page about the live HTTP check.
pub(super) fn http_line(summary: &Summary, running: bool) -> String {
    let last = match summary.last {
        Some((Outcome::Answered { ms }, age)) => format!("answered in {ms} ms, {}", ago(age)),
        Some((Outcome::Failed, age)) => format!("did not answer, {}", ago(age)),
        Some((Outcome::NotCheckable, _)) => {
            return "Live check: not available on this route; the state above is what the app last recorded.".into()
        }
        _ if running => return "Live check: checking…".into(),
        _ => return "Live check: not run yet.".into(),
    };
    format!(
        "Live check: {last}. Last 15 min: {} of {} answered.",
        summary.answered,
        summary.checks()
    )
}

/// One line for the Help page about video, from frames only.
pub(super) fn media_line(now: Media, summary: &Summary) -> String {
    let current = match now {
        Media::Receiving { streams: 1 } => "frames from 1 stream are arriving on this PC".into(),
        Media::Receiving { streams } => format!("frames from {streams} streams are arriving on this PC"),
        Media::NoFrames => "no frames are arriving, although video is shared in this call".into(),
        Media::NothingToMeasure => "nothing to measure (no shared video in your call)".into(),
    };
    if summary.media_samples == 0 {
        format!("Media: {current}.")
    } else {
        format!(
            "Media: {current}. Last 15 min: frames seen in {} of {} samples.",
            summary.media_seen, summary.media_samples
        )
    }
}

/// A plain GET of the account provider's public discovery document. Success
/// needs a 2xx answer whose `issuer` is exactly the configured issuer.
pub(super) fn account_check(issuer: &url::Url) -> Outcome {
    let base = issuer.as_str().trim_end_matches('/');
    let address = format!("{base}/.well-known/openid-configuration");
    let Ok(client) = reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .redirect(reqwest::redirect::Policy::none())
        .build()
    else {
        return Outcome::Failed;
    };
    let started = Instant::now();
    let Ok(response) = client.get(address).send() else {
        return Outcome::Failed;
    };
    if !response.status().is_success() {
        return Outcome::Failed;
    }
    let ms = started.elapsed().as_millis();
    let Ok(body) = response.bytes() else {
        return Outcome::Failed;
    };
    let named = body.len() <= 65_536
        && serde_json::from_slice::<serde_json::Value>(&body).is_ok_and(|doc| {
            doc.get("issuer")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|value| value.trim_end_matches('/') == base)
        });
    if named {
        Outcome::Answered { ms: u32::try_from(ms).unwrap_or(u32::MAX) }
    } else {
        Outcome::Failed
    }
}

/// The service's `/api/health`, through the same probe the connection panel
/// uses. The account-free route has no HTTP address to check.
pub(super) fn service_check(address: &str) -> Outcome {
    if crate::connection_path::transport_for(address) == crate::connection_path::Transport::Iroh {
        return Outcome::NotCheckable;
    }
    match crate::connection_path::health_probe(address) {
        Ok((ms, _)) => Outcome::Answered { ms: ms.round().clamp(0.0, u32::MAX as f32) as u32 },
        Err(_) => Outcome::Failed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_outside_the_window_are_dropped_and_the_window_is_stated() {
        let start = Instant::now();
        let mut log = HealthLog::default();
        assert_eq!(log.window_minutes(start), None);
        log.record(Service::Community, Outcome::Answered { ms: 40 }, start);
        log.record(Service::Community, Outcome::Failed, start + Duration::from_secs(90));
        let now = start + Duration::from_secs(150);
        assert_eq!(log.window_minutes(now), Some(3));
        let summary = log.summary(Service::Community, now);
        assert_eq!((summary.answered, summary.failed, summary.checks()), (1, 1, 2));
        let later = start + WINDOW + Duration::from_secs(60);
        let summary = log.summary(Service::Community, later);
        assert_eq!((summary.answered, summary.failed), (0, 1));
        assert!(log.window_minutes(later).unwrap() <= 15);
        log.record(Service::Account, Outcome::Failed, start + WINDOW * 3);
        assert_eq!(log.summary(Service::Community, start + WINDOW * 3).checks(), 0);
    }

    #[test]
    fn services_are_counted_apart_and_a_median_is_kept() {
        let now = Instant::now();
        let mut log = HealthLog::default();
        for ms in [30, 10, 20] {
            log.record(Service::Friends, Outcome::Answered { ms }, now);
        }
        log.record(Service::Community, Outcome::Failed, now);
        let friends = log.summary(Service::Friends, now);
        assert_eq!((friends.answered, friends.failed, friends.median_ms), (3, 0, Some(20)));
        assert_eq!(log.summary(Service::Community, now).failed, 1);
        assert_eq!(log.summary(Service::Account, now), Summary::default());
    }

    #[test]
    fn video_is_never_checked_over_http() {
        let now = Instant::now();
        let mut log = HealthLog::default();
        assert!(!log.start(Service::Video, now, || Outcome::Answered { ms: 1 }));
        assert!(log.due(Service::Video, now));
        // A green HTTP answer for the community does not make video healthy.
        log.record(Service::Community, Outcome::Answered { ms: 5 }, now);
        // Not being in a call is not a failed video sample.
        assert!(log.sample_media(Media::NothingToMeasure, now));
        let video = log.summary(Service::Video, now);
        assert_eq!((video.answered, video.media_seen, video.media_samples), (0, 0, 0));
        assert_eq!(media_line(Media::NothingToMeasure, &video), "Media: nothing to measure (no shared video in your call).");
    }

    #[test]
    fn checks_are_spaced_and_results_arrive_from_the_background() {
        let now = Instant::now();
        let mut log = HealthLog::default();
        assert!(log.start(Service::Community, now, || Outcome::Answered { ms: 12 }));
        assert!(!log.start(Service::Community, now, || Outcome::Failed));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !log.collect() {
            assert!(Instant::now() < deadline, "the check never finished");
            std::thread::yield_now();
        }
        assert!(!log.running(Service::Community));
        assert!(!log.due(Service::Community, now + Duration::from_secs(5)));
        assert!(log.due(Service::Community, now + CHECK_EVERY));
        log.check_now();
        assert!(log.due(Service::Community, now));
        assert_eq!(log.summary(Service::Community, Instant::now()).answered, 1);
    }

    #[test]
    fn a_media_outcome_from_an_http_job_is_treated_as_a_failure() {
        let now = Instant::now();
        let mut log = HealthLog::default();
        log.start(Service::Friends, now, || Outcome::Media(Media::Receiving { streams: 1 }));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !log.collect() {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        let summary = log.summary(Service::Friends, Instant::now());
        assert_eq!((summary.failed, summary.media_seen), (1, 0));
    }

    #[test]
    fn media_evidence_needs_fresh_frames() {
        let fresh = Duration::from_millis(300);
        let stale = FRAME_FRESH + Duration::from_secs(1);
        assert_eq!(media_evidence(true, 2, [fresh, stale]), Media::Receiving { streams: 1 });
        assert_eq!(media_evidence(true, 1, [stale]), Media::NoFrames);
        assert_eq!(media_evidence(true, 0, []), Media::NothingToMeasure);
        assert_eq!(media_evidence(false, 0, [stale]), Media::NothingToMeasure);
    }

    #[test]
    fn lines_say_what_was_checked_and_never_claim_media_from_http() {
        let now = Instant::now();
        let mut log = HealthLog::default();
        assert_eq!(http_line(&log.summary(Service::Community, now), false), "Live check: not run yet.");
        assert_eq!(http_line(&log.summary(Service::Community, now), true), "Live check: checking…");
        log.record(Service::Community, Outcome::Answered { ms: 42 }, now);
        let line = http_line(&log.summary(Service::Community, now), false);
        assert_eq!(line, "Live check: answered in 42 ms, just now. Last 15 min: 1 of 1 answered.");
        log.record(Service::Friends, Outcome::NotCheckable, now);
        assert!(http_line(&log.summary(Service::Friends, now), false).contains("not available"));
        for text in [HTTP_NOTE, MEDIA_NOTE] {
            assert!(text.contains("video"));
        }
        assert!(HTTP_NOTE.contains("does not test"));
        log.record(Service::Video, Outcome::Media(Media::Receiving { streams: 2 }), now);
        log.record(Service::Video, Outcome::Media(Media::NoFrames), now);
        let video = log.summary(Service::Video, now);
        assert_eq!(
            media_line(Media::Receiving { streams: 2 }, &video),
            "Media: frames from 2 streams are arriving on this PC. Last 15 min: frames seen in 1 of 2 samples."
        );
    }

    #[test]
    fn the_account_check_requires_the_exact_issuer() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let issuer = url::Url::parse(&format!("http://{address}/realms/relay")).unwrap();
        let bodies = [
            format!("{{\"issuer\":\"{}\"}}", issuer.as_str().trim_end_matches('/')),
            "{\"issuer\":\"http://elsewhere.invalid/realms/relay\"}".to_owned(),
        ];
        let server = std::thread::spawn(move || {
            for body in bodies {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0; 2048];
                let read = stream.read(&mut request).unwrap();
                let text = String::from_utf8_lossy(&request[..read]).to_lowercase();
                assert!(text.starts_with("get /realms/relay/.well-known/openid-configuration "));
                assert!(!text.contains("authorization"));
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        assert!(matches!(account_check(&issuer), Outcome::Answered { .. }));
        assert_eq!(account_check(&issuer), Outcome::Failed);
        server.join().unwrap();
    }
}
