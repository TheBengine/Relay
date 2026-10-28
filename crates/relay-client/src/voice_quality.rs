//! Incoming voice quality per speaker: loss from missing media sequence
//! numbers, RFC 3550 interarrival jitter and late packets, over a short
//! rolling window. Pure bookkeeping; the receive worker feeds it.
use std::time::{Duration, Instant};

/// Each window lasts this long; a summary covers the last one or two.
pub(crate) const WINDOW: Duration = Duration::from_secs(5);
/// Larger jumps are a new talkspurt layout, a whisper or a restart, not loss.
const MAX_GAP: u64 = 25;
/// Fewer expected packets than this are too few to report a percentage.
const MIN_PACKETS: u64 = 25;
/// A transit change this large is a mute or a lost restart packet, not jitter.
const MAX_JITTER_STEP_MS: f64 = 500.0;
const SAMPLES_PER_MS: f64 = relay_protocol::AUDIO_SAMPLE_RATE as f64 / 1000.0;

#[derive(Clone, Copy, Default, Debug, PartialEq)]
struct Window {
    expected: u64,
    received: u64,
    late: u64,
}

pub(crate) struct ArrivalStats {
    origin: Instant,
    started: Instant,
    last_packet: Instant,
    highest: Option<u64>,
    last_transit_ms: Option<f64>,
    jitter_ms: f64,
    current: Window,
    previous: Option<Window>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Quality {
    /// Share of expected packets that never arrived, 0–100.
    pub loss_percent: f32,
    /// Share of received packets that came too late to play, 0–100.
    pub late_percent: f32,
    pub jitter_ms: f32,
    pub expected: u64,
}

impl ArrivalStats {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            origin: now,
            started: now,
            last_packet: now,
            highest: None,
            last_transit_ms: None,
            jitter_ms: 0.0,
            current: Window::default(),
            previous: None,
        }
    }

    fn roll(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.started);
        if elapsed >= WINDOW {
            // A quiet stretch longer than a window leaves nothing current to keep.
            self.previous = (elapsed < WINDOW * 2).then_some(self.current);
            self.current = Window::default();
            self.started = now;
        }
    }

    /// One authenticated packet from this speaker, before playout decisions.
    pub(crate) fn observe(&mut self, sequence: u64, timestamp: u64, restart: bool, now: Instant) {
        self.roll(now);
        self.last_packet = now;
        let transit = now.saturating_duration_since(self.origin).as_secs_f64() * 1000.0
            - timestamp as f64 / SAMPLES_PER_MS;
        let gap = match self.highest {
            Some(highest) if !restart && sequence > highest => {
                let gap = sequence - highest;
                self.current.expected += if gap <= MAX_GAP { gap } else { 1 };
                self.current.received += 1;
                self.highest = Some(sequence);
                gap
            }
            Some(highest) if sequence <= highest => {
                // A reordered packet (a talkspurt's restart packet included)
                // fills a gap counted earlier; an old duplicate counts for nothing.
                if sequence + MAX_GAP > highest {
                    self.current.received += 1;
                }
                return;
            }
            _ => {
                self.current.expected += 1;
                self.current.received += 1;
                self.highest = Some(sequence);
                self.last_transit_ms = Some(transit);
                return;
            }
        };
        if let Some(previous) = self.last_transit_ms {
            let difference = (transit - previous).abs();
            // Only consecutive packets: across a loss or a mute the sender's
            // clock may have paused, which is not network jitter.
            if gap == 1 && difference.is_finite() && difference < MAX_JITTER_STEP_MS {
                self.jitter_ms += (difference - self.jitter_ms) / 16.0;
            }
        }
        self.last_transit_ms = Some(transit);
    }

    /// Nothing heard for two windows: the person left or stopped talking.
    pub(crate) fn expired(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.last_packet) >= WINDOW * 2
    }

    /// A packet that arrived after its playout time or was dropped from a full queue.
    pub(crate) fn late(&mut self, now: Instant) {
        self.roll(now);
        self.current.late += 1;
    }

    /// The last one to two windows, or nothing while too few packets arrived.
    pub(crate) fn quality(&mut self, now: Instant) -> Option<Quality> {
        self.roll(now);
        let previous = self.previous.unwrap_or_default();
        let expected = previous.expected + self.current.expected;
        let received = previous.received + self.current.received;
        let late = previous.late + self.current.late;
        if expected < MIN_PACKETS {
            return None;
        }
        let lost = expected.saturating_sub(received);
        Some(Quality {
            loss_percent: (lost as f64 * 100.0 / expected as f64) as f32,
            late_percent: if received == 0 {
                0.0
            } else {
                (late.min(received) as f64 * 100.0 / received as f64) as f32
            },
            jitter_ms: self.jitter_ms as f32,
            expected,
        })
    }
}

