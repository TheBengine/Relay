//! Friends-message metadata only. This observer never opens a crypto actor,
//! reads message bodies, changes unread state, or acknowledges a conversation.
use super::*;
use crate::notifications::{direct_policy_allows, group_policy_allows, GenerationGuard};
use relay_protocol::{ChangePage, MessageChange};
use std::collections::{HashMap, HashSet, VecDeque};

const MAX_IDS: usize = 512;
const MAX_PENDING: usize = 64;
const RETRY_LIFETIME: Duration = Duration::from_secs(15);
// Ownership is a lease, not a permanent claim. A failed/stalled subscriber
// leaves the already authenticated UI able to notify without replaying IDs.
const OWNERSHIP_LIFETIME: Duration = Duration::from_secs(10);

pub(super) struct Messages {
    baseline: Option<u64>,
    cursor: u64,
    cursor_received: Instant,
    claimed_through: u64,
    owned_until: Option<Instant>,
    peers: HashMap<String, String>,
    /// The groups of friends this account belongs to right now. Only these
    /// channels can alert, and only for a message by a current member.
    groups: HashMap<String, Group>,
    /// People this account blocked: never an author of an alert.
    blocked: HashSet<String>,
    seen: VecDeque<String>,
    pending: Vec<Pending>,
}
#[derive(Clone, PartialEq, Eq)]
struct Group {
    name: String,
    members: HashSet<String>,
    /// Names of the members, for the alert text. Group members need not be friends.
    names: HashMap<String, String>,
}
/// Why a change may alert: who wrote it, and the group when it is not a private message.
struct Candidate {
    peer: String,
    group: Option<String>,
    mentioned: bool,
}
struct Pending {
    id: String,
    peer: String,
    group: Option<String>,
    mentioned: bool,
    deadline: Instant,
    sent: bool,
    retry_at: Option<Instant>,
}
pub(super) struct Delivery {
    pub origin: CommandOrigin,
    pub id: String,
    pub notification: Notification,
}

pub(super) fn message_cue(policy: &Policy, delivered: bool, ringing: bool) -> Option<AudioCue> {
    (delivered
        && !ringing
        && !policy.disabled
        && policy.notifications
        && !policy.deafened
        && policy.audio.sounds_enabled
        && policy.audio.sound_volume > 0.0)
        .then_some(AudioCue::Message)
}

impl Messages {
    pub fn rebind(&mut self) {
        self.owned_until = None;
        // Rebinding the same authenticated account keeps bounded unsent
        // retries. The fresh bootstrap revalidates peers/DnD before delivery.
    }
    fn candidate(&self, origin: &CommandOrigin, change: &MessageChange) -> Option<Candidate> {
        let message = change.message.as_ref()?;
        if change.kind != "created"
            || change.deleted
            || message.id != change.message_id
            || message.id.is_empty()
            || message.id.len() > 128
            || change.user_id != message.author.id
            || change.user_id == origin.user_id
            || self.blocked.contains(&message.author.id)
        {
            return None;
        }
        let group = match &change.channel_id {
            // A private message to this account.
            None => {
                if message.channel_id.is_some()
                    || change.recipient_id.as_deref() != Some(origin.user_id.as_str())
                    || message.recipient_id.as_deref() != Some(origin.user_id.as_str())
                    || !self.peers.contains_key(&message.author.id)
                {
                    return None;
                }
                None
            }
            // A message in one of this account's groups, from a current member.
            Some(channel) => {
                let known = self.groups.get(channel)?;
                if message.channel_id.as_ref() != Some(channel)
                    || change.recipient_id.is_some()
                    || message.recipient_id.is_some()
                    || !known.members.contains(&message.author.id)
                {
                    return None;
                }
                Some(channel.clone())
            }
        };
        Some(Candidate {
            peer: message.author.id.clone(),
            group,
            mentioned: message.mentions.iter().any(|id| id == &origin.user_id),
        })
    }
    fn claim(&mut self, change: &MessageChange, who: Candidate, now: Instant, queue: bool) -> bool {
        let fresh = self.baseline.is_some_and(|b| change.sequence > b)
            && change.sequence > self.claimed_through
            && !self.seen.contains(&change.message_id);
        self.claimed_through = self.claimed_through.max(change.sequence);
        if !fresh {
            return false;
        }
        if self.seen.len() == MAX_IDS {
            self.seen.pop_front();
        }
        self.seen.push_back(change.message_id.clone());
        if queue && self.pending.len() < MAX_PENDING {
            self.pending.push(Pending {
                id: change.message_id.clone(),
                peer: who.peer,
                group: who.group,
                mentioned: who.mentioned,
                deadline: now + RETRY_LIFETIME,
                sent: false,
                retry_at: None,
            });
        }
        true
    }
}

