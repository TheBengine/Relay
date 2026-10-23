//! Receive timing policy only; never operates on transport or encryption counters.
use relay_protocol::AUDIO_SAMPLE_RATE;
use std::time::{Duration, Instant};

pub(crate) const MIN_DELAY_MS: u32 = 10;
pub(crate) const MAX_DELAY_MS: u32 = 80;
const RAISE_INTERVAL: Duration = Duration::from_secs(1);
const RECOVERY_INTERVAL: Duration = Duration::from_secs(5);

pub(crate) struct AdaptiveDelay {
    target_ms: u32,
    previous: Option<(u64, Instant)>,
    jitter_ms: f64,
    last_raise: Option<Instant>,
    good_since: Option<Instant>,
}

impl Default for AdaptiveDelay {
    fn default() -> Self {
        Self {
            target_ms: MIN_DELAY_MS,
            previous: None,
            jitter_ms: 0.0,
            last_raise: None,
            good_since: None,
        }
    }
}

impl AdaptiveDelay {
    pub(crate) fn target_ms(&self) -> u32 {
        self.target_ms
    }

    pub(crate) fn observe(&mut self, timestamp: u64, restart: bool, now: Instant) {
        let previous = self.previous.replace((timestamp, now));
        let Some((at, received)) = previous else {
            return;
        };
        let elapsed = now.saturating_duration_since(received);
        let source_ms =
            (i128::from(timestamp) - i128::from(at)) as f64 * 1000.0 / f64::from(AUDIO_SAMPLE_RATE);
        // A talkspurt, suspended worker, or changed source clock is a new timing
        // baseline, not proof of network congestion or ten seconds of recovery.
        if restart
            || elapsed > Duration::from_millis(200)
            || source_ms.abs() > 200.0
            || now < received
        {
            self.jitter_ms = 0.0;
            self.good_since = None;
            return;
        }
        // Arrival-order transit variation, using RFC 3550's 1/16 smoothing.
        // This is a local controller, not an RTCP jitter report.
        let variation = (elapsed.as_secs_f64() * 1000.0 - source_ms)
            .abs()
            .min(200.0);
        self.jitter_ms += (variation - self.jitter_ms) / 16.0;
        let extra = (2.0 * self.jitter_ms - 5.0).max(0.0).ceil() as u32;
        let suggested = (MIN_DELAY_MS + extra)
            .div_ceil(10)
            .saturating_mul(10)
            .clamp(MIN_DELAY_MS, MAX_DELAY_MS);
        if suggested > self.target_ms {
            self.good_since = None;
            self.raise(now);
        } else if suggested < self.target_ms {
            let since = *self.good_since.get_or_insert(now);
            if now.saturating_duration_since(since) >= RECOVERY_INTERVAL {
                self.target_ms = self.target_ms.saturating_sub(10).max(MIN_DELAY_MS);
                self.good_since = Some(now);
            }
        } else {
            self.good_since = None;
        }
    }

    pub(crate) fn late_packet(&mut self, now: Instant) {
        // An actual late arrival proves that the current deadline was too
        // short. A missing packet alone cannot prove that waiting would help.
        self.good_since = None;
        self.raise(now);
    }

    fn raise(&mut self, now: Instant) {
        if self
            .last_raise
            .is_none_or(|at| now.saturating_duration_since(at) >= RAISE_INTERVAL)
        {
            self.target_ms = (self.target_ms + 10).min(MAX_DELAY_MS);
            self.last_raise = Some(now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stable_minute_and_short_jitter_recover_within_twenty_seconds() {
        let now = Instant::now();
        let mut policy = AdaptiveDelay::default();
        for n in 0..6000u64 {
            policy.observe(n * 480, n == 0, now + Duration::from_millis(n * 10));
        }
        assert_eq!(policy.target_ms(), 10);
        policy.late_packet(now + Duration::from_secs(60));
        policy.late_packet(now + Duration::from_secs(61));
        assert_eq!(policy.target_ms(), 30);
        for n in 6000..8000u64 {
            policy.observe(
                n * 480,
                n == 6000,
                now + Duration::from_millis(n * 10 + 2000),
            );
        }
        assert_eq!(policy.target_ms(), 10);
    }

    #[test]
    fn stable_arrival_and_clock_gaps_do_not_invent_congestion() {
        let now = Instant::now();
        let mut policy = AdaptiveDelay::default();
        for n in 0..1000u64 {
            policy.observe(n * 960, n == 0, now + Duration::from_millis(n * 20));
        }
        assert_eq!(policy.target_ms(), 10);
        policy.observe(960_000 + 48_000, true, now + Duration::from_secs(21));
        policy.observe(u64::MAX, false, now + Duration::from_secs(22));
        assert_eq!(policy.target_ms(), 10);
    }

    #[test]
    fn uneven_arrival_and_reorder_raise_slowly_then_recover_with_live_samples() {
        let now = Instant::now();
        let mut policy = AdaptiveDelay::default();
        for n in 0..300u64 {
            // Ordered arrival, unequal transit delays; every pair remains 40 ms.
            let arrival = n * 20 + if n % 2 == 0 { 0 } else { 19 };
            policy.observe(n * 960, n == 0, now + Duration::from_millis(arrival));
        }
        assert!(policy.target_ms() >= 40 && policy.target_ms() <= 80);
        let raised = policy.target_ms();
        // No traffic is not recovery evidence.
        policy.observe(300 * 960, true, now + Duration::from_secs(60));
        assert_eq!(policy.target_ms(), raised);
        for n in 1..=2000u64 {
            policy.observe(
                (300 + n) * 960,
                false,
                now + Duration::from_secs(60) + Duration::from_millis(n * 20),
            );
        }
        assert_eq!(policy.target_ms(), 10);
        // Arrival order with a backwards source timestamp must remain bounded.
        policy.observe(3000 * 960, false, now + Duration::from_secs(101));
        policy.observe(
            2999 * 960,
            false,
            now + Duration::from_secs(101) + Duration::from_millis(1),
        );
        assert!((10..=80).contains(&policy.target_ms()));
    }

    #[test]
    fn repeated_late_packets_have_a_hard_ceiling_and_raise_cooldown() {
        let now = Instant::now();
        let mut policy = AdaptiveDelay::default();
        for n in 0..1000 {
            policy.late_packet(now + Duration::from_millis(n));
        }
        assert_eq!(policy.target_ms(), 20);
        for n in 1000..10_000 {
            policy.late_packet(now + Duration::from_millis(n));
        }
        assert_eq!(policy.target_ms(), 80);
    }
}