/// Measured loss above this share turns automatic loss recovery on.
pub(crate) const RECOVERY_LOSS_PERCENT: f32 = 2.0;
/// Automatic loss recovery turns off after this long without loss above it.
pub(crate) const RECOVERY_CLEAN_TIME: Duration = Duration::from_secs(60);
/// Packets a person must have sent in the window before their loss counts:
/// one second of 10 ms packets. One loss in the first 30 is not 3 % loss.
pub(crate) const LOSS_EVIDENCE_PACKETS: u64 = 100;

/// The worst loss among people with enough packets to measure it.
pub(crate) fn loss_evidence(qualities: impl IntoIterator<Item = Quality>) -> Option<Quality> {
    worst(qualities.into_iter().filter(|q| q.expected >= LOSS_EVIDENCE_PACKETS))
}

/// Automatic Opus loss recovery, decided from the incoming `Quality` window. Relay has no report of how others receive us, so the
/// loss we measure on their packets stands in for the loss on ours: most
/// loss on home Wi-Fi and mobile links hits both directions.
#[derive(Default)]
pub(crate) struct LossRecoveryPolicy {
    active: bool,
    clean_since: Option<Instant>,
}

impl LossRecoveryPolicy {
    /// Returns whether automatic recovery should be on now. `None` (too few
    /// packets heard) is no evidence: it neither starts nor ends the clean minute.
    pub(crate) fn observe(&mut self, quality: Option<Quality>, now: Instant) -> bool {
        let quality = quality.filter(|q| q.expected >= LOSS_EVIDENCE_PACKETS);
        if quality.is_some_and(|q| q.loss_percent > RECOVERY_LOSS_PERCENT) {
            self.active = true;
            self.clean_since = None;
        } else if self.active && quality.is_some() {
            let since = *self.clean_since.get_or_insert(now);
            if now.saturating_duration_since(since) >= RECOVERY_CLEAN_TIME {
                self.active = false;
                self.clean_since = None;
            }
        }
        self.active
    }
}