impl Account {
    pub(super) fn observe_message_bootstrap(
        &mut self,
        b: &Bootstrap,
        received: Instant,
        cursor: Option<MessageCursor>,
    ) {
        if b.protocol_version < relay_protocol::API_PROTOCOL_VERSION
            || !b.capabilities.iter().any(|c| c == "people-v1")
        {
            if self.messages.take().is_some() {
                self.message_guard.revoke();
                self.message_guard = GenerationGuard::new();
            }
            return;
        }
        let peers: HashMap<_, _> = b
            .members
            .iter()
            .filter(|u| {
                u.id != b.user.id
                    && u.id.len() <= 128
                    && b.social.friends.contains(&u.id)
                    && !b.social.blocked.contains(&u.id)
            })
            .map(|u| {
                let name = if u.profile.display_name.trim().is_empty() {
                    &u.username
                } else {
                    &u.profile.display_name
                };
                (u.id.clone(), name.chars().take(128).collect::<String>())
            })
            .take(1024)
            .collect();
        let groups: HashMap<String, Group> = if b
            .capabilities
            .iter()
            .any(|c| c == relay_protocol::PEOPLE_GROUPS_CAPABILITY)
        {
            b.conversations
                .iter()
                .filter(|c| {
                    c.kind == "group"
                        && !c.archived
                        && c.member_ids.contains(&b.user.id)
                        && !b.conversations.iter().any(|room| {
                            room.kind == "group_voice"
                                && room.parent_id.as_ref() == Some(&c.channel_id)
                        })
                })
                .filter_map(|c| {
                    let channel = b
                        .channels
                        .iter()
                        .find(|channel| channel.id == c.channel_id && channel.can_send)?;
                    Some((
                        c.channel_id.clone(),
                        Group {
                            name: channel.name.chars().take(64).collect(),
                            members: c.member_ids.iter().take(16).cloned().collect(),
                            names: b
                                .members
                                .iter()
                                .filter(|u| c.member_ids.contains(&u.id))
                                .map(|u| {
                                    let shown = if u.profile.display_name.trim().is_empty() {
                                        &u.username
                                    } else {
                                        &u.profile.display_name
                                    };
                                    (u.id.clone(), shown.chars().take(128).collect::<String>())
                                })
                                .take(16)
                                .collect(),
                        },
                    ))
                })
                .take(256)
                .collect()
        } else {
            HashMap::new()
        };
        if self.messages.is_none() {
            // Only response parsing knows whether serde defaulted a missing
            // cursor. Explicit zero is a valid baseline for the very first DM.
            // Call-only typed snapshots cannot manufacture that knowledge.
            let Some(cursor) = cursor else {
                return;
            };
            self.messages = Some(Messages {
                baseline: cursor.value,
                cursor: cursor.value.unwrap_or(0),
                cursor_received: received,
                claimed_through: cursor.value.unwrap_or(0),
                owned_until: None,
                peers: HashMap::new(),
                groups: HashMap::new(),
                blocked: HashSet::new(),
                seen: VecDeque::new(),
                pending: Vec::new(),
            });
        }
        let messages = self
            .messages
            .as_mut()
            .expect("message observer initialized");
        let blocked: HashSet<String> = b.social.blocked.iter().take(1024).cloned().collect();
        if messages.peers != peers
            || messages.groups != groups
            || messages.blocked != blocked
            || self.quiet
        {
            self.message_guard.revoke();
            self.message_guard = GenerationGuard::new();
        }
        messages.peers = peers;
        messages.groups = groups;
        messages.blocked = blocked;
        // Someone who stopped being a friend, or a group this account left,
        // can no longer alert.
        messages.pending.retain(|p| match &p.group {
            None => messages.peers.contains_key(&p.peer),
            Some(g) => {
                !messages.blocked.contains(&p.peer)
                    && messages
                        .groups
                        .get(g)
                        .is_some_and(|group| group.members.contains(&p.peer))
            }
        });
        if cursor.is_some_and(|sample| {
            sample.value.is_some_and(|value| value < messages.cursor)
                && sample.requested.is_some_and(|requested| {
                    requested >= messages.cursor_received && requested <= received
                })
        }) {
            // A shared worker/initial bootstrap can have been captured before
            // a concurrent subscriber page even when it arrives later. Only
            // a fresh subscriber request proves this lower cursor is a reset.
            // Suppress its entire initial history chain before live arrivals.
            messages.cursor = b.latest_change_sequence;
            messages.cursor_received = received;
            messages.baseline = None;
            messages.claimed_through = b.latest_change_sequence;
            messages.rebind();
            messages.pending.clear();
            self.message_guard.revoke();
            self.message_guard = GenerationGuard::new();
        }
    }
}

