//! Fixed, redacted observations for the owned six-track generated-media probe.
//! No production capture, transport, codec, E2EE or packet timing is changed.
use super::*;
use livekit::webrtc::stats_presence::{InboundKind, InboundRtpObservation, OutboundRtpObservation};

const SAMPLE_SECONDS: f64 = 12.0;
const MIN_WINDOW_SECONDS: f64 = 24.0;
const MAX_SAMPLE_SKEW_SECONDS: f64 = 0.250;
const MAX_REPORT_AGE_SECONDS: f64 = 0.250;
const MAX_WALL_CLOCK_DIFFERENCE_SECONDS: f64 = 0.050;
// Root verified ToJson emits microseconds with the exact pinned native library.
// See native-stats-unit-20261023/result.jsonl and rtc_stats.h's UNIX epoch contract.
const NATIVE_TIMESTAMP_UNITS_PER_SECOND: Option<f64> = Some(1_000_000.0);

#[derive(Clone, Copy)]
enum Source {
    Audio,
    Camera,
    Screen,
}
impl Source {
    fn index(self) -> usize {
        match self {
            Self::Audio => 0,
            Self::Camera => 1,
            Self::Screen => 2,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::Audio => "audio",
            Self::Camera => "camera",
            Self::Screen => "screen",
        }
    }
    fn source(self) -> TrackSource {
        match self {
            Self::Audio => TrackSource::ScreenshareAudio,
            Self::Camera => TrackSource::Camera,
            Self::Screen => TrackSource::Screenshare,
        }
    }
    fn kind(self) -> InboundKind {
        if matches!(self, Self::Audio) {
            InboundKind::Audio
        } else {
            InboundKind::Video
        }
    }
    fn matches(
        self,
        publication: TrackSource,
        subscribed: TrackSource,
        track: TrackSource,
    ) -> bool {
        [publication, subscribed, track]
            .into_iter()
            .all(|actual| actual == self.source())
    }
}

// No Debug/Serialize: all publication, participant and native track identities
// stay in memory. Public output contains only fixed direction/source labels.
#[derive(Clone)]
struct Binding {
    publication: String,
    participant: String,
    native_track: String,
    sender_track: String,
}
impl Binding {
    fn same_as(&self, other: &Self) -> bool {
        self.publication == other.publication
            && self.participant == other.participant
            && self.native_track == other.native_track
            && self.sender_track == other.sender_track
    }
}
struct Request {
    slot: usize,
    source: Source,
    binding: Result<(Binding, LocalTrack, RemoteTrack), &'static str>,
}
struct Snapshot {
    binding: Binding,
    at: f64,
    sender_at: f64,
    query_span: f64,
    receiver_wall_midpoint: Option<f64>,
    sender_wall_midpoint: Option<f64>,
    report_age_bound: Result<f64, &'static str>,
    observation: InboundRtpObservation,
    sender: OutboundRtpObservation,
    auxiliary: Option<[usize; 3]>,
    sender_auxiliary: Option<[usize; 3]>,
}
struct Sample {
    slot: usize,
    result: Result<Snapshot, &'static str>,
}

fn wall_seconds() -> Option<f64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|time| time.as_secs_f64())
        .filter(|time| time.is_finite())
}
struct QueryBounds {
    start: f64,
    end: f64,
    wall_start: Option<f64>,
    wall_end: Option<f64>,
}
impl QueryBounds {
    fn wall_midpoint(&self) -> Option<f64> {
        Some((self.wall_start? + self.wall_end?) / 2.0)
    }
}
fn report_age_bound(timestamp: Option<f64>, bounds: &QueryBounds) -> Result<f64, &'static str> {
    let units = NATIVE_TIMESTAMP_UNITS_PER_SECOND.ok_or("native_timestamp_unit_unverified")?;
    let report = timestamp.ok_or("missing_native_report_timestamp")? / units;
    let (Some(start), Some(end)) = (bounds.wall_start, bounds.wall_end) else {
        return Err("native_query_wall_clock_unavailable");
    };
    let elapsed = bounds.end - bounds.start;
    if !elapsed.is_finite()
        || elapsed < 0.0
        || end < start
        || (end - start - elapsed).abs() > MAX_WALL_CLOCK_DIFFERENCE_SECONDS
    {
        return Err("native_query_wall_clock_jump");
    }
    let age = end - report;
    if !age.is_finite()
        || !(0.0..=MAX_REPORT_AGE_SECONDS).contains(&age)
        || report < start - MAX_REPORT_AGE_SECONDS
    {
        return Err("native_report_age_or_clock_base_unknown");
    }
    Ok(age)
}

fn request(owner: &Peer, receiver: &Peer, source: Source, slot: usize) -> Request {
    let expected: Vec<_> = if matches!(source, Source::Audio) {
        owner
            .audio
            .iter()
            .map(|(_, track, p)| (p, LocalTrack::Audio(track.clone())))
            .collect()
    } else {
        owner
            .video
            .iter()
            .filter(|p| p.publication.source() == source.source())
            .map(|p| (&p.publication, LocalTrack::Video(p._track.clone())))
            .collect()
    };
    let binding = (|| {
        if expected.len() != 1 {
            return Err("missing_or_ambiguous_publication");
        }
        let (publication, sender) = &expected[0];
        let sid = publication.sid().to_string();
        let subscribed = receiver
            .quality_tracks
            .get(&sid)
            .ok_or("publication_not_subscribed")?;
        if !source.matches(
            publication.source(),
            subscribed.source,
            subscribed.track.source(),
        ) || subscribed.participant != owner.keys.local_user_id
            || subscribed.track.sid().to_string() != sid
            || sender.sid().to_string() != sid
        {
            return Err("subscription_source_mismatch");
        }
        let native_track = subscribed.track.rtc_track().id();
        let sender_track = sender.rtc_track().id();
        if native_track.is_empty()
            || native_track.len() > 256
            || sender_track.is_empty()
            || sender_track.len() > 256
        {
            return Err("missing_native_track_identity");
        }
        Ok((
            Binding {
                publication: sid,
                participant: subscribed.participant.clone(),
                native_track,
                sender_track,
            },
            sender.clone(),
            subscribed.track.clone(),
        ))
    })();
    Request {
        slot,
        source,
        binding,
    }
}

fn still_matches(
    owner: &Peer,
    receiver: &Peer,
    source: Source,
    slot: usize,
    binding: &Binding,
) -> bool {
    request(owner, receiver, source, slot)
        .binding
        .is_ok_and(|(current, _, _)| current.same_as(binding))
}

