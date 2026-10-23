//! Resolve older plaintext originals without trusting host reactions on
//! protected messages or letting a burst monopolize the command worker.
use super::*;
use std::collections::{HashSet, VecDeque};

const LIMIT: usize = 64;
type Key = (Target, String);

struct Pending {
    key: Key,
    attempts: u8,
    due: Instant,
}

#[derive(Default)]
pub(super) struct LegacyReactions {
    origin: Option<CommandOrigin>,
    pending: VecDeque<Pending>,
    originals: HashMap<Key, Message>,
    order: VecDeque<Key>,
}

impl LegacyReactions {
    pub(super) fn next_deadline(&self) -> Option<Instant> {
        self.pending.iter().map(|pending| pending.due).min()
    }
    #[cfg(test)]
    pub(super) fn sizes(&self) -> (usize, usize) {
        (self.pending.len(), self.originals.len())
    }
    pub(super) fn clear(&mut self) {
        *self = Self::default();
    }

    fn set_origin(&mut self, origin: CommandOrigin) {
        if self.origin.as_ref() != Some(&origin) {
            self.clear();
            self.origin = Some(origin);
        }
    }

    fn remember(&mut self, key: Key, message: Message) {
        // Only routing and existing Legacy rows are needed for projection.
        // Bound host-controlled rows independently of the HTTP body limit.
        let message = Message {
            id: message.id,
            channel_id: message.channel_id,
            recipient_id: message.recipient_id,
            author: User {
                id: message.author.id,
                ..Default::default()
            },
            reactions: message
                .reactions
                .into_iter()
                .filter(|row| !row.emoji.is_empty() && row.emoji.len() <= 128)
                .take(64)
                .map(|row| Reaction {
                    emoji: row.emoji,
                    user_ids: row
                        .user_ids
                        .into_iter()
                        .filter(|id| !id.is_empty() && id.len() <= 128)
                        .take(64)
                        .collect(),
                })
                .collect(),
            ..Default::default()
        };
        if !self.originals.contains_key(&key) {
            if self.order.len() >= LIMIT {
                if let Some(old) = self.order.pop_front() {
                    self.originals.remove(&old);
                }
            }
            self.order.push_back(key.clone());
        }
        self.originals.insert(key.clone(), message);
        self.pending.retain(|pending| pending.key != key);
    }

    fn enqueue(&mut self, key: Key) {
        if self.pending.len() < LIMIT && !self.pending.iter().any(|pending| pending.key == key) {
            self.pending.push_back(Pending {
                key,
                attempts: 0,
                due: Instant::now(),
            });
        }
    }
}

fn belongs_to(message: &Message, target: &Target, own: &str) -> bool {
    match target {
        Target::Channel(id) => {
            message.channel_id.as_ref() == Some(id) && message.recipient_id.is_none()
        }
        Target::Direct(peer) => {
            message.channel_id.is_none()
                && ((message.author.id == own && message.recipient_id.as_ref() == Some(peer))
                    || (message.author.id == *peer && message.recipient_id.as_deref() == Some(own)))
        }
    }
}

fn protected_id(c: &Connection, scope: &str, id: &str) -> bool {
    c.crypto.as_ref().is_some_and(|crypto| {
        crypto.cached_envelopes(scope).any(|cached| match cached {
            Ok(cached) => {
                cached.message_id.as_deref() == Some(id) && cached.envelope.action.is_none()
            }
            Err(_) => true,
        })
    })
}

impl Worker {
    pub(super) fn schedule_legacy_reactions(
        &mut self,
        target: &Target,
        delivered: &[String],
        raw: &[Message],
    ) {
        let Some(c) = self.connection.as_ref() else {
            self.legacy_reactions.clear();
            return;
        };
        let origin = CommandOrigin::from(c);
        if self.command_connection(&origin).is_err() {
            self.legacy_reactions.clear();
            return;
        }
        let Some(crypto) = &c.crypto else {
            return;
        };
        let Ok(scope) = self.crypto_scope(target) else {
            return;
        };
        let delivered: HashSet<_> = delivered.iter().map(String::as_str).collect();
        let mut changed = Vec::new();
        for entry in crypto.cached_envelopes(&scope) {
            let cached = match entry {
                Ok(cached) => cached,
                Err(error) => {
                    self.emit(Update::Error(error));
                    return;
                }
            };
            if !cached
                .message_id
                .as_deref()
                .is_some_and(|id| delivered.contains(id))
            {
                continue;
            }
            match &cached.envelope.action {
                Some(crate::e2ee::MessageAction::Reaction {
                    message_id,
                    target_client_id: None,
                    ..
                }) if extended::path_id(message_id).is_ok()
                    && !protected_id(c, &scope, message_id) =>
                {
                    changed.push(message_id.clone());
                }
                _ => {}
            }
        }
        changed.sort_unstable();
        changed.dedup();
        let originals: Vec<_> = raw
            .iter()
            .filter(|message| {
                !message.content.starts_with(crate::e2ee::MESSAGE_PREFIX)
                    && message.content != "Encrypted message unavailable"
                    && belongs_to(message, target, &c.session.user.id)
                    && !protected_id(c, &scope, &message.id)
            })
            .take(LIMIT)
            .cloned()
            .collect();
        self.legacy_reactions.set_origin(origin);
        for message in originals {
            self.legacy_reactions
                .remember((target.clone(), message.id.clone()), message);
        }
        for id in changed {
            let key = (target.clone(), id);
            if self.legacy_reactions.originals.contains_key(&key) {
                self.emit_legacy_reaction_snapshot(&key);
            } else {
                self.legacy_reactions.enqueue(key);
            }
        }
    }

