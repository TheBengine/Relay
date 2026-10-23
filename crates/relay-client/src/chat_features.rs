//! Small, session-only chat state. Server enforcement remains authoritative.
use crate::network::Target;
use relay_protocol::{TypingState, User};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

const MAX_COOLDOWN_MS: u64 = 21_600_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Countdown {
    pub seconds: u64,
    pub slowmode: bool,
}

#[derive(Clone, Copy)]
struct Cooldown {
    until: Instant,
    slowmode: bool,
}

#[derive(Default)]
pub struct Cooldowns {
    account: Option<Cooldown>,
    conversations: HashMap<Target, Cooldown>,
}

impl Cooldowns {
    pub fn retain_slowmode(&mut self, enabled: impl Fn(&Target) -> bool) {
        self.conversations.retain(|target, _| enabled(target));
    }
    pub fn record(&mut self, target: Target, code: &str, retry_after_ms: u64, now: Instant) {
        let cooldown = Cooldown {
            until: now + Duration::from_millis(retry_after_ms.clamp(1, MAX_COOLDOWN_MS)),
            slowmode: code == "slowmode",
        };
        self.conversations.retain(|_, saved| saved.until > now);
        if cooldown.slowmode {
            if self.conversations.len() >= 64 && !self.conversations.contains_key(&target) {
                if let Some(oldest) = self
                    .conversations
                    .iter()
                    .min_by_key(|(_, saved)| saved.until)
                    .map(|(target, _)| target.clone())
                {
                    self.conversations.remove(&oldest);
                }
            }
            let saved = self.conversations.entry(target).or_insert(cooldown);
            if saved.until < cooldown.until {
                *saved = cooldown;
            }
        } else if self
            .account
            .is_none_or(|saved| saved.until < cooldown.until)
        {
            self.account = Some(cooldown);
        }
    }

    pub fn remaining(&self, target: &Target, now: Instant) -> Option<Countdown> {
        self.account
            .iter()
            .chain(self.conversations.get(target))
            .filter(|saved| saved.until > now)
            .max_by_key(|saved| saved.until)
            .map(|saved| Countdown {
                seconds: saved
                    .until
                    .saturating_duration_since(now)
                    .as_millis()
                    .div_ceil(1000) as u64,
                slowmode: saved.slowmode,
            })
    }
}

#[derive(Default)]
pub struct TypingUsers(Vec<(User, Instant)>);

impl TypingUsers {
    pub fn replace(&mut self, state: TypingState, own_id: &str, now: Instant) {
        self.0.clear();
        for entry in state.users.into_iter().take(32) {
            let remaining = if state.server_time_ms == 0 {
                8_000
            } else {
                entry
                    .expires_at_ms
                    .saturating_sub(state.server_time_ms)
                    .min(8_000)
            };
            if remaining > 0
                && entry.user.id != own_id
                && !self.0.iter().any(|(user, _)| user.id == entry.user.id)
            {
                self.0
                    .push((entry.user, now + Duration::from_millis(remaining)));
            }
        }
    }

    pub fn text(&self, now: Instant) -> Option<String> {
        let names: Vec<_> = self
            .0
            .iter()
            .filter(|(_, until)| *until > now)
            .map(|(user, _)| user.username.as_str())
            .collect();
        match names.as_slice() {
            [] => None,
            [name] => Some(format!("{name} is typing…")),
            [first, second] => Some(format!("{first} and {second} are typing…")),
            [first, second, ..] => Some(format!(
                "{first}, {second} and {} others are typing…",
                names.len() - 2
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slowmode_is_conversation_scoped_but_spam_budget_follows_the_account() {
        let now = Instant::now();
        let general = Target::Channel("general".into());
        let dm = Target::Direct("mira".into());
        let mut limits = Cooldowns::default();
        limits.record(general.clone(), "slowmode", 10_100, now);
        assert_eq!(
            limits.remaining(&general, now),
            Some(Countdown {
                seconds: 11,
                slowmode: true
            })
        );
        assert_eq!(limits.remaining(&dm, now), None);
        limits.record(dm.clone(), "message_rate_limit", 20_000, now);
        assert_eq!(limits.remaining(&general, now).unwrap().seconds, 20);
        assert!(!limits.remaining(&dm, now).unwrap().slowmode);
        assert_eq!(
            limits.remaining(&general, now + Duration::from_secs(20)),
            None
        );
        assert_eq!(limits.remaining(&dm, now + Duration::from_secs(20)), None);
    }

    #[test]
    fn malformed_delays_are_bounded_and_late_shorter_updates_do_not_unlock_send() {
        let now = Instant::now();
        let target = Target::Channel("general".into());
        let mut limits = Cooldowns::default();
        limits.record(target.clone(), "rate_limited", u64::MAX, now);
        limits.record(target.clone(), "rate_limited", 0, now);
        assert_eq!(limits.remaining(&target, now).unwrap().seconds, 21_600);
        let fresh = Cooldowns::default();
        assert_eq!(
            fresh.remaining(&target, now),
            None,
            "New community/account state must not inherit cooldowns"
        );
    }

    #[test]
    fn disabling_slowmode_or_promoting_a_member_preserves_account_spam_protection() {
        let now = Instant::now();
        let target = Target::Channel("general".into());
        let mut limits = Cooldowns::default();
        limits.record(target.clone(), "slowmode", 30_000, now);
        limits.retain_slowmode(|_| false);
        assert!(limits.remaining(&target, now).is_none());
        limits.record(target.clone(), "slowmode", 30_000, now);
        limits.record(target.clone(), "message_rate_limit", 5_000, now);
        limits.retain_slowmode(|_| false);
        assert_eq!(
            limits.remaining(&target, now),
            Some(Countdown {
                seconds: 5,
                slowmode: false
            })
        );
    }

    #[test]
    fn typing_uses_server_relative_ttl_expires_and_omits_self_and_duplicates() {
        let now = Instant::now();
        let mut typing = TypingUsers::default();
        let state: TypingState = serde_json::from_value(serde_json::json!({
            "server_time_ms": 1_000,
            "users": [
                {"user":{"id":"self","username":"Alex","role":"member"},"expires_at_ms":9_000},
                {"user":{"id":"mira","username":"Mira","role":"member"},"expires_at_ms":4_000},
                {"user":{"id":"mira","username":"Mira","role":"member"},"expires_at_ms":8_000},
                {"user":{"id":"expired","username":"Gone","role":"member"},"expires_at_ms":999}
            ]
        }))
        .unwrap();
        typing.replace(state, "self", now);
        assert_eq!(typing.text(now).as_deref(), Some("Mira is typing…"));
        assert_eq!(typing.text(now + Duration::from_secs(3)), None);
    }
}