impl State {
    pub(super) fn retire_message_account(&mut self, origin: &CommandOrigin) {
        if let Some(account) = self.accounts.iter().find(|a| &a.origin == origin) {
            account.message_guard.revoke();
            if account.messages.is_some() && !self.revoked_messages.contains(origin) {
                if self.revoked_messages.len() == MAX_ACCOUNTS {
                    self.revoked_messages.remove(0);
                }
                self.revoked_messages.push(origin.clone());
            }
        }
    }
    pub(super) fn message_cursor(&self, origin: &CommandOrigin, generation: u64) -> Option<u64> {
        if self.stopped {
            return None;
        }
        self.accounts
            .iter()
            .find(|a| &a.origin == origin && a.generation == generation)?
            .messages
            .as_ref()
            .map(|m| m.cursor)
    }
    pub(super) fn release_message_feed(&mut self, origin: &CommandOrigin, generation: u64) {
        if let Some(messages) = self
            .accounts
            .iter_mut()
            .find(|a| &a.origin == origin && a.generation == generation)
            .and_then(|a| a.messages.as_mut())
        {
            messages.owned_until = None;
        }
    }
    pub(super) fn message_page(
        &mut self,
        origin: &CommandOrigin,
        generation: u64,
        after: u64,
        page: &ChangePage,
        now: Instant,
    ) -> bool {
        if self.stopped {
            return false;
        }
        let Some(account) = self
            .accounts
            .iter_mut()
            .find(|a| &a.origin == origin && a.generation == generation)
        else {
            return false;
        };
        let Some(messages) = &mut account.messages else {
            return false;
        };
        if after != messages.cursor
            || page.next_cursor < after
            || page.changes.len() > 200
            || page.has_more && page.next_cursor == after
        {
            return false;
        }
        let mut previous = after;
        for change in &page.changes {
            if change.sequence <= previous || change.sequence > page.next_cursor {
                return false;
            }
            previous = change.sequence;
        }
        // Only a validated changes reply temporarily transfers UI ownership.
        messages.owned_until = Some(now + OWNERSHIP_LIFETIME);
        if messages.baseline.is_none() {
            // Legacy/reset history can span many bounded pages. None remains
            // a suppression fence until the entire initial chain is consumed.
            if !page.has_more {
                messages.baseline = Some(page.next_cursor);
            }
            messages.claimed_through = page.next_cursor;
        } else {
            for change in &page.changes {
                if let Some(peer) = messages.candidate(origin, change) {
                    messages.claim(change, peer, now, true);
                } else {
                    messages.claimed_through = messages.claimed_through.max(change.sequence);
                }
            }
        }
        messages.cursor = page.next_cursor;
        messages.cursor_received = now;
        true
    }
    pub(super) fn ui_message_claims(
        &mut self,
        origin: &CommandOrigin,
        changes: &[MessageChange],
        now: Instant,
    ) -> Vec<Result<Option<GenerationGuard>, ()>> {
        let stopped = self.stopped || self.revoked_messages.contains(origin);
        let account = self.accounts.iter_mut().find(|a| &a.origin == origin);
        let Some(account) = account else {
            return changes
                .iter()
                .map(|_| if stopped { Err(()) } else { Ok(None) })
                .collect();
        };
        let Some(messages) = &mut account.messages else {
            return vec![Ok(None); changes.len()];
        };
        let guard = account.message_guard.clone();
        let owned = messages.owned_until.is_some_and(|until| now < until);
        changes
            .iter()
            .map(|change| {
                if stopped || !guard.valid() {
                    return Err(());
                }
                // Community messages keep their existing UI notification path; only
                // a group of this account's friends is claimed like a private message.
                if change
                    .channel_id
                    .as_ref()
                    .is_some_and(|channel| !messages.groups.contains_key(channel))
                {
                    return Ok(None);
                }
                let Some(peer) = messages.candidate(origin, change) else {
                    return Err(());
                };
                if !messages.claim(change, peer, now, owned) {
                    return Err(());
                }
                if owned {
                    Err(())
                } else {
                    Ok(Some(guard.clone()))
                }
            })
            .collect()
    }
    pub(super) fn message_effects(
        &mut self,
        now: Instant,
        unix: u64,
        focused: bool,
    ) -> Vec<Delivery> {
        let mut effects = Vec::new();
        for account in &mut self.accounts {
            let Some(messages) = &mut account.messages else {
                continue;
            };
            messages.pending.retain(|p| now < p.deadline);
            if self.stopped
                || self.policy.disabled
                || !self.policy.notifications
                || account.quiet
                || focused
            {
                messages.pending.clear();
                continue;
            }
            for pending in &mut messages.pending {
                if pending.sent || pending.retry_at.is_some_and(|at| now < at) {
                    continue;
                }
                // Names are shown only with previews on; the message itself never is.
                let (target, body) = match &pending.group {
                    None => {
                        let Some(name) = messages.peers.get(&pending.peer) else {
                            pending.sent = true;
                            continue;
                        };
                        if !direct_policy_allows(
                            &self.policy.message_policies,
                            &account.origin,
                            &pending.peer,
                            self.policy.mentions_only,
                            unix,
                        ) {
                            pending.sent = true;
                            continue;
                        }
                        (
                            Target::Direct(pending.peer.clone()),
                            if self.policy.previews {
                                format!("{name} sent you a private message. Open Relay to read it.")
                            } else {
                                "Open Relay to read your private message.".into()
                            },
                        )
                    }
                    Some(channel) => {
                        let Some(group) = messages.groups.get(channel) else {
                            pending.sent = true;
                            continue;
                        };
                        let Some(name) = group.names.get(&pending.peer) else {
                            pending.sent = true;
                            continue;
                        };
                        if !group.members.contains(&pending.peer)
                            || !group_policy_allows(
                                &self.policy.message_policies,
                                &account.origin,
                                channel,
                                pending.mentioned,
                                unix,
                            )
                        {
                            pending.sent = true;
                            continue;
                        }
                        (
                            Target::Channel(channel.clone()),
                            if self.policy.previews {
                                format!("{name} wrote in {}. Open Relay to read it.", group.name)
                            } else {
                                "Open Relay to read your group message.".into()
                            },
                        )
                    }
                };
                pending.sent = true;
                effects.push(Delivery {
                    origin: account.origin.clone(),
                    id: pending.id.clone(),
                    notification: Notification {
                        title: "New Relay message".into(),
                        body,
                        origin: account.origin.clone(),
                        target,
                        guard: Some(account.message_guard.clone()),
                        background_only: true,
                    },
                });
            }
        }
        effects
    }
    pub(super) fn delivered_message(&mut self, origin: &CommandOrigin, id: &str) {
        if let Some(messages) = self
            .accounts
            .iter_mut()
            .find(|a| &a.origin == origin)
            .and_then(|a| a.messages.as_mut())
        {
            messages.pending.retain(|p| p.id != id);
        }
    }
    pub(super) fn retry_message(&mut self, origin: &CommandOrigin, id: &str, now: Instant) {
        if let Some(pending) = self
            .accounts
            .iter_mut()
            .find(|a| &a.origin == origin)
            .and_then(|a| a.messages.as_mut())
            .and_then(|m| m.pending.iter_mut().find(|p| p.id == id))
        {
            pending.sent = false;
            pending.retry_at = Some(now + Duration::from_secs(1));
        }
    }
}