/// The worst figure across speakers, each taken separately: one bad sender is
/// what the listener hears.
pub(crate) fn worst(qualities: impl IntoIterator<Item = Quality>) -> Option<Quality> {
    qualities.into_iter().fold(None, |worst: Option<Quality>, quality| {
        Some(match worst {
            None => quality,
            Some(worst) => Quality {
                loss_percent: worst.loss_percent.max(quality.loss_percent),
                late_percent: worst.late_percent.max(quality.late_percent),
                jitter_ms: worst.jitter_ms.max(quality.jitter_ms),
                expected: worst.expected.max(quality.expected),
            },
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: Duration = Duration::from_millis(20);

    /// 20 ms packets at a steady pace; `skip` decides which never arrive.
    fn stream(stats: &mut ArrivalStats, start: Instant, count: u64, skip: impl Fn(u64) -> bool) -> Instant {
        let mut now = start;
        for sequence in 1..=count {
            now = start + FRAME * sequence as u32;
            if !skip(sequence) {
                stats.observe(sequence, sequence * 960, sequence == 1, now);
            }
        }
        now
    }

    #[test]
    fn steady_stream_has_no_loss_and_no_jitter() {
        let start = Instant::now();
        let mut stats = ArrivalStats::new(start);
        let now = stream(&mut stats, start, 100, |_| false);
        let quality = stats.quality(now).unwrap();
        assert_eq!(quality.loss_percent, 0.0);
        assert!(quality.jitter_ms < 0.01);
        assert_eq!(quality.late_percent, 0.0);
    }

    #[test]
    fn missing_sequence_numbers_are_loss_and_reordering_is_not() {
        let start = Instant::now();
        let mut stats = ArrivalStats::new(start);
        // Every tenth packet lost: 10 %.
        let now = stream(&mut stats, start, 101, |sequence| sequence % 10 == 0);
        let quality = stats.quality(now).unwrap();
        assert!((quality.loss_percent - 10.0).abs() < 0.5, "{quality:?}");
        // A packet that arrives out of order still arrives.
        let mut stats = ArrivalStats::new(start);
        stream(&mut stats, start, 50, |sequence| sequence == 30);
        stats.observe(30, 30 * 960, false, start + FRAME * 51);
        assert_eq!(stats.quality(start + FRAME * 51).unwrap().loss_percent, 0.0);
    }

    #[test]
    fn restarts_and_large_jumps_are_not_counted_as_loss() {
        let start = Instant::now();
        let mut stats = ArrivalStats::new(start);
        stream(&mut stats, start, 40, |_| false);
        // A new talkspurt after a whisper elsewhere: sequence jumps by 200.
        let resumed = start + Duration::from_secs(3);
        for offset in 0..20u64 {
            stats.observe(240 + offset, (240 + offset) * 960, offset == 0, resumed + FRAME * offset as u32);
        }
        assert_eq!(stats.quality(resumed + FRAME * 20).unwrap().loss_percent, 0.0);
    }

    #[test]
    fn uneven_arrival_raises_jitter_and_late_packets_are_reported() {
        let start = Instant::now();
        let mut stats = ArrivalStats::new(start);
        for sequence in 1..=100u64 {
            // Alternate 10 ms early and 10 ms late around the steady pace.
            let wobble = if sequence % 2 == 0 { 10 } else { 0 };
            let now = start + FRAME * sequence as u32 + Duration::from_millis(wobble);
            stats.observe(sequence, sequence * 960, sequence == 1, now);
            if sequence % 20 == 0 {
                stats.late(now);
            }
        }
        let quality = stats.quality(start + FRAME * 101).unwrap();
        assert!(quality.jitter_ms > 5.0 && quality.jitter_ms < 11.0, "{quality:?}");
        assert!((quality.late_percent - 5.0).abs() < 0.5, "{quality:?}");
    }

    #[test]
    fn too_few_or_old_packets_report_nothing() {
        let start = Instant::now();
        let mut stats = ArrivalStats::new(start);
        let now = stream(&mut stats, start, 10, |_| false);
        assert_eq!(stats.quality(now), None, "ten packets are not a percentage");
        let now = stream(&mut stats, start, 100, |_| false);
        assert!(stats.quality(now).is_some());
        assert_eq!(stats.quality(now + WINDOW * 3), None, "old windows expire");
    }

    #[test]
    fn a_reordered_restart_packet_is_not_phantom_loss() {
        let start = Instant::now();
        let mut stats = ArrivalStats::new(start);
        stream(&mut stats, start, 40, |_| false);
        // The next talkspurt starts at 41, but 42 arrives first.
        let resumed = start + Duration::from_secs(2);
        stats.observe(42, 42 * 960, false, resumed);
        stats.observe(41, 41 * 960, true, resumed + Duration::from_millis(1));
        for sequence in 43..=60u64 {
            stats.observe(sequence, sequence * 960, false, resumed + FRAME * (sequence - 41) as u32);
        }
        assert_eq!(stats.quality(resumed + FRAME * 20).unwrap().loss_percent, 0.0);
    }

    #[test]
    fn a_paused_sender_clock_is_not_jitter() {
        let start = Instant::now();
        let mut stats = ArrivalStats::new(start);
        let now = stream(&mut stats, start, 40, |_| false);
        // Muted for 10 s: the restart packet after unmute is lost, so the next
        // packet arrives without a restart flag and a 10 s transit jump.
        let resumed = now + Duration::from_secs(10);
        for sequence in 42..=80u64 {
            stats.observe(sequence, sequence * 960, false, resumed + FRAME * (sequence - 42) as u32);
        }
        let last = resumed + FRAME * 38;
        assert!(stats.quality(last).unwrap().jitter_ms < 1.0);
        assert!(!stats.expired(last + Duration::from_secs(9)));
        assert!(stats.expired(last + Duration::from_secs(10)));
    }

    fn with_loss(loss_percent: f32) -> Option<Quality> {
        Some(Quality { loss_percent, late_percent: 0.0, jitter_ms: 2.0, expected: 500 })
    }

    #[test]
    fn fec_turns_on_when_measured_loss_exceeds_two_percent_and_off_again_after_a_clean_minute() {
        let start = Instant::now();
        let mut policy = LossRecoveryPolicy::default();
        assert!(!policy.observe(None, start), "no evidence, no recovery");
        // One lost packet in the first 30 is not yet a loss rate.
        let early = Quality { loss_percent: 3.3, late_percent: 0.0, jitter_ms: 2.0, expected: 30 };
        assert!(!policy.observe(Some(early), start));
        assert_eq!(loss_evidence([early]), None);
        let a = Quality { expected: 400, ..early };
        let b = Quality { loss_percent: 9.0, expected: 40, ..early };
        assert_eq!(loss_evidence([a, b]), Some(a), "each person's own count decides");
        assert!(!policy.observe(with_loss(2.0), start), "exactly 2 % is not above it");
        assert!(policy.observe(with_loss(2.5), start + Duration::from_secs(1)));
        // Loss falls back: recovery stays on for a whole clean minute.
        let clean = start + Duration::from_secs(2);
        for second in 0..60 {
            assert!(policy.observe(with_loss(0.4), clean + Duration::from_secs(second)), "{second}");
        }
        // Another loss spike restarts the minute.
        assert!(policy.observe(with_loss(4.0), clean + Duration::from_secs(59)));
        assert!(policy.observe(with_loss(0.0), clean + Duration::from_secs(60)));
        // Silence is no evidence either way: it neither ends nor restarts the minute.
        assert!(policy.observe(None, clean + Duration::from_secs(100)));
        assert!(policy.observe(with_loss(0.0), clean + Duration::from_secs(119)));
        assert!(!policy.observe(with_loss(0.0), clean + Duration::from_secs(120)));
        assert!(!policy.observe(with_loss(1.9), clean + Duration::from_secs(121)));

        // Driven by real arrival counts over the 5 s window: 3 % loss turns it on.
        let mut stats = ArrivalStats::new(start);
        let mut policy = LossRecoveryPolicy::default();
        let now = stream(&mut stats, start, 200, |sequence| sequence % 33 == 0);
        assert!(policy.observe(stats.quality(now), now));
        let mut stats = ArrivalStats::new(start);
        let mut policy = LossRecoveryPolicy::default();
        let now = stream(&mut stats, start, 200, |sequence| sequence % 100 == 0);
        assert!(!policy.observe(stats.quality(now), now), "1 % stays off");
    }

    #[test]
    fn worst_speaker_decides_each_figure() {
        let a = Quality { loss_percent: 2.0, late_percent: 0.0, jitter_ms: 3.0, expected: 100 };
        let b = Quality { loss_percent: 0.5, late_percent: 4.0, jitter_ms: 9.0, expected: 60 };
        let worst = worst([a, b]).unwrap();
        assert_eq!((worst.loss_percent, worst.late_percent, worst.jitter_ms), (2.0, 4.0, 9.0));
        assert_eq!(super::worst([]), None);
    }
}