async fn sample(request: Request, origin: Instant) -> Sample {
    let Request {
        slot,
        source,
        binding,
    } = request;
    let result = async {
        let (binding, sender, receiver) = binding?;
        let tx = async {
            let start = origin.elapsed().as_secs_f64();
            let wall_start = wall_seconds();
            let report = tokio::time::timeout(
                Duration::from_millis(750),
                sender.get_outbound_observations(),
            )
            .await
            .map_err(|_| "sender_statistics_deadline")?
            .map_err(|_| "sender_statistics_unavailable")?;
            Ok::<_, &'static str>((
                report,
                QueryBounds {
                    start,
                    end: origin.elapsed().as_secs_f64(),
                    wall_start,
                    wall_end: wall_seconds(),
                },
            ))
        };
        let rx = async {
            let start = origin.elapsed().as_secs_f64();
            let wall_start = wall_seconds();
            let report = tokio::time::timeout(
                Duration::from_millis(750),
                receiver.get_inbound_observations(),
            )
            .await
            .map_err(|_| "receiver_statistics_deadline")?
            .map_err(|_| "receiver_statistics_unavailable")?;
            Ok::<_, &'static str>((
                report,
                QueryBounds {
                    start,
                    end: origin.elapsed().as_secs_f64(),
                    wall_start,
                    wall_end: wall_seconds(),
                },
            ))
        };
        let (tx, rx) = tokio::join!(tx, rx);
        let (tx, tx_bounds) = tx?;
        let (rx, rx_bounds) = rx?;
        let observation = rx
            .select_track(&binding.native_track, source.kind())
            .map_err(|e| e.label())?
            .clone();
        let sender = tx
            .select_track(&binding.sender_track, source.kind())
            .map_err(|e| e.label())?
            .clone();
        let auxiliary = rx.auxiliary_records(&binding.native_track);
        let sender_auxiliary = tx.auxiliary_records(&binding.sender_track);
        let age_bound =
            report_age_bound(observation.native_timestamp(), &rx_bounds).and_then(|rx_age| {
                report_age_bound(sender.native_timestamp(), &tx_bounds)
                    .map(|tx_age| rx_age.max(tx_age))
            });
        Ok(Snapshot {
            binding,
            at: (rx_bounds.start + rx_bounds.end) / 2.0,
            sender_at: (tx_bounds.start + tx_bounds.end) / 2.0,
            query_span: (tx_bounds.end - tx_bounds.start).max(rx_bounds.end - rx_bounds.start),
            receiver_wall_midpoint: rx_bounds.wall_midpoint(),
            sender_wall_midpoint: tx_bounds.wall_midpoint(),
            report_age_bound: age_bound,
            observation,
            sender,
            auxiliary,
            sender_auxiliary,
        })
    }
    .await;
    Sample { slot, result }
}

#[derive(Clone, Copy, Debug)]
struct Measure {
    value: Option<f64>,
    unknown: Option<&'static str>,
}
impl Measure {
    fn unknown(reason: &'static str) -> Self {
        Self {
            value: None,
            unknown: Some(reason),
        }
    }
    fn value(value: f64) -> Self {
        if value.is_finite() && (0.0..=9_007_199_254_740_992.0).contains(&value) {
            Self {
                value: Some(value),
                unknown: None,
            }
        } else {
            Self::unknown("invalid_or_imprecise_counter")
        }
    }
    fn merge(self, next: Self) -> Self {
        match (self.value, next.value) {
            (Some(a), Some(b)) => Self::value(a + b),
            _ => Self::unknown(self.unknown.or(next.unknown).unwrap_or("unknown_counter")),
        }
    }
    fn divide(self, denominator: Self, scale: f64) -> Self {
        match (self.value, denominator.value) {
            (Some(n), Some(d)) if d > 0.0 => Self::value(scale * n / d),
            (Some(_), Some(_)) => Self::unknown("nonpositive_denominator"),
            _ => Self::unknown(
                self.unknown
                    .or(denominator.unknown)
                    .unwrap_or("unknown_counter"),
            ),
        }
    }
}
fn count_delta(before: Option<u64>, after: Option<u64>) -> Measure {
    match (before, after) {
        (Some(a), Some(b)) => match b.checked_sub(a) {
            Some(n) if n <= 9_007_199_254_740_991 => Measure::value(n as f64),
            Some(_) => Measure::unknown("invalid_or_imprecise_counter"),
            None => Measure::unknown("counter_reset_or_decrease"),
        },
        _ => Measure::unknown("missing_or_invalid_counter"),
    }
}
fn lost_delta(before: Option<i64>, after: Option<i64>) -> Measure {
    match (before, after) {
        (Some(a), Some(b)) => match b.checked_sub(a) {
            Some(n) if (0..=9_007_199_254_740_991).contains(&n) => Measure::value(n as f64),
            _ => Measure::unknown("loss_counter_reset_or_correction"),
        },
        _ => Measure::unknown("missing_or_invalid_counter"),
    }
}
fn seconds_delta(before: Option<f64>, after: Option<f64>) -> Measure {
    match (before, after) {
        (Some(a), Some(b)) if a.is_finite() && b.is_finite() && b >= a && a >= 0.0 => {
            Measure::value(b - a)
        }
        (Some(_), Some(_)) => Measure::unknown("counter_reset_or_decrease"),
        _ => Measure::unknown("missing_or_invalid_counter"),
    }
}
fn deltas(a: &InboundRtpObservation, b: &InboundRtpObservation) -> [Measure; 12] {
    let (a, b) = (&a.counters, &b.counters);
    [
        count_delta(a.packets_received, b.packets_received),
        lost_delta(a.packets_lost, b.packets_lost),
        count_delta(a.bytes_received, b.bytes_received),
        count_delta(a.frames_decoded, b.frames_decoded),
        count_delta(a.frames_dropped, b.frames_dropped),
        count_delta(a.freeze_count, b.freeze_count),
        seconds_delta(a.total_freeze_duration, b.total_freeze_duration),
        seconds_delta(a.jitter_buffer_delay, b.jitter_buffer_delay),
        count_delta(a.jitter_buffer_emitted_count, b.jitter_buffer_emitted_count),
        count_delta(a.total_samples_received, b.total_samples_received),
        count_delta(a.concealed_samples, b.concealed_samples),
        count_delta(
            a.retransmitted_packets_received,
            b.retransmitted_packets_received,
        ),
    ]
}
fn sender_deltas(a: &OutboundRtpObservation, b: &OutboundRtpObservation) -> [Measure; 7] {
    let (a, b) = (&a.counters, &b.counters);
    [
        count_delta(a.packets_sent, b.packets_sent),
        count_delta(a.bytes_sent, b.bytes_sent),
        count_delta(a.frames_encoded, b.frames_encoded),
        count_delta(a.frames_sent, b.frames_sent),
        count_delta(a.retransmitted_packets_sent, b.retransmitted_packets_sent),
        seconds_delta(a.total_encode_time, b.total_encode_time),
        seconds_delta(a.source_samples_duration, b.source_samples_duration),
    ]
}
fn assessed(measure: Measure, min: f64, max: Option<f64>, essential: bool) -> Value {
    let state = match measure.value {
        None => "unknown",
        Some(value) if value < min || max.is_some_and(|limit| value > limit) => "fail",
        Some(_) => "pass",
    };
    json!({"value":measure.value,"unknown_reason":measure.unknown,"scenario_assessment":state,
        "local_budget":{"minimum":min,"maximum":max},"essential_for_scenario":essential})
}