    fn emit_legacy_reaction_snapshot(&self, key: &Key) {
        let (Some(c), Some(message)) = (&self.connection, self.legacy_reactions.originals.get(key))
        else {
            return;
        };
        if self.legacy_reactions.origin.as_ref() != Some(&CommandOrigin::from(c)) {
            return;
        }
        let Ok(scope) = self.crypto_scope(&key.0) else {
            return;
        };
        let Some(crypto) = &c.crypto else {
            return;
        };
        if protected_id(c, &scope, &key.1)
            || self.command_connection(&CommandOrigin::from(c)).is_err()
        {
            return;
        }
        let mut originals = vec![message.clone()];
        match super::reactions::ReactionProjection::from_history(
            &scope,
            crypto.cached_envelopes(&scope),
        ) {
            Ok(projection) => projection.apply(&mut originals),
            Err(error) => {
                self.emit(Update::Error(error));
                return;
            }
        }
        self.emit(Update::ReactionSnapshot {
            url: c.url.clone(),
            target: key.0.clone(),
            message_id: key.1.clone(),
            target_client_id: None,
            rows: originals.remove(0).reactions,
        });
    }

    pub(super) fn poll_legacy_reactions(&mut self) {
        let Some(c) = &self.connection else {
            self.legacy_reactions.clear();
            return;
        };
        let origin = CommandOrigin::from(c);
        if self.legacy_reactions.origin.as_ref() != Some(&origin)
            || self.command_connection(&origin).is_err()
        {
            self.legacy_reactions.clear();
            return;
        }
        let Some(index) = self
            .legacy_reactions
            .pending
            .iter()
            .position(|pending| pending.due <= Instant::now())
        else {
            return;
        };
        let mut pending = self.legacy_reactions.pending.remove(index).unwrap();
        let result = (|| -> Result<Option<Message>, String> {
            let scope = self.crypto_scope(&pending.key.0)?;
            if protected_id(self.command_connection(&origin)?, &scope, &pending.key.1) {
                return Ok(None);
            }
            let request = self
                .request(reqwest::Method::GET, "/api/history")?
                .query(&[("around", &pending.key.1)])
                .timeout(Duration::from_millis(500));
            let request = match &pending.key.0 {
                Target::Channel(id) => request.query(&[("channel_id", id)]),
                Target::Direct(id) => request.query(&[("peer_id", id)]),
            };
            let response = request.send().map_err(network_error)?;
            if matches!(
                response.status(),
                reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::FORBIDDEN
            ) {
                return Ok(None);
            }
            let page: HistoryPage = read_json(checked(response)?, 8 * 1024 * 1024)?;
            if page.messages.len() > 100 {
                return Err("The reaction context exceeds its limit.".into());
            }
            self.command_connection(&origin)?;
            Ok(page.messages.into_iter().find(|message| {
                message.id == pending.key.1
                    && belongs_to(message, &pending.key.0, &origin.user_id)
                    && !message.content.starts_with(crate::e2ee::MESSAGE_PREFIX)
                    && message.content != "Encrypted message unavailable"
            }))
        })();
        match result {
            Ok(Some(message)) => {
                self.legacy_reactions.remember(pending.key.clone(), message);
                self.emit_legacy_reaction_snapshot(&pending.key);
            }
            Ok(None) => {}
            Err(_) if pending.attempts < 2 => {
                pending.attempts += 1;
                pending.due = Instant::now() + Duration::from_secs(1);
                self.legacy_reactions.pending.push_back(pending);
            }
            Err(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_reaction_queue_and_cache_are_bounded_deduplicated_and_account_scoped() {
        let mut queue = LegacyReactions::default();
        let origin = CommandOrigin {
            url: "http://127.0.0.1:4580".into(),
            server_id: "server".into(),
            user_id: "alice".into(),
        };
        queue.set_origin(origin.clone());
        for id in 0..128 {
            let key = (Target::Channel("room".into()), id.to_string());
            queue.enqueue(key.clone());
            queue.enqueue(key);
        }
        assert_eq!(queue.sizes(), (64, 0));
        for id in 0..128 {
            queue.remember(
                (Target::Channel("room".into()), id.to_string()),
                Message {
                    id: id.to_string(),
                    content: "a long Legacy original need not remain in this cache".into(),
                    ..Default::default()
                },
            );
        }
        assert_eq!(queue.sizes(), (0, 64));
        assert!(queue
            .originals
            .values()
            .all(|message| message.content.is_empty()));
        queue.set_origin(CommandOrigin {
            user_id: "bob".into(),
            ..origin
        });
        assert_eq!(queue.sizes(), (0, 0));
    }
}
