//! A fixed voice recovery deadline, based only on current authenticated replies.
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
pub const RECOVERY_LIMIT: Duration = Duration::from_secs(30);

/// The sender probes every two seconds; a current authenticated reply may
/// arrive after a newer probe. Keep a small set within the existing timeout.
/// Endpoint, AEAD and transport replay validation remain the caller's job.
#[derive(Default)]
pub(crate) struct PendingProbes(VecDeque<(u64, Instant)>);
impl PendingProbes {
    fn prune(&mut self, now: Instant) {
        self.0.retain(|(_, sent)| {
            now.checked_duration_since(*sent)
                .is_some_and(|age| age < PROBE_TIMEOUT)
        });
    }
    pub(crate) fn record(&mut self, id: u64, now: Instant) {
        self.prune(now);
        self.0.retain(|(pending, _)| *pending != id);
        if self.0.len() == 4 {
            self.0.pop_front();
        }
        self.0.push_back((id, now));
    }
    pub(crate) fn take_reply(&mut self, id: u64, now: Instant) -> Option<Instant> {
        self.prune(now);
        let at = self.0.iter().position(|(pending, _)| *pending == id)?;
        self.0.remove(at).map(|(_, sent)| sent)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkState {
    Checking,
    Connected,
    Reconnecting { remaining: Duration },
    Expired,
}

pub struct VoiceRecovery {
    started: Instant,
    last_reply: Option<Instant>,
}
impl VoiceRecovery {
    pub fn new(started: Instant) -> Self {
        Self {
            started,
            last_reply: None,
        }
    }
    pub fn state(&self, now: Instant) -> LinkState {
        let age = now.saturating_duration_since(self.last_reply.unwrap_or(self.started));
        if age >= PROBE_TIMEOUT + RECOVERY_LIMIT {
            LinkState::Expired
        } else if age >= PROBE_TIMEOUT {
            LinkState::Reconnecting {
                remaining: PROBE_TIMEOUT + RECOVERY_LIMIT - age,
            }
        } else if self.last_reply.is_some() {
            LinkState::Connected
        } else {
            LinkState::Checking
        }
    }
    pub fn confirm_with_live_fallback(&mut self, now: Instant, fallback_live: bool) -> bool {
        if fallback_live {
            self.last_reply = Some(now);
            true
        } else {
            self.confirm(now)
        }
    }
    /// The caller must verify endpoint, AEAD, replay and the pending probe ID.
    /// A late reply cannot revive a transport that has reached its deadline.
    pub fn confirm(&mut self, now: Instant) -> bool {
        if self.state(now) == LinkState::Expired {
            return false;
        }
        self.last_reply = Some(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fresh_older_probes_can_reply_after_newer_sends_once_in_either_order() {
        let start = Instant::now();
        for newest_first in [false, true] {
            let mut probes = PendingProbes::default();
            probes.record(1, start);
            probes.record(2, start + Duration::from_secs(2));
            let now = start + Duration::from_millis(2600);
            for id in if newest_first { [2, 1] } else { [1, 2] } {
                assert_eq!(
                    probes.take_reply(id, now),
                    Some(if id == 1 {
                        start
                    } else {
                        start + Duration::from_secs(2)
                    })
                );
                assert_eq!(
                    probes.take_reply(id, now),
                    None,
                    "a proof must be consumed once"
                );
            }
            assert!(probes.0.is_empty());
        }
    }
    #[test]
    fn unknown_expired_or_future_proofs_do_not_extend_the_existing_recovery_deadline() {
        let start = Instant::now();
        let mut probes = PendingProbes::default();
        probes.record(1, start);
        assert!(probes
            .take_reply(999, start + Duration::from_secs(1))
            .is_none());
        assert!(probes.take_reply(1, start + PROBE_TIMEOUT).is_none());
        probes.record(2, start + Duration::from_secs(10));
        assert!(probes
            .take_reply(2, start + Duration::from_secs(9))
            .is_none());
        let mut link = VoiceRecovery::new(start);
        probes.record(3, start + Duration::from_secs(34));
        assert!(probes
            .take_reply(3, start + Duration::from_secs(35))
            .is_some());
        assert!(
            !link.confirm(start + Duration::from_secs(35)),
            "a fresh proof cannot revive expired UDP recovery"
        );
        assert_eq!(
            link.state(start + Duration::from_secs(35)),
            LinkState::Expired
        );
        let mut fresh = PendingProbes::default();
        fresh.record(4, start);
        assert_eq!(
            fresh.take_reply(4, start + PROBE_TIMEOUT - Duration::from_nanos(1)),
            Some(start)
        );
    }
    #[test]
    fn probe_storage_is_bounded_and_duplicate_records_do_not_duplicate_proofs() {
        let start = Instant::now();
        let mut probes = PendingProbes::default();
        for id in 0..1000 {
            probes.record(id, start);
            assert!(probes.0.len() <= 4);
        }
        assert!(probes.take_reply(995, start).is_none());
        probes.record(999, start);
        assert_eq!(probes.0.len(), 4);
        assert_eq!(probes.take_reply(999, start), Some(start));
        assert!(probes.take_reply(999, start).is_none());
        probes.record(1000, start + PROBE_TIMEOUT);
        assert_eq!(
            probes.0.len(),
            1,
            "record must prune stale proofs as well as read"
        );
    }
    #[test]
    fn unconfirmed_transport_has_five_seconds_then_one_fixed_recovery_window() {
        let start = Instant::now();
        let link = VoiceRecovery::new(start);
        assert_eq!(
            link.state(start + Duration::from_millis(4999)),
            LinkState::Checking
        );
        assert_eq!(
            link.state(start + PROBE_TIMEOUT),
            LinkState::Reconnecting {
                remaining: RECOVERY_LIMIT
            }
        );
        assert_eq!(
            link.state(start + Duration::from_secs(15)),
            LinkState::Reconnecting {
                remaining: Duration::from_secs(20)
            }
        );
        assert_eq!(
            link.state(start + Duration::from_secs(35)),
            LinkState::Expired
        );
        assert_eq!(
            link.state(start + Duration::from_secs(60)),
            LinkState::Expired
        );
    }
    #[test]
    fn a_current_reply_recovers_without_restarting_the_transport() {
        let start = Instant::now();
        let mut link = VoiceRecovery::new(start);
        assert!(link.confirm(start + Duration::from_secs(2)));
        assert_eq!(
            link.state(start + Duration::from_secs(3)),
            LinkState::Connected
        );
        assert_eq!(
            link.state(start + Duration::from_secs(7)),
            LinkState::Reconnecting {
                remaining: RECOVERY_LIMIT
            }
        );
        assert!(link.confirm(start + Duration::from_secs(20)));
        assert_eq!(
            link.state(start + Duration::from_secs(21)),
            LinkState::Connected
        );
        assert_eq!(
            link.state(start + Duration::from_secs(55)),
            LinkState::Expired
        );
    }
    #[test]
    fn worker_stall_and_late_reply_cannot_extend_a_failed_episode() {
        let start = Instant::now();
        let mut link = VoiceRecovery::new(start);
        assert!(link.confirm(start));
        assert_eq!(
            link.state(start + Duration::from_secs(34)),
            LinkState::Reconnecting {
                remaining: Duration::from_secs(1)
            }
        );
        assert!(!link.confirm(start + Duration::from_secs(35)));
        assert_eq!(
            link.state(start + Duration::from_secs(35)),
            LinkState::Expired
        );
        assert_eq!(
            link.state(start - Duration::from_millis(1)),
            LinkState::Connected
        );
    }
}
