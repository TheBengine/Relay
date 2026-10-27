//! Voice first under load. When incoming voice shows lateness, loss or rising
//! jitter, video steps down, uploads slow down and the replica copy waits,
//! until voice has been healthy for a while. Nothing is cancelled.
use crate::voice_quality::Quality;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Bad samples must persist this long before anything is throttled. The
/// quality figures already cover 5-10 s, so a single dropout stays below the
/// thresholds below rather than being filtered here.
const ENTER_AFTER: Duration = Duration::from_secs(3);
/// Throttling ends after voice has been healthy this long.
const EXIT_AFTER: Duration = Duration::from_secs(10);

static PRESSURE: AtomicBool = AtomicBool::new(false);

/// Read by the video, upload and replica workers.
pub(crate) fn pressured() -> bool {
    PRESSURE.load(Ordering::Acquire)
}

/// Only the voice worker publishes; it clears the flag when the call ends.
pub(crate) fn publish(active: bool) {
    PRESSURE.store(active, Ordering::Release);
}

pub(crate) fn struggling(quality: &Quality) -> bool {
    quality.late_percent >= 3.0 || quality.loss_percent >= 3.0 || quality.jitter_ms >= 30.0
}

/// Clears the flag when the voice worker ends, also by panic.
pub(crate) struct ClearOnExit;

impl Drop for ClearOnExit {
    fn drop(&mut self) {
        publish(false);
    }
}

#[derive(Default)]
pub(crate) struct Detector {
    bad_since: Option<Instant>,
    healthy_since: Option<Instant>,
    active: bool,
}

impl Detector {
    /// Feed the current incoming quality (None: not enough packets, which counts
    /// as healthy so silence never keeps throttling on). Returns the state.
    pub(crate) fn observe(&mut self, quality: Option<Quality>, now: Instant) -> bool {
        if quality.as_ref().is_some_and(struggling) {
            self.healthy_since = None;
            let since = *self.bad_since.get_or_insert(now);
            if now.saturating_duration_since(since) >= ENTER_AFTER {
                self.active = true;
            }
        } else {
            self.bad_since = None;
            let since = *self.healthy_since.get_or_insert(now);
            if now.saturating_duration_since(since) >= EXIT_AFTER {
                self.active = false;
            }
        }
        self.active
    }
}

/// Upload rate while voice struggles: 1 MiB/s, so the largest 25 MiB upload
/// still finishes well inside the 120 s request timeout.
pub(crate) const UPLOAD_BYTES_PER_SECOND: u64 = 1024 * 1024;

/// Token bucket over one-second windows, independent of the reader's buffer
/// size: how long to wait before reading more, if at all.
pub(crate) fn upload_wait(pressured: bool, sent_in_window: u64, window_elapsed: Duration) -> Option<Duration> {
    if !pressured || window_elapsed >= Duration::from_secs(1) || sent_in_window < UPLOAD_BYTES_PER_SECOND {
        return None;
    }
    Some(Duration::from_secs(1) - window_elapsed)
}

/// How long a scheduled replica pull may wait for voice before it runs anyway.
pub(crate) const REPLICA_MAX_DEFER: Duration = Duration::from_secs(60);

/// The first pull and "Check now" are never held back.
pub(crate) fn defer_replica(pressured: bool, waited: Duration, first: bool, requested: bool) -> bool {
    pressured && !first && !requested && waited < REPLICA_MAX_DEFER
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quality(loss: f32, late: f32, jitter: f32) -> Option<Quality> {
        Some(Quality { loss_percent: loss, late_percent: late, jitter_ms: jitter, expected: 100 })
    }

    #[test]
    fn short_blips_do_not_throttle_and_sustained_trouble_does() {
        let start = Instant::now();
        let mut detector = Detector::default();
        // One 200 ms dropout at 10 ms packets is 2 % over the 10 s window.
        assert!(!detector.observe(quality(2.0, 0.0, 0.0), start));
        assert!(!detector.observe(quality(2.0, 0.0, 0.0), start + Duration::from_secs(8)));
        assert!(!detector.observe(quality(5.0, 0.0, 0.0), start + Duration::from_secs(9)));
        assert!(!detector.observe(quality(0.0, 0.0, 0.0), start + Duration::from_millis(9500)));
        assert!(!detector.observe(quality(0.0, 4.0, 0.0), start + Duration::from_secs(10)));
        assert!(!detector.observe(quality(0.0, 4.0, 0.0), start + Duration::from_secs(12)));
        assert!(detector.observe(quality(0.0, 4.0, 0.0), start + Duration::from_millis(13100)));
    }

    #[test]
    fn release_waits_for_ten_healthy_seconds() {
        let start = Instant::now();
        let mut detector = Detector::default();
        detector.observe(quality(0.0, 0.0, 45.0), start);
        assert!(!detector.observe(quality(0.0, 0.0, 45.0), start + Duration::from_secs(2)));
        assert!(detector.observe(quality(0.0, 0.0, 45.0), start + Duration::from_secs(3)));
        assert!(detector.observe(quality(0.1, 0.0, 5.0), start + Duration::from_secs(3)));
        assert!(detector.observe(None, start + Duration::from_secs(12)), "still within ten seconds");
        // Trouble again restarts the healthy clock.
        assert!(detector.observe(quality(4.5, 0.0, 0.0), start + Duration::from_secs(12)));
        assert!(detector.observe(quality(0.0, 0.0, 0.0), start + Duration::from_secs(13)));
        assert!(detector.observe(quality(0.0, 0.0, 0.0), start + Duration::from_secs(20)));
        assert!(!detector.observe(quality(0.0, 0.0, 0.0), start + Duration::from_secs(23)));
    }

    #[test]
    fn throttles_are_bounded_and_released() {
        let window = Duration::from_millis(300);
        assert_eq!(upload_wait(false, 10 * UPLOAD_BYTES_PER_SECOND, window), None);
        assert_eq!(upload_wait(true, UPLOAD_BYTES_PER_SECOND - 1, window), None);
        assert_eq!(upload_wait(true, UPLOAD_BYTES_PER_SECOND, window), Some(Duration::from_millis(700)));
        assert_eq!(upload_wait(true, UPLOAD_BYTES_PER_SECOND, Duration::from_secs(1)), None);
        let largest = 25 * 1024 * 1024;
        assert!(largest / UPLOAD_BYTES_PER_SECOND < 100, "the largest upload still fits the 120 s timeout");
        assert!(defer_replica(true, Duration::from_secs(10), false, false));
        assert!(!defer_replica(true, REPLICA_MAX_DEFER, false, false), "a copy is never postponed long");
        assert!(!defer_replica(true, Duration::ZERO, true, false), "the first copy is not held");
        assert!(!defer_replica(true, Duration::ZERO, false, true), "Check now is not held");
        assert!(!defer_replica(false, Duration::ZERO, false, false));
    }
}
