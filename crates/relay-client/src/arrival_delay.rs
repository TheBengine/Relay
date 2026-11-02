//! Relative transit delay and a recent 95th percentile, inspired by NetEq's
//! PacketArrivalHistory and UnderrunOptimizer. Relay-specific Rust policy;
//! no transport counters, audio stretching or extra playout queue.
use relay_protocol::AUDIO_SAMPLE_RATE;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

const WINDOW: Duration = Duration::from_secs(2);
// Two seconds of the shortest supported 2.5 ms packets, never unbounded.
const MAX_SAMPLES: usize = 800;
const MAX_GAP: Duration = Duration::from_millis(200);

pub(crate) enum Observation {
    Reset,
    Ignore,
    DelayMs(u32),
}

struct Arrival {
    at: Instant,
    source: u64,
    transit_us: i128,
    relative_us: u32,
}

pub(crate) struct RelativeDelay {
    anchor: Option<(u64, Instant)>,
    newest: Option<(u64, Instant)>,
    last_arrival: Option<Instant>,
    history: VecDeque<Arrival>,
    sorted: Vec<u32>,
}

impl Default for RelativeDelay {
    fn default() -> Self {
        Self {
            anchor: None,
            newest: None,
            last_arrival: None,
            history: VecDeque::with_capacity(MAX_SAMPLES),
            sorted: Vec::with_capacity(MAX_SAMPLES),
        }
    }
}

impl RelativeDelay {
    fn reset(&mut self, source: u64, now: Instant) -> Observation {
        self.anchor = Some((source, now));
        self.newest = Some((source, now));
        self.last_arrival = Some(now);
        self.history.clear();
        self.history.push_back(Arrival {
            at: now,
            source,
            transit_us: 0,
            relative_us: 0,
        });
        Observation::Reset
    }

    pub(crate) fn observe(&mut self, source: u64, restart: bool, now: Instant) -> Observation {
        let Some((newest, received)) = self.newest else {
            return self.reset(source, now);
        };
        let gap = source.abs_diff(newest) as u128 * 1_000_000 / u128::from(AUDIO_SAMPLE_RATE);
        if restart
            || self.last_arrival.is_some_and(|last| now < last)
            || (source > newest
                && (gap > MAX_GAP.as_micros() || now.saturating_duration_since(received) > MAX_GAP))
        {
            return self.reset(source, now);
        }
        // An obsolete/replayed clock cannot replace the newest timing baseline.
        if (source < newest && gap > MAX_GAP.as_micros())
            || self.history.iter().any(|sample| sample.source == source)
        {
            return Observation::Ignore;
        }
        self.last_arrival = Some(now);
        if source > newest {
            self.newest = Some((source, now));
        }
        while self
            .history
            .front()
            .is_some_and(|sample| now.saturating_duration_since(sample.at) > WINDOW)
            || self.history.len() >= MAX_SAMPLES
        {
            self.history.pop_front();
        }
        let (anchor, start) = self.anchor.unwrap();
        let transit_us = now.saturating_duration_since(start).as_micros() as i128
            - (i128::from(source) - i128::from(anchor)) * 1_000_000 / i128::from(AUDIO_SAMPLE_RATE);
        let minimum = self
            .history
            .iter()
            .map(|sample| sample.transit_us)
            .min()
            .unwrap_or(transit_us)
            .min(transit_us);
        let relative_us = (transit_us - minimum).clamp(0, 200_000) as u32;
        // Keep each delay as measured at arrival. Recomputing old delays against
        // a newly faster route would invent congestion after an improvement.
        self.history.push_back(Arrival {
            at: now,
            source,
            transit_us,
            relative_us,
        });
        self.sorted.clear();
        self.sorted
            .extend(self.history.iter().map(|sample| sample.relative_us));
        self.sorted.sort_unstable();
        let index = (self.sorted.len() * 95).div_ceil(100).saturating_sub(1);
        Observation::DelayMs(self.sorted[index].div_ceil(1000))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn relative_delay_handles_reordering_without_poisoning_the_next_packet() {
        let start = Instant::now();
        let mut tracker = RelativeDelay::default();
        let mut measured = 0;
        let mut arrivals: Vec<_> = (0..400u64)
            .map(|n| (n * 10 + if n % 4 == 0 { 25 } else { 0 }, n))
            .collect();
        arrivals.sort_unstable();
        for (ms, n) in arrivals {
            if let Observation::DelayMs(delay) =
                tracker.observe(n * 480, false, start + Duration::from_millis(ms))
            {
                measured = delay;
            }
        }
        assert_eq!(measured, 25);
    }
    #[test]
    fn faster_route_and_clock_drift_do_not_invent_extra_buffering() {
        let start = Instant::now();
        let mut tracker = RelativeDelay::default();
        for n in 0..1200u64 {
            let path_us = if n < 600 { 40_000 } else { 0 };
            // 100 ppm drift. Skip the overlapping transition packets to isolate
            // the faster baseline; the separate reorder trace covers overlaps.
            let now = start + Duration::from_micros(n * 10_000 + path_us + n);
            if (600..604).contains(&n) {
                continue;
            }
            if let Observation::DelayMs(delay) = tracker.observe(n * 480, n == 0, now) {
                assert!(delay <= 1, "{n}: {delay}");
            }
        }
    }
    #[test]
    fn history_is_bounded_and_duplicates_and_old_packets_do_not_change_it() {
        let start = Instant::now();
        let mut tracker = RelativeDelay::default();
        for n in 0..4000u64 {
            tracker.observe(n * 120, false, start + Duration::from_micros(n * 2500));
        }
        assert!(tracker.history.len() <= MAX_SAMPLES);
        let len = tracker.history.len();
        assert!(matches!(
            tracker.observe(3999 * 120, false, start + Duration::from_secs(10)),
            Observation::Ignore
        ));
        assert!(matches!(
            tracker.observe(1, false, start + Duration::from_secs(10)),
            Observation::Ignore
        ));
        assert_eq!(tracker.history.len(), len);
        assert!(matches!(
            tracker.observe(0, true, start + Duration::from_secs(20)),
            Observation::Reset
        ));
        assert_eq!(tracker.history.len(), 1);
    }
}