struct Window {
    previous: Option<Snapshot>,
    sum: [Measure; 12],
    sender_sum: [Measure; 7],
    duration: f64,
    sender_duration: f64,
    intervals: u32,
    fault: Option<&'static str>,
    codec: Option<&'static str>,
    auxiliary: Option<[usize; 3]>,
    sender_auxiliary: Option<[usize; 3]>,
    alignment_fault: Option<&'static str>,
    max_pair_skew: f64,
    max_query_span: f64,
    max_interval_difference: f64,
    timestamp_units_per_second: Option<f64>,
    max_report_age_bound: Option<f64>,
    timeline_fault: Option<&'static str>,
    native_duration: f64,
    native_sender_duration: f64,
    max_native_pair_skew: f64,
    max_native_query_difference: f64,
    native_pair_samples: u32,
    native_intervals: u32,
}
impl Default for Window {
    fn default() -> Self {
        Self {
            previous: None,
            sum: [Measure::unknown("no_interval"); 12],
            sender_sum: [Measure::unknown("no_interval"); 7],
            duration: 0.0,
            sender_duration: 0.0,
            intervals: 0,
            fault: None,
            codec: None,
            auxiliary: None,
            sender_auxiliary: None,
            alignment_fault: None,
            max_pair_skew: 0.0,
            max_query_span: 0.0,
            max_interval_difference: 0.0,
            timestamp_units_per_second: NATIVE_TIMESTAMP_UNITS_PER_SECOND,
            max_report_age_bound: None,
            timeline_fault: None,
            native_duration: 0.0,
            native_sender_duration: 0.0,
            max_native_pair_skew: 0.0,
            max_native_query_difference: 0.0,
            native_pair_samples: 0,
            native_intervals: 0,
        }
    }
}
impl Window {
    fn observe_native_timeline(&mut self, current: &Snapshot) {
        match current.report_age_bound {
            Ok(age) if age.is_finite() && (0.0..=MAX_REPORT_AGE_SECONDS).contains(&age) => {
                self.max_report_age_bound = Some(self.max_report_age_bound.unwrap_or(0.0).max(age))
            }
            Ok(_) => {
                self.timeline_fault
                    .get_or_insert("native_report_age_or_clock_base_unknown");
            }
            Err(reason) => {
                self.timeline_fault.get_or_insert(reason);
            }
        }
        let Some(units) = self
            .timestamp_units_per_second
            .filter(|units| units.is_finite() && *units > 0.0)
        else {
            self.timeline_fault
                .get_or_insert("native_timestamp_unit_unverified");
            return;
        };
        let (Some(rx), Some(tx)) = (
            current.observation.native_timestamp(),
            current.sender.native_timestamp(),
        ) else {
            self.timeline_fault
                .get_or_insert("missing_native_report_timestamp");
            return;
        };
        self.native_pair_samples += 1;
        self.max_native_pair_skew = self.max_native_pair_skew.max((rx - tx).abs() / units);
        if self.max_native_pair_skew > MAX_SAMPLE_SKEW_SECONDS {
            self.timeline_fault
                .get_or_insert("native_report_pair_timestamp_skew");
        }
        if let Some(previous) = self.previous.as_ref() {
            let (Some(previous_rx), Some(previous_tx)) = (
                previous.observation.native_timestamp(),
                previous.sender.native_timestamp(),
            ) else {
                self.timeline_fault
                    .get_or_insert("missing_native_report_timestamp");
                return;
            };
            let rx_interval = (rx - previous_rx) / units;
            let tx_interval = (tx - previous_tx) / units;
            for (before, after, monotonic) in [
                (
                    previous.receiver_wall_midpoint,
                    current.receiver_wall_midpoint,
                    current.at - previous.at,
                ),
                (
                    previous.sender_wall_midpoint,
                    current.sender_wall_midpoint,
                    current.sender_at - previous.sender_at,
                ),
            ] {
                match before.zip(after) {
                    Some((before, after))
                        if (after - before - monotonic).abs()
                            <= MAX_WALL_CLOCK_DIFFERENCE_SECONDS => {}
                    _ => {
                        self.timeline_fault
                            .get_or_insert("native_query_wall_clock_jump_or_missing");
                    }
                }
            }
            if !rx_interval.is_finite()
                || !tx_interval.is_finite()
                || rx_interval <= 0.0
                || tx_interval <= 0.0
            {
                self.timeline_fault
                    .get_or_insert("native_report_timestamp_stale_or_reset");
                return;
            }
            let difference = (rx_interval - (current.at - previous.at))
                .abs()
                .max((tx_interval - (current.sender_at - previous.sender_at)).abs());
            self.max_native_query_difference = self.max_native_query_difference.max(difference);
            if self.max_native_query_difference > 2.0 * MAX_SAMPLE_SKEW_SECONDS {
                self.timeline_fault
                    .get_or_insert("native_report_vs_query_interval_mismatch");
            }
            self.native_duration += rx_interval;
            self.native_sender_duration += tx_interval;
            self.native_intervals += 1;
        }
    }
    fn observe(&mut self, snapshot: Result<Snapshot, &'static str>) {
        let current = match snapshot {
            Ok(value) => value,
            Err(reason) => {
                self.fault.get_or_insert(reason);
                self.previous = None;
                return;
            }
        };
        self.observe_native_timeline(&current);
        self.codec = Some(current.observation.codec.label());
        self.auxiliary = current.auxiliary;
        self.sender_auxiliary = current.sender_auxiliary;
        self.max_pair_skew = self
            .max_pair_skew
            .max((current.at - current.sender_at).abs());
        self.max_query_span = self.max_query_span.max(current.query_span);
        if self.max_pair_skew > MAX_SAMPLE_SKEW_SECONDS
            || self.max_query_span > MAX_SAMPLE_SKEW_SECONDS
        {
            self.alignment_fault
                .get_or_insert("paired_sampling_skew_or_uncertainty");
        }
        if let Some(previous) = self.previous.as_ref() {
            let duration = current.at - previous.at;
            let sender_duration = current.sender_at - previous.sender_at;
            self.max_interval_difference = self
                .max_interval_difference
                .max((duration - sender_duration).abs());
            if self.max_interval_difference > 2.0 * MAX_SAMPLE_SKEW_SECONDS {
                self.alignment_fault
                    .get_or_insert("paired_interval_mismatch");
            }
            if !previous.binding.same_as(&current.binding)
                || !current.observation.same_stream_as(&previous.observation)
                || !current.sender.same_stream_as(&previous.sender)
                || current.sender.codec != current.observation.codec
            {
                self.fault.get_or_insert("stream_identity_changed");
            } else if !duration.is_finite()
                || duration <= 0.0
                || !sender_duration.is_finite()
                || sender_duration <= 0.0
            {
                self.fault.get_or_insert("nonpositive_observation_interval");
            } else {
                let next = deltas(&previous.observation, &current.observation);
                let tx_next = sender_deltas(&previous.sender, &current.sender);
                for (old, new) in self.sum.iter_mut().zip(next) {
                    *old = if self.intervals == 0 {
                        new
                    } else {
                        old.merge(new)
                    };
                }
                for (old, new) in self.sender_sum.iter_mut().zip(tx_next) {
                    *old = if self.intervals == 0 {
                        new
                    } else {
                        old.merge(new)
                    };
                }
                self.duration += duration;
                self.sender_duration += sender_duration;
                self.intervals += 1;
            }
        }
        self.previous = Some(current);
    }
    fn report(&self, source: Source) -> Value {
        let sums = self.sum.map(|m| self.fault.map_or(m, Measure::unknown));
        let tx = self
            .sender_sum
            .map(|m| self.fault.map_or(m, Measure::unknown));
        let elapsed = self
            .timeline_fault
            .map_or(Measure::value(self.native_duration), Measure::unknown);
        let tx_elapsed = self.timeline_fault.map_or(
            Measure::value(self.native_sender_duration),
            Measure::unknown,
        );
        let mut metrics = serde_json::Map::new();
        for (label, value, min, max, essential) in [
            (
                "reported_primary_packets_received_delta",
                sums[0],
                1.0,
                None,
                true,
            ),
            (
                "reported_primary_packets_sent_delta",
                tx[0],
                1.0,
                None,
                true,
            ),
            (
                "reported_primary_packets_lost_delta",
                sums[1],
                0.0,
                None,
                false,
            ),
            (
                "reported_primary_loss_percent",
                sums[1].divide(sums[0].merge(sums[1]), 100.0),
                0.0,
                Some(2.0),
                true,
            ),
            (
                "reported_primary_inbound_kbps",
                sums[2].divide(elapsed, 0.008),
                0.0,
                None,
                false,
            ),
            (
                "reported_primary_outbound_kbps",
                tx[1].divide(tx_elapsed, 0.008),
                0.0,
                None,
                false,
            ),
            (
                "jitter_buffer_mean_ms",
                sums[7].divide(sums[8], 1000.0),
                0.0,
                Some(80.0),
                true,
            ),
            (
                "reported_retransmitted_packets_received_delta",
                sums[11],
                0.0,
                None,
                false,
            ),
            (
                "reported_retransmitted_packets_sent_delta",
                tx[4],
                0.0,
                None,
                false,
            ),
        ] {
            metrics.insert(label.into(), assessed(value, min, max, essential));
        }
        if matches!(source, Source::Audio) {
            metrics.insert(
                "total_samples_received_delta".into(),
                assessed(sums[9], 1.0, None, true),
            );
            metrics.insert(
                "concealed_samples_delta".into(),
                assessed(sums[10], 0.0, None, false),
            );
            let concealment = if sums[10]
                .value
                .zip(sums[9].value)
                .is_some_and(|(n, d)| n > d)
            {
                Measure::unknown("concealed_exceeds_corresponding_samples")
            } else {
                sums[10].divide(sums[9], 100.0)
            };
            metrics.insert(
                "concealed_samples_percent".into(),
                assessed(concealment, 0.0, Some(2.0), true),
            );
            metrics.insert(
                "sender_source_samples_duration_seconds_delta".into(),
                assessed(tx[6], 0.0, None, false),
            );
            metrics.insert(
                "audio_encoded_decoded_frame_comparison".into(),
                assessed(
                    Measure::unknown("native_audio_encoder_frame_counter_not_exposed"),
                    0.0,
                    None,
                    false,
                ),
            );
        } else {
            let aligned_ratio = self
                .alignment_fault
                .or(self.timeline_fault)
                .map_or(sums[3].divide(tx[2], 100.0), Measure::unknown);
            for (label, value, min, max, essential) in [
                ("frames_encoded_delta", tx[2], 1.0, None, false),
                ("frames_sent_delta", tx[3], 1.0, None, false),
                ("frames_decoded_delta", sums[3], 1.0, None, false),
                (
                    "frames_encoded_per_second",
                    tx[2].divide(tx_elapsed, 1.0),
                    8.0,
                    Some(12.0),
                    true,
                ),
                (
                    "frames_decoded_per_second",
                    sums[3].divide(elapsed, 1.0),
                    8.0,
                    Some(12.0),
                    true,
                ),
                (
                    "decoded_to_encoded_percent",
                    aligned_ratio,
                    80.0,
                    Some(120.0),
                    true,
                ),
                ("frames_dropped_delta", sums[4], 0.0, None, false),
                ("freeze_count_delta", sums[5], 0.0, Some(1.0), false),
                (
                    "freeze_duration_seconds_delta",
                    sums[6],
                    0.0,
                    Some(0.250),
                    false,
                ),
                (
                    "mean_encode_ms",
                    tx[5].divide(tx[2], 1000.0),
                    0.0,
                    Some(50.0),
                    false,
                ),
            ] {
                metrics.insert(label.into(), assessed(value, min, max, essential));
            }
        }
        let essential = metrics
            .values()
            .filter(|value| value["essential_for_scenario"] == true)
            .collect::<Vec<_>>();
        let enough = self.intervals >= 2
            && self.duration >= MIN_WINDOW_SECONDS
            && self.sender_duration >= MIN_WINDOW_SECONDS;
        let assessment = if self.fault.is_some() || !enough {
            "unknown"
        } else if metrics.values().any(|v| v["scenario_assessment"] == "fail") {
            "fail"
        } else if self.alignment_fault.is_some()
            || self.timeline_fault.is_some()
            || essential
                .iter()
                .any(|v| v["scenario_assessment"] == "unknown")
        {
            "unknown"
        } else {
            "pass"
        };
        let reason = self
            .fault
            .or_else(|| (!enough).then_some("insufficient_sustained_window"))
            .or(self.alignment_fault)
            .or(self.timeline_fault)
            .or_else(|| (assessment == "unknown").then_some("essential_counter_unknown"));
        json!({"source":source.label(),"codec":self.fault.is_none().then_some(self.codec).flatten(),
            "cumulative_interval_count":self.intervals,"observed_interval_seconds":self.duration,
            "sender_observed_interval_seconds":self.sender_duration,
            "native_report_timeline":{"timestamp_unit_verified":self.timestamp_units_per_second.is_some(),
                "maximum_observed_report_age_bound_ms":self.max_report_age_bound.map(|age|age*1000.0),
                "unknown_reason":self.timeline_fault,"receiver_interval_seconds":(self.native_intervals>0).then_some(self.native_duration),
                "sender_interval_seconds":(self.native_intervals>0).then_some(self.native_sender_duration),
                "max_pair_timestamp_skew_ms":(self.native_pair_samples>0).then_some(1000.0*self.max_native_pair_skew),
                "max_report_vs_query_interval_difference_ms":(self.native_intervals>0).then_some(1000.0*self.max_native_query_difference)},
            "assessment":assessment,"assessment_scope":"local_generated_scenario_only_not_product_acceptance","unknown_reason":reason,"metrics":metrics,
            "sampling_alignment":{"max_pair_midpoint_skew_ms":1000.0*self.max_pair_skew,
                "max_query_span_ms":1000.0*self.max_query_span,"max_interval_duration_difference_ms":1000.0*self.max_interval_difference,
                "unknown_reason":self.alignment_fault,"clock":"host monotonic query midpoint; bounded callback uncertainty, not packet timestamps"},
            "receiver_auxiliary_rtp_records_in_last_sample":self.auxiliary.filter(|_|self.fault.is_none()).map(|a|json!({"rtx":a[0],"red":a[1],"fec":a[2]})),
            "sender_auxiliary_rtp_records_in_last_sample":self.sender_auxiliary.filter(|_|self.fault.is_none()).map(|a|json!({"rtx":a[0],"red":a[1],"fec":a[2]}))})
    }
}

