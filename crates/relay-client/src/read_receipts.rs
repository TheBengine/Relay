//! Keep queued read receipts distinct from failed writes without resending every frame.
use std::time::{Duration, Instant};

pub(super) struct Attempt {
    sequence: u64,
    retry_at: Option<Instant>,
    failures: u8,
}

impl Attempt {
    pub(super) fn queued(sequence: u64) -> Self {
        Self {
            sequence,
            retry_at: None,
            failures: 0,
        }
    }

    pub(super) fn ready(&self, sequence: u64, now: Instant) -> bool {
        sequence > self.sequence
            || (sequence == self.sequence && self.retry_at.is_some_and(|at| at <= now))
    }

    pub(super) fn retry_at(&self) -> Option<Instant> {
        self.retry_at
    }

    pub(super) fn sent(&mut self, sequence: u64) {
        if sequence != self.sequence {
            self.failures = 0;
        }
        self.sequence = sequence;
        self.retry_at = None;
    }

    /// Returns true only for the first failed attempt at this sequence.
    pub(super) fn completed(&mut self, sequence: u64, succeeded: bool, now: Instant) -> bool {
        if sequence != self.sequence {
            return false;
        }
        if succeeded {
            self.failures = 0;
            self.retry_at = None;
            return false;
        }
        let first_failure = self.failures == 0;
        self.failures = self.failures.saturating_add(1);
        let seconds = (1_u64 << self.failures.min(6)).min(60);
        self.retry_at = Some(now + Duration::from_secs(seconds));
        first_failure
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_receipt_retries_the_same_visible_message_only_after_backoff() {
        let now = Instant::now();
        let mut receipt = Attempt::queued(12);
        assert!(!receipt.ready(12, now));
        assert!(receipt.completed(12, false, now));
        assert!(!receipt.ready(12, now + Duration::from_secs(1)));
        assert!(receipt.ready(12, now + Duration::from_secs(2)));
        receipt.sent(12);
        assert!(!receipt.ready(12, now + Duration::from_secs(20)));
        assert!(!receipt.completed(12, false, now + Duration::from_secs(2)));
        assert!(!receipt.ready(12, now + Duration::from_secs(5)));
        assert!(receipt.ready(12, now + Duration::from_secs(6)));
        receipt.sent(12);
        receipt.completed(12, true, now + Duration::from_secs(6));
        assert!(!receipt.ready(12, now + Duration::from_secs(600)));
        assert!(receipt.ready(13, now));
    }

    #[test]
    fn delayed_results_cannot_reopen_or_suppress_a_newer_receipt() {
        let now = Instant::now();
        let mut receipt = Attempt::queued(12);
        receipt.sent(15);
        assert!(!receipt.completed(12, false, now));
        assert!(!receipt.ready(15, now + Duration::from_secs(60)));
        receipt.completed(15, false, now);
        receipt.completed(12, true, now);
        assert!(receipt.ready(15, now + Duration::from_secs(2)));
        assert!(!receipt.ready(14, now + Duration::from_secs(2)));
    }

    #[test]
    fn repeated_failures_never_create_a_busy_loop_or_unbounded_wait() {
        let mut now = Instant::now();
        let mut receipt = Attempt::queued(12);
        for _ in 0..300 {
            receipt.completed(12, false, now);
            let deadline = receipt.retry_at().unwrap();
            assert!(deadline.duration_since(now) >= Duration::from_secs(2));
            assert!(deadline.duration_since(now) <= Duration::from_secs(60));
            assert!(!receipt.ready(12, now));
            assert!(receipt.ready(12, deadline));
            now = deadline;
            receipt.sent(12);
        }
    }
}