impl Alerts {
    pub(super) fn reset_message_guards(inner: &mut Inner) {
        for account in &mut inner.state.accounts {
            account.message_guard.revoke();
            account.message_guard = GenerationGuard::new();
        }
    }
    pub fn message_cursor(&self, origin: &CommandOrigin, generation: u64) -> Option<u64> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .state
            .message_cursor(origin, generation)
    }
    pub fn release_message_feed(&self, origin: &CommandOrigin, generation: u64) {
        let mut inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        inner.state.release_message_feed(origin, generation);
    }
    pub fn observe_message_page(
        &self,
        origin: &CommandOrigin,
        generation: u64,
        after: u64,
        page: &ChangePage,
    ) -> bool {
        let mut inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let valid = inner
            .state
            .message_page(origin, generation, after, page, Instant::now());
        if valid {
            Self::apply(&mut inner, Instant::now());
        }
        valid
    }
    pub fn retry_ui_message(
        &self,
        origin: &CommandOrigin,
        guard: &GenerationGuard,
        change: &MessageChange,
    ) {
        let mut inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if inner.state.stopped || !guard.valid() {
            return;
        }
        let Some(account) = inner
            .state
            .accounts
            .iter_mut()
            .find(|a| &a.origin == origin && &a.message_guard == guard)
        else {
            return;
        };
        let Some(messages) = &mut account.messages else {
            return;
        };
        let Some(who) = messages.candidate(origin, change) else {
            return;
        };
        if messages.pending.len() < MAX_PENDING
            && !messages.pending.iter().any(|p| p.id == change.message_id)
        {
            messages.pending.push(Pending {
                id: change.message_id.clone(),
                peer: who.peer,
                group: who.group,
                mentioned: who.mentioned,
                deadline: Instant::now() + RETRY_LIFETIME,
                sent: false,
                retry_at: Some(Instant::now() + Duration::from_secs(1)),
            });
        }
    }
    pub fn claim_ui_messages(
        &self,
        origin: &CommandOrigin,
        changes: &[MessageChange],
    ) -> Vec<Result<Option<GenerationGuard>, ()>> {
        let mut inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let claims = inner
            .state
            .ui_message_claims(origin, changes, Instant::now());
        Self::apply(&mut inner, Instant::now());
        claims
    }
}

#[cfg(test)]
#[path = "message_alerts_tests.rs"]
mod tests;