pub(super) struct QualityProbe {
    origin: Instant,
    next: f64,
    phase: usize,
    recovery_requested: bool,
    before: [Window; 6],
    after: [Window; 6],
    pending: Option<tokio::task::JoinHandle<Vec<Sample>>>,
}
impl QualityProbe {
    pub(super) fn new(origin: Instant) -> Self {
        Self {
            origin,
            next: 6.0,
            phase: 0,
            recovery_requested: false,
            before: std::array::from_fn(|_| Window::default()),
            after: std::array::from_fn(|_| Window::default()),
            pending: None,
        }
    }
    pub(super) fn recovery_started(&mut self) {
        self.recovery_requested = true;
        self.phase = 1;
        if let Some(pending) = self.pending.take() {
            pending.abort();
        }
    }
    pub(super) fn tick(&mut self, a: &Peer, b: &Peer) {
        if self.phase == 1 && a.reconnecting && a.reconnected && b.reconnecting && b.reconnected {
            self.phase = 2;
            self.next = self.origin.elapsed().as_secs_f64();
        }
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.is_finished())
        {
            let Some(Ok(samples)) = self.pending.take().unwrap().now_or_never() else {
                let windows = if self.phase == 0 {
                    &mut self.before
                } else {
                    &mut self.after
                };
                for window in windows {
                    window.observe(Err("statistics_worker_unavailable"));
                }
                return;
            };
            for sample in samples {
                let source = [Source::Audio, Source::Camera, Source::Screen][sample.slot % 3];
                let (owner, receiver) = if sample.slot < 3 { (a, b) } else { (b, a) };
                let result = sample.result.and_then(|snapshot| {
                    if still_matches(owner, receiver, source, sample.slot, &snapshot.binding) {
                        Ok(snapshot)
                    } else {
                        Err("subscription_changed_during_sample")
                    }
                });
                let window = if self.phase == 0 {
                    &mut self.before[sample.slot]
                } else {
                    &mut self.after[sample.slot]
                };
                window.observe(result);
            }
        }
        let now = self.origin.elapsed().as_secs_f64();
        if self.phase == 1 || self.pending.is_some() || now < self.next {
            return;
        }
        self.next = now + SAMPLE_SECONDS;
        let requests = [(a, b, 0), (b, a, 3)]
            .into_iter()
            .flat_map(|(owner, receiver, offset)| {
                [Source::Audio, Source::Camera, Source::Screen]
                    .into_iter()
                    .map(move |source| request(owner, receiver, source, offset + source.index()))
            })
            .collect::<Vec<_>>();
        let origin = self.origin;
        self.pending = Some(tokio::spawn(async move {
            futures_util::future::join_all(requests.into_iter().map(|r| sample(r, origin))).await
        }));
    }
    pub(super) fn summary(&self) -> Value {
        let sources = [Source::Audio, Source::Camera, Source::Screen];
        let directions=[("a_to_b",0),("b_to_a",3)].map(|(direction,offset)|json!({"direction":direction,
            "before_recovery":sources.map(|source|self.before[offset+source.index()].report(source)),
            "after_recovery":sources.map(|source|self.after[offset+source.index()].report(source))}));
        let windows = directions
            .iter()
            .flat_map(|direction| {
                ["before_recovery", "after_recovery"]
                    .into_iter()
                    .flat_map(move |phase| direction[phase].as_array().into_iter().flatten())
            })
            .collect::<Vec<_>>();
        let scenario = if windows.iter().any(|w| w["assessment"] == "fail") {
            "fail"
        } else if self.recovery_requested
            && self.phase == 2
            && windows.len() == 12
            && windows.iter().all(|w| w["assessment"] == "pass")
        {
            "pass"
        } else {
            "unknown"
        };
        json!({"schema":1,"scope":"Owned generated per-source paired sender/receiver RTP observations; one Windows PC",
            "scenario":{"name":"owned_generated_native_v1","budget_version":"2026-10-22-local",
                "video_codec":"vp8","generated_video_fps":10,"camera_size":[640,360],"screen_size":[1280,720],
                "audio_hz":48000,"audio_channels":2,"sample_period_seconds":SAMPLE_SECONDS,"minimum_window_seconds":MIN_WINDOW_SECONDS,"minimum_intervals":2,
                "maximum_pair_midpoint_skew_ms":250,"maximum_query_span_ms":250,
                "maximum_interval_duration_difference_ms":500,
                "native_timestamp_units":"microseconds; pinned RTCStats UNIX epoch contract",
                "maximum_native_report_age_ms":250,"maximum_wall_vs_monotonic_difference_ms":50,
                "local_video_fps_range":[8,12],"local_decoded_to_encoded_percent_range":[80,120],
                "local_maximum_reported_loss_percent":2,"local_maximum_jitter_buffer_mean_ms":80,"local_maximum_audio_concealment_percent":2},
            "recovery_requested":self.recovery_requested,"recovery_observed":self.phase==2,"directions":directions,
            "unfinished_sample_at_capture":self.pending.is_some(),
            "scenario_assessment":scenario,"product_acceptance":"not_assessed",
            "padding_bytes":"unknown_not_exposed_separately_by_native_receiver_stats",
            "limitations":"Pass/fail applies only to the explicit local generated scenario budgets, across sustained windows both before and after recovery. Native timestamp age/progress/interval checks and bounded query skew must pass for timed assessments; unknown clock base, stale reports and wall-clock jumps retain unknown. Primary codec and auxiliary RTP records are separated locally; reported packet/byte counters may include repair/padding semantics. Audio encoder frame counts are unavailable. Buffer residence is not E2E latency; concealment is not listening quality. No cross-SFU SSRC matching, TURN-byte ratio, physical/WAN/audio-listening acceptance, Discord comparison or universal loss/frame-rate guarantee."})
    }
    pub(super) fn retain_owned_diagnostic(&self, primary_probe_passed: bool) {
        let Some(fixture) = std::env::var_os("RELAY_LIVE_FRIENDS_MEDIA") else {
            return;
        };
        let fixture = std::path::PathBuf::from(fixture);
        let Some(root) = fixture.parent() else {
            return;
        };
        let Ok(encoded) = serde_json::to_vec(
            &json!({"primary_probe_passed":primary_probe_passed,"per_source_quality_observations":self.summary()}),
        ) else {
            return;
        };
        // Same already-validated owned root as the native partial diagnostic.
        // The fixed six tracks/two phases are bounded; no identifiers serialize.
        if encoded.len() > 65_536 {
            return;
        }
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(root.join("native-track-quality.private.json"))
        {
            use std::io::Write;
            let _ = file.write_all(&encoded);
        }
    }
}
impl Drop for QualityProbe {
    fn drop(&mut self) {
        if let Some(pending) = self.pending.take() {
            pending.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use livekit::webrtc::stats_presence::{
        InboundObservationError, InboundRtpReport, OutboundRtpReport,
    };

    fn window() -> Window {
        Window::default()
    }
    fn observation(kind: InboundKind, fields: Value, id: &str) -> InboundRtpObservation {
        let (kind, mime) = if kind == InboundKind::Audio {
            ("audio", "audio/opus")
        } else {
            ("video", "video/VP8")
        };
        let mut row = json!({"type":"inbound-rtp","id":id,"trackIdentifier":"private-native-track","kind":kind,"codecId":"private-codec","timestamp":1_000_000_000.0});
        for (key, value) in fields.as_object().unwrap() {
            row[key] = value.clone();
        }
        let raw = json!([{"type":"codec","id":"private-codec","mimeType":mime},row]).to_string();
        InboundRtpReport::from_native_json(&raw)
            .unwrap()
            .select_track(
                "private-native-track",
                if kind == "audio" {
                    InboundKind::Audio
                } else {
                    InboundKind::Video
                },
            )
            .unwrap()
            .clone()
    }
    fn snapshot(source: Source, at: f64, step: u64) -> Snapshot {
        let fields = json!({"timestamp":1_000_000_000.0+at*1_000_000.0,"packetsReceived":step*100,"packetsLost":0,"bytesReceived":step*1000,
            "framesDecoded":step*120,"framesDropped":0,"freezeCount":0,"totalFreezeDuration":0.0,
            "jitterBufferDelay":1.0+step as f64*1.5,"jitterBufferEmittedCount":step*100,
            "totalSamplesReceived":100+step*100,"concealedSamples":step,"retransmittedPacketsReceived":0});
        let mut tx_fields = json!({"timestamp":1_000_000_000.0+at*1_000_000.0,"packetsSent":step*100,"bytesSent":step*1000,"retransmittedPacketsSent":0,"totalEncodeTime":step as f64*0.12});
        if !matches!(source, Source::Audio) {
            tx_fields["framesEncoded"] = json!(step * 120);
            tx_fields["framesSent"] = json!(step * 120);
        }
        Snapshot {
            binding: Binding {
                publication: "private-publication".into(),
                participant: "private-participant".into(),
                native_track: "private-native-track".into(),
                sender_track: "private-sender-track".into(),
            },
            at,
            sender_at: at,
            query_span: 0.001,
            receiver_wall_midpoint: Some(1000.0 + at),
            sender_wall_midpoint: Some(1000.0 + at),
            report_age_bound: Ok(0.001),
            observation: observation(source.kind(), fields, "private-stats-record"),
            sender: outbound(
                source.kind(),
                tx_fields,
                "private-sender-record",
                step as f64 * 12.0,
            ),
            auxiliary: Some([0; 3]),
            sender_auxiliary: Some([0; 3]),
        }
    }
    fn outbound(
        kind: InboundKind,
        fields: Value,
        id: &str,
        duration: f64,
    ) -> OutboundRtpObservation {
        let (kind, mime) = if kind == InboundKind::Audio {
            ("audio", "audio/opus")
        } else {
            ("video", "video/VP8")
        };
        let mut row = json!({"type":"outbound-rtp","id":id,"mediaSourceId":"private-source","kind":kind,"codecId":"private-codec","timestamp":1_000_000_000.0});
        for (key, value) in fields.as_object().unwrap() {
            row[key] = value.clone();
        }
        let raw=json!([{"type":"codec","id":"private-codec","mimeType":mime},
            {"type":"media-source","id":"private-source","trackIdentifier":"private-sender-track","totalSamplesDuration":duration},row]).to_string();
        OutboundRtpReport::from_native_json(&raw)
            .unwrap()
            .select_track(
                "private-sender-track",
                if kind == "audio" {
                    InboundKind::Audio
                } else {
                    InboundKind::Video
                },
            )
            .unwrap()
            .clone()
    }

    #[test]
    fn presence_missing_zero_and_typed_compatibility_are_distinct() {
        let missing = observation(InboundKind::Audio, json!({}), "owned-record");
        let zero = observation(
            InboundKind::Audio,
            json!({"packetsReceived":0,"jitterBufferDelay":0.0,"concealedSamples":0}),
            "owned-record",
        );
        assert_eq!(missing.counters.packets_received, None);
        assert_eq!(zero.counters.packets_received, Some(0));
        assert_eq!(missing.counters.jitter_buffer_delay, None);
        assert_eq!(zero.counters.jitter_buffer_delay, Some(0.0));
        assert_eq!(missing.counters.concealed_samples, None);
        assert_eq!(zero.counters.concealed_samples, Some(0));
        assert_eq!(
            observation(
                InboundKind::Audio,
                json!({"timestamp":null}),
                "owned-record"
            )
            .native_timestamp(),
            None
        );
        assert_eq!(
            observation(InboundKind::Audio, json!({"timestamp":0}), "owned-record")
                .native_timestamp(),
            Some(0.0)
        );
        assert_eq!(
            outbound(
                InboundKind::Audio,
                json!({"timestamp":null}),
                "owned-record",
                0.0
            )
            .native_timestamp(),
            None
        );
        assert_eq!(
            outbound(
                InboundKind::Audio,
                json!({"timestamp":0}),
                "owned-record",
                0.0
            )
            .native_timestamp(),
            Some(0.0)
        );
        let typed: RtcStats = serde_json::from_value(json!({"type":"inbound-rtp"})).unwrap();
        let RtcStats::InboundRtp(typed) = typed else {
            panic!("owned typed fixture");
        };
        assert_eq!(
            typed.received.packets_received, 0,
            "existing typed get_stats contract is unchanged"
        );
        assert!(count_delta(None, Some(0)).value.is_none());
        assert_eq!(count_delta(Some(0), Some(0)).value, Some(0.0));
        assert!(count_delta(Some(0), Some(9_007_199_254_740_993))
            .value
            .is_none());
    }

    #[test]
    fn exact_track_codec_and_source_matching_excludes_repair_and_ambiguity() {
        let rows = json!([
            {"type":"codec","id":"main","mimeType":"video/VP8"},
            {"type":"codec","id":"repair","mimeType":"video/rtx"},
            {"type":"codec","id":"redundant","mimeType":"video/red"},
            {"type":"inbound-rtp","id":"primary-record","trackIdentifier":"camera-track","kind":"video","codecId":"main","packetsReceived":100},
            {"type":"inbound-rtp","id":"repair-record","trackIdentifier":"camera-track","kind":"video","codecId":"repair","packetsReceived":999},
            {"type":"inbound-rtp","id":"red-record","trackIdentifier":"camera-track","kind":"video","codecId":"redundant","packetsReceived":999},
            {"type":"inbound-rtp","id":"other-record","trackIdentifier":"screen-track","kind":"video","codecId":"main","packetsReceived":555}
        ]);
        let report = InboundRtpReport::from_native_json(&rows.to_string()).unwrap();
        assert_eq!(
            report
                .select_track("camera-track", InboundKind::Video)
                .unwrap()
                .counters
                .packets_received,
            Some(100)
        );
        assert_eq!(report.auxiliary_records("camera-track"), Some([1, 1, 0]));
        assert_eq!(
            report
                .select_track("camera-track", InboundKind::Audio)
                .unwrap_err(),
            InboundObservationError::MissingPrimary
        );
        let mut ambiguous = rows.clone();
        ambiguous.as_array_mut().unwrap().push(rows[3].clone());
        assert_eq!(
            InboundRtpReport::from_native_json(&ambiguous.to_string())
                .unwrap()
                .select_track("camera-track", InboundKind::Video)
                .unwrap_err(),
            InboundObservationError::AmbiguousPrimary
        );
        let mut no_binding = rows.clone();
        no_binding[3]
            .as_object_mut()
            .unwrap()
            .remove("trackIdentifier");
        assert_eq!(
            InboundRtpReport::from_native_json(&no_binding.to_string())
                .unwrap()
                .select_track("camera-track", InboundKind::Video)
                .unwrap_err(),
            InboundObservationError::MissingTrackIdentity
        );
        let mut no_codec = rows.clone();
        no_codec[3]["codecId"] = json!("absent-codec");
        assert_eq!(
            InboundRtpReport::from_native_json(&no_codec.to_string())
                .unwrap()
                .select_track("camera-track", InboundKind::Video)
                .unwrap_err(),
            InboundObservationError::UnknownCodec
        );
        assert!(Source::Camera.matches(
            TrackSource::Camera,
            TrackSource::Camera,
            TrackSource::Camera
        ));
        assert!(!Source::Screen.matches(
            TrackSource::Screenshare,
            TrackSource::Camera,
            TrackSource::Camera
        ));
        assert!(!Source::Audio.matches(
            TrackSource::Microphone,
            TrackSource::ScreenshareAudio,
            TrackSource::ScreenshareAudio
        ));
        let mut unassigned_repair = rows.clone();
        unassigned_repair[4]
            .as_object_mut()
            .unwrap()
            .remove("trackIdentifier");
        unassigned_repair[4].as_object_mut().unwrap().remove("kind");
        let report = InboundRtpReport::from_native_json(&unassigned_repair.to_string()).unwrap();
        assert_eq!(
            report
                .select_track("camera-track", InboundKind::Video)
                .unwrap()
                .counters
                .packets_received,
            Some(100)
        );
        assert_eq!(report.auxiliary_records("camera-track"),None,"unassigned repair is unknown without discarding independently aligned primary counters");
    }

    #[test]
    fn sender_presence_requires_exact_media_source_and_preserves_missing_encoded_frames() {
        let missing = outbound(InboundKind::Video, json!({}), "owned-sender", 0.0);
        let zero = outbound(
            InboundKind::Video,
            json!({"framesEncoded":0,"framesSent":0,"packetsSent":0}),
            "owned-sender",
            0.0,
        );
        assert_eq!(missing.counters.frames_encoded, None);
        assert_eq!(zero.counters.frames_encoded, Some(0));
        let rows = json!([
            {"type":"codec","id":"primary","mimeType":"video/VP8"},
            {"type":"codec","id":"rtx","mimeType":"video/rtx"},
            {"type":"media-source","id":"source","trackIdentifier":"local-camera"},
            {"type":"outbound-rtp","id":"main","kind":"video","mediaSourceId":"source","codecId":"primary","framesEncoded":120},
            {"type":"outbound-rtp","id":"repair","kind":"video","codecId":"rtx","framesEncoded":999}
        ]);
        let report = OutboundRtpReport::from_native_json(&rows.to_string()).unwrap();
        assert_eq!(
            report
                .select_track("local-camera", InboundKind::Video)
                .unwrap()
                .counters
                .frames_encoded,
            Some(120)
        );
        assert_eq!(
            report
                .select_track("different-local-track", InboundKind::Video)
                .unwrap_err(),
            InboundObservationError::MissingPrimary
        );
        assert_eq!(
            report.auxiliary_records("local-camera"),
            None,
            "RTX without media-source identity is not counted as zero"
        );
        let mut absent = rows.clone();
        absent[3].as_object_mut().unwrap().remove("mediaSourceId");
        assert_eq!(
            OutboundRtpReport::from_native_json(&absent.to_string())
                .unwrap()
                .select_track("local-camera", InboundKind::Video)
                .unwrap_err(),
            InboundObservationError::MissingTrackIdentity
        );
        let mut duplicate = rows.clone();
        duplicate.as_array_mut().unwrap().push(rows[3].clone());
        assert_eq!(
            OutboundRtpReport::from_native_json(&duplicate.to_string())
                .unwrap()
                .select_track("local-camera", InboundKind::Video)
                .unwrap_err(),
            InboundObservationError::AmbiguousPrimary
        );
    }

    #[test]
    fn cumulative_deltas_use_corresponding_positive_jitter_and_sample_denominators() {
        let mut window = window();
        for step in 0..=2 {
            window.observe(Ok(snapshot(Source::Audio, step as f64 * 12.0, step)));
        }
        let report = window.report(Source::Audio);
        assert!(
            (report["metrics"]["jitter_buffer_mean_ms"]["value"]
                .as_f64()
                .unwrap()
                - 15.0)
                .abs()
                < 1e-9
        );
        assert_eq!(
            report["metrics"]["total_samples_received_delta"]["value"],
            200.0
        );
        assert_eq!(report["metrics"]["concealed_samples_percent"]["value"], 1.0);
        assert_eq!(report["assessment"], "pass");
        assert_eq!(
            report["metrics"]["audio_encoded_decoded_frame_comparison"]["scenario_assessment"],
            "unknown"
        );
        let zero = Measure::value(0.0);
        assert_eq!(
            zero.divide(zero, 1000.0).unknown,
            Some("nonpositive_denominator")
        );
        assert_eq!(
            Measure::unknown("missing_or_invalid_counter")
                .divide(Measure::value(10.0), 1000.0)
                .value,
            None
        );
    }

    #[test]
    fn native_report_age_progress_and_clock_alignment_gate_timed_assessments() {
        let empty = window().report(Source::Camera);
        assert!(empty["native_report_timeline"]["receiver_interval_seconds"].is_null());
        assert!(empty["native_report_timeline"]["max_pair_timestamp_skew_ms"].is_null());
        let bounds = QueryBounds {
            start: 10.0,
            end: 10.02,
            wall_start: Some(1000.0),
            wall_end: Some(1000.02),
        };
        assert!((report_age_bound(Some(1_000_010_000.0), &bounds).unwrap() - 0.01).abs() < 1e-9);
        assert_eq!(
            report_age_bound(None, &bounds),
            Err("missing_native_report_timestamp")
        );
        assert_eq!(
            report_age_bound(Some(999_000_000.0), &bounds),
            Err("native_report_age_or_clock_base_unknown")
        );
        assert_eq!(
            report_age_bound(Some(1_001_000_000.0), &bounds),
            Err("native_report_age_or_clock_base_unknown")
        );
        let jumped = QueryBounds {
            start: 10.0,
            end: 10.02,
            wall_start: Some(1000.0),
            wall_end: Some(1001.02),
        };
        assert_eq!(
            report_age_bound(Some(1_001_010_000.0), &jumped),
            Err("native_query_wall_clock_jump")
        );

        for defect in [
            "stale",
            "reset",
            "missing",
            "native_interval",
            "native_pair",
            "age",
            "wall_between_queries",
            "unverified_unit",
        ] {
            let mut window = window();
            if defect == "unverified_unit" {
                window.timestamp_units_per_second = None;
            }
            window.observe(Ok(snapshot(Source::Camera, 0.0, 0)));
            for step in 1..=2 {
                let at = step as f64 * 12.0;
                let mut current = snapshot(Source::Camera, at, step);
                let timestamp = match defect {
                    "stale" => json!(1_000_000_000.0),
                    "reset" => json!(999_000_000.0),
                    "missing" => Value::Null,
                    "native_interval" => json!(1_000_000_000.0 + at * 2_000_000.0),
                    _ => json!(1_000_000_000.0 + at * 1_000_000.0),
                };
                let mut observed = observation(
                    InboundKind::Video,
                    json!({"timestamp":timestamp}),
                    "private-stats-record",
                );
                observed.counters = current.observation.counters.clone();
                current.observation = observed;
                if defect == "native_pair" {
                    let mut sender = outbound(
                        InboundKind::Video,
                        json!({"timestamp":1_001_000_000.0+at*1_000_000.0}),
                        "private-sender-record",
                        at,
                    );
                    sender.counters = current.sender.counters.clone();
                    current.sender = sender;
                }
                if defect == "age" {
                    current.report_age_bound = Err("native_report_age_or_clock_base_unknown");
                }
                if defect == "wall_between_queries" {
                    current.receiver_wall_midpoint = Some(1001.0 + at);
                }
                window.observe(Ok(current));
            }
            let report = window.report(Source::Camera);
            assert_eq!(report["assessment"], "unknown", "{defect}");
            assert!(
                report["metrics"]["frames_encoded_per_second"]["value"].is_null(),
                "{defect}"
            );
            assert!(
                report["metrics"]["decoded_to_encoded_percent"]["value"].is_null(),
                "{defect}"
            );
            assert_eq!(
                report["metrics"]["jitter_buffer_mean_ms"]["scenario_assessment"], "pass",
                "independent delta survives {defect}"
            );
        }
    }

    #[test]
    fn missing_reset_corrected_loss_and_short_windows_never_become_pass_or_zero() {
        let mut missing = window();
        missing.observe(Ok(snapshot(Source::Camera, 0.0, 0)));
        let mut next = snapshot(Source::Camera, 12.0, 1);
        next.observation.counters.packets_received = None;
        missing.observe(Ok(next));
        missing.observe(Ok(snapshot(Source::Camera, 24.0, 2)));
        let report = missing.report(Source::Camera);
        assert!(report["metrics"]["reported_primary_packets_received_delta"]["value"].is_null());
        assert_eq!(report["assessment"], "unknown");
        let mut reset = window();
        reset.observe(Ok(snapshot(Source::Camera, 0.0, 0)));
        reset.observe(Ok(snapshot(Source::Camera, 12.0, 1)));
        let mut next = snapshot(Source::Camera, 24.0, 2);
        next.observation.counters.frames_decoded = Some(3);
        reset.observe(Ok(next));
        assert!(reset.report(Source::Camera)["metrics"]["frames_decoded_delta"]["value"].is_null());
        assert!(lost_delta(Some(10), Some(5)).value.is_none());
        let mut short = window();
        short.observe(Ok(snapshot(Source::Camera, 0.0, 0)));
        short.observe(Ok(snapshot(Source::Camera, 0.5, 1)));
        assert_eq!(
            short.report(Source::Camera)["assessment"],
            "unknown",
            "a few post-recovery frames are not quality acceptance"
        );
        assert_eq!(
            short.report(Source::Camera)["unknown_reason"],
            "insufficient_sustained_window"
        );
    }

    #[test]
    fn identity_changes_and_sampling_holes_are_not_spliced_into_complete_windows() {
        for change in [
            "publication",
            "native_track",
            "sender_track",
            "participant",
            "stats_record",
            "sender_record",
            "sample_hole",
        ] {
            let mut window = window();
            window.observe(Ok(snapshot(Source::Camera, 0.0, 0)));
            let mut next = snapshot(Source::Camera, 12.0, 1);
            match change {
                "publication" => next.binding.publication = "different-publication".into(),
                "native_track" => next.binding.native_track = "different-track".into(),
                "sender_track" => next.binding.sender_track = "different-sender-track".into(),
                "participant" => next.binding.participant = "different-participant".into(),
                "stats_record" => {
                    next.observation = observation(
                        InboundKind::Video,
                        json!({"packetsReceived":1000}),
                        "different-record",
                    )
                }
                "sender_record" => {
                    next.sender = outbound(
                        InboundKind::Video,
                        json!({"framesEncoded":120}),
                        "different-sender-record",
                        12.0,
                    )
                }
                _ => window.observe(Err("native_statistics_deadline")),
            }
            window.observe(Ok(next));
            window.observe(Ok(snapshot(Source::Camera, 24.0, 2)));
            let report = window.report(Source::Camera);
            assert_eq!(report["assessment"], "unknown", "{change}");
            assert!(report["metrics"]["frames_decoded_per_second"]["value"].is_null());
        }
    }

    #[test]
    fn paired_encoder_decoder_alignment_and_local_scenario_return_pass_fail_or_unknown() {
        let mut pass = window();
        for step in 0..=2 {
            pass.observe(Ok(snapshot(Source::Camera, step as f64 * 12.0, step)));
        }
        let report = pass.report(Source::Camera);
        assert_eq!(
            report["metrics"]["decoded_to_encoded_percent"]["value"],
            100.0
        );
        assert_eq!(
            report["metrics"]["frames_encoded_per_second"]["value"],
            10.0
        );
        assert_eq!(report["assessment"], "pass");
        let mut skew = window();
        for step in 0..=2 {
            let mut s = snapshot(Source::Camera, step as f64 * 12.0, step);
            s.sender_at += 0.6;
            skew.observe(Ok(s));
        }
        let report = skew.report(Source::Camera);
        assert_eq!(report["assessment"], "unknown");
        assert!(report["metrics"]["decoded_to_encoded_percent"]["value"].is_null());
        assert_eq!(
            report["metrics"]["jitter_buffer_mean_ms"]["scenario_assessment"], "pass",
            "independent jitter evidence is retained"
        );
        let mut missing = window();
        for step in 0..=2 {
            let mut s = snapshot(Source::Camera, step as f64 * 12.0, step);
            s.sender.counters.frames_encoded = None;
            missing.observe(Ok(s));
        }
        assert_eq!(missing.report(Source::Camera)["assessment"], "unknown");
        let mut fail = window();
        for step in 0..=2 {
            let mut s = snapshot(Source::Camera, step as f64 * 12.0, step);
            s.observation.counters.frames_decoded = Some(step * 60);
            fail.observe(Ok(s));
        }
        assert_eq!(fail.report(Source::Camera)["assessment"], "fail");
        let mut audio_fail = window();
        for step in 0..=2 {
            let mut s = snapshot(Source::Audio, step as f64 * 12.0, step);
            s.observation.counters.concealed_samples = Some(step * 5);
            audio_fail.observe(Ok(s));
        }
        assert_eq!(audio_fail.report(Source::Audio)["assessment"], "fail");
        let mut probe = QualityProbe::new(Instant::now());
        probe.phase = 2;
        probe.recovery_requested = true;
        for (slot, window) in probe
            .before
            .iter_mut()
            .enumerate()
            .chain(probe.after.iter_mut().enumerate())
        {
            let source = [Source::Audio, Source::Camera, Source::Screen][slot % 3];
            for step in 0..=2 {
                window.observe(Ok(snapshot(source, step as f64 * 12.0, step)));
            }
        }
        assert_eq!(probe.summary()["scenario_assessment"], "pass");
        assert_eq!(probe.summary()["product_acceptance"], "not_assessed");
        probe.after[1] = window();
        assert_eq!(
            probe.summary()["scenario_assessment"],
            "unknown",
            "before-only evidence or a few post-recovery frames cannot pass the scenario"
        );
    }

    #[test]
    fn observed_optional_budget_failures_fail_but_missing_optional_counters_do_not() {
        for defect in [
            "freeze_count",
            "freeze_duration",
            "encode_time",
            "optional_missing",
        ] {
            let mut window = window();
            for step in 0..=2 {
                let mut current = snapshot(Source::Camera, step as f64 * 12.0, step);
                match defect {
                    "freeze_count" => current.observation.counters.freeze_count = Some(step * 2),
                    "freeze_duration" => {
                        current.observation.counters.total_freeze_duration = Some(step as f64 * 0.5)
                    }
                    "encode_time" => {
                        current.sender.counters.total_encode_time = Some(step as f64 * 12.0)
                    }
                    _ => {
                        current.observation.counters.freeze_count = None;
                        current.observation.counters.total_freeze_duration = None;
                        current.sender.counters.total_encode_time = None;
                    }
                }
                window.observe(Ok(current));
            }
            assert_eq!(
                window.report(Source::Camera)["assessment"],
                if defect == "optional_missing" {
                    "pass"
                } else {
                    "fail"
                },
                "{defect}"
            );
        }
    }

    #[test]
    fn whitelist_and_fixed_quality_output_redact_identifiers_and_bound_reports() {
        let secret = "private-token=sdp-secret;192.168.22.33;turns://secret.invalid";
        let raw=json!([
            {"type":"codec","id":secret,"mimeType":"audio/opus","sdpFmtpLine":secret},
            {"type":"inbound-rtp","id":secret,"trackIdentifier":secret,"kind":"audio","codecId":secret,"packetsReceived":0,"address":secret,"token":secret},
            {"type":"local-candidate","address":secret,"url":secret,"usernameFragment":secret},
            {"type":"certificate","base64Certificate":secret}
        ]).to_string();
        let report = InboundRtpReport::from_native_json(&raw).unwrap();
        let selected = report.select_track(secret, InboundKind::Audio).unwrap();
        assert!(!format!("{report:?} {selected:?}").contains(secret));
        let tx_raw=json!([
            {"type":"codec","id":secret,"mimeType":"video/VP8","sdpFmtpLine":secret},
            {"type":"media-source","id":secret,"trackIdentifier":secret},
            {"type":"outbound-rtp","id":secret,"mediaSourceId":secret,"kind":"video","codecId":secret,"framesEncoded":0,"address":secret}
        ]).to_string();
        let tx = OutboundRtpReport::from_native_json(&tx_raw).unwrap();
        let selected = tx.select_track(secret, InboundKind::Video).unwrap();
        assert!(!format!("{tx:?} {selected:?}").contains(secret));
        let mut probe = QualityProbe::new(Instant::now());
        for window in probe.before.iter_mut().chain(probe.after.iter_mut()) {
            for step in 0..=2 {
                window.observe(Ok(snapshot(Source::Camera, step as f64 * 12.0, step)));
            }
        }
        let output = probe.summary().to_string();
        for forbidden in [
            secret,
            "private-native-track",
            "private-publication",
            "private-participant",
            "private-stats-record",
            "192.168.22.33",
        ] {
            assert!(!output.contains(forbidden));
        }
        assert!(output.len() < 65_536);
        assert_eq!(probe.summary()["product_acceptance"], "not_assessed");
        assert!(matches!(
            InboundRtpReport::from_native_json(&"x".repeat(262_145)),
            Err(InboundObservationError::ReportTooLarge)
        ));
        assert!(matches!(
            InboundRtpReport::from_native_json(&format!("[{}]", vec!["{}"; 257].join(","))),
            Err(InboundObservationError::ReportTooLarge)
        ));
        assert!(matches!(
            InboundRtpReport::from_native_json(secret),
            Err(InboundObservationError::MalformedReport)
        ));
    }
}
