//! Reactions are authenticated MLS actions; the host's reaction rows are not
//! evidence about a protected message.
use super::*;
use crate::e2ee::{CachedEnvelope, MessageAction};
use std::collections::{BTreeMap, HashSet};

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Original {
    Protected(String, String),
    Legacy(String),
}

struct State {
    sequence: u64,
    active: bool,
}

pub(super) struct ReactionProjection {
    protected: HashMap<String, HashSet<(String, String)>>,
    states: BTreeMap<(Original, String, String), State>,
}

impl ReactionProjection {
    #[cfg(test)]
    pub(super) fn from_authenticated<'a>(
        scope: &str,
        cached: impl IntoIterator<Item = &'a CachedEnvelope>,
    ) -> Self {
        Self::from_history(scope, cached.into_iter().cloned().map(Ok))
            .expect("in-memory authenticated entries have no storage errors")
    }

    pub(super) fn from_history(
        scope: &str,
        cached: impl IntoIterator<Item = Result<CachedEnvelope, String>>,
    ) -> Result<Self, String> {
        let mut projection = Self {
            protected: HashMap::new(),
            states: BTreeMap::new(),
        };
        let mut seen = HashSet::new();
        for cached in cached {
            let cached = cached?;
            let envelope = &cached.envelope;
            let Some(delivery_id) = &cached.message_id else {
                continue;
            };
            if envelope.scope != scope
                || cached.sequence == 0
                || cached.author.user_id.is_empty()
                || envelope.client_id.is_empty()
                || !seen.insert((
                    cached.author.user_id.clone(),
                    cached.author.device_id.clone(),
                    envelope.client_id.clone(),
                ))
            {
                continue;
            }
            let Some(MessageAction::Reaction {
                message_id,
                target_client_id,
                emoji,
                active,
            }) = &envelope.action
            else {
                projection
                    .protected
                    .entry(delivery_id.clone())
                    .or_default()
                    .insert((envelope.client_id.clone(), cached.author.user_id.clone()));
                continue;
            };
            if message_id.is_empty()
                || message_id.len() > 128
                || delivery_id == message_id
                || emoji.is_empty()
                || emoji.len() > 128
                || emoji.chars().any(char::is_control)
                || !envelope.content.is_empty()
                || !envelope.attachments.is_empty()
                || !envelope.mentions.is_empty()
                || envelope.reply_to_id.is_some()
                || target_client_id
                    .as_ref()
                    .is_some_and(|id| id.is_empty() || id.len() > 128 || id == &envelope.client_id)
            {
                continue;
            }
            let original = match target_client_id {
                Some(id) => Original::Protected(message_id.clone(), id.clone()),
                None => Original::Legacy(message_id.clone()),
            };
            let key = (original, emoji.clone(), cached.author.user_id.clone());
            match projection.states.get_mut(&key) {
                Some(state) if cached.sequence > state.sequence => {
                    *state = State {
                        sequence: cached.sequence,
                        active: *active,
                    };
                }
                // A duplicate/conflicting sequence cannot replace the first
                // authenticated intent. Ordinary repeated history is harmless.
                Some(_) => {}
                None => {
                    projection.states.insert(
                        key,
                        State {
                            sequence: cached.sequence,
                            active: *active,
                        },
                    );
                }
            }
        }
        Ok(projection)
    }

    pub(super) fn apply(&self, messages: &mut [Message]) {
        for message in messages {
            let protected = self.protected.get(&message.id).and_then(|identities| {
                identities.iter().find(|(id, author)| {
                    message.client_id.as_ref() == Some(id) && &message.author.id == author
                })
            });
            let original = if let Some((client_id, _)) = protected {
                message.reactions.clear();
                Original::Protected(message.id.clone(), client_id.clone())
            } else if message.content.starts_with(crate::e2ee::MESSAGE_PREFIX)
                || message.content == "Encrypted message unavailable"
            {
                // An unavailable ciphertext must not acquire a trusted-looking
                // reaction count from legacy host-controlled rows.
                message.reactions.clear();
                continue;
            } else {
                Original::Legacy(message.id.clone())
            };
            let mut rows: BTreeMap<String, HashSet<String>> = message
                .reactions
                .iter()
                .filter(|r| !r.emoji.is_empty() && r.emoji.len() <= 128)
                .map(|r| {
                    (
                        r.emoji.clone(),
                        r.user_ids
                            .iter()
                            .filter(|id| !id.is_empty())
                            .cloned()
                            .collect(),
                    )
                })
                .collect();
            for ((target, emoji, author), state) in &self.states {
                if target != &original {
                    continue;
                }
                let users = rows.entry(emoji.clone()).or_default();
                if state.active {
                    users.insert(author.clone());
                } else {
                    users.remove(author);
                }
            }
            message.reactions = rows
                .into_iter()
                .filter_map(|(emoji, users)| {
                    if users.is_empty() {
                        return None;
                    }
                    let mut user_ids: Vec<_> = users.into_iter().collect();
                    user_ids.sort_unstable();
                    Some(Reaction { emoji, user_ids })
                })
                .collect();
        }
    }
}

impl Worker {
    pub(super) fn send_reaction(
        &mut self,
        target: Target,
        message_id: String,
        target_client_id: Option<String>,
        emoji: String,
        active: bool,
    ) -> Result<(), String> {
        // The authenticated original identity comes from the displayed message's
        // protection status. Changing the community mode must not downgrade its
        // reactions into host-controlled rows, which protected history ignores.
        if target_client_id.is_none()
            && self
                .connection
                .as_ref()
                .is_some_and(|c| c.chat.plain_allowed())
        {
            super::extended::path_id(&message_id)?;
            let _: Message = self.post(
                &format!("/api/messages/{message_id}/reactions"),
                &serde_json::json!({"emoji":emoji,"active":active}),
            )?;
            return self.load_messages();
        }
        let scope = self.crypto_scope(&target)?;
        let action = MessageAction::Reaction {
            message_id,
            target_client_id,
            emoji,
            active,
        };
        let client_id = self
            .connection
            .as_ref()
            .and_then(|c| c.crypto.as_ref())
            .and_then(|crypto| crypto.pending_action_id(&scope, &action))
            .unwrap_or_else(operation_id);
        let content = self.encrypt_action(&target, &client_id, action)?;
        let (channel_id, recipient_id) = match &target {
            Target::Channel(id) => (Some(id.clone()), None),
            Target::Direct(id) => (None, Some(id.clone())),
        };
        let body = SendMessage {
            channel_id,
            recipient_id,
            content,
            client_id: Some(client_id.clone()),
            ..Default::default()
        };
        let first = self
            .request(reqwest::Method::POST, "/api/messages")?
            .json(&body)
            .send();
        let response = match first {
            Ok(response) => response,
            Err(error)
                if error.is_connect()
                    || error.is_timeout()
                    || error.is_body()
                    || error.is_request() =>
            {
                // Reuse the exact ciphertext and ID for an uncertain commit.
                // Definite HTTP errors, including 429, never enter this branch.
                self.request(reqwest::Method::POST, "/api/messages")?
                    .json(&body)
                    .send()
                    .map_err(network_error)?
            }
            Err(error) => return Err(network_error(error)),
        };
        let message: Message = parse(response)?;
        let own_id = &self
            .connection
            .as_ref()
            .ok_or("Not connected.")?
            .session
            .user
            .id;
        if message.content != body.content
            || message.client_id != body.client_id
            || &message.author.id != own_id
            || message.channel_id != body.channel_id
            || message.recipient_id != body.recipient_id
        {
            return Err(
                "The encrypted reaction acknowledgement does not match its original request."
                    .into(),
            );
        }
        let crypto = self
            .connection
            .as_mut()
            .and_then(|c| c.crypto.as_mut())
            .ok_or("Encryption keys are not ready.")?;
        crypto.bind_delivery(&message.content, &message.id, message.sequence)?;
        crypto.committed(&client_id)?;
        self.schedule_legacy_reactions(&target, std::slice::from_ref(&message.id), &[]);
        self.load_messages()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::e2ee::Envelope;
    fn cached(
        id: &str,
        client: &str,
        author: &str,
        sequence: u64,
        action: Option<MessageAction>,
    ) -> CachedEnvelope {
        CachedEnvelope {
            author: relay_crypto::Identity {
                server_id: "server".into(),
                user_id: author.into(),
                device_id: format!("device-{author}"),
            },
            envelope: Envelope {
                version: 1,
                scope: "scope".into(),
                client_id: client.into(),
                content: if action.is_none() {
                    "private original".into()
                } else {
                    String::new()
                },
                reply_to_id: None,
                mentions: Vec::new(),
                attachments: Vec::new(),
                action,
            },
            message_id: Some(id.into()),
            sequence,
            delivery_revision: sequence,
        }
    }
    fn action(
        id: &str,
        client: &str,
        author: &str,
        sequence: u64,
        target: Option<&str>,
        active: bool,
    ) -> CachedEnvelope {
        cached(
            id,
            client,
            author,
            sequence,
            Some(MessageAction::Reaction {
                message_id: "original".into(),
                target_client_id: target.map(str::to_owned),
                emoji: "<3".into(),
                active,
            }),
        )
    }
    fn original() -> Message {
        Message {
            id: "original".into(),
            client_id: Some("original-client".into()),
            author: User {
                id: "alice".into(),
                ..Default::default()
            },
            content: "private original".into(),
            reactions: vec![Reaction {
                emoji: "forged".into(),
                user_ids: vec!["host".into()],
            }],
            ..Default::default()
        }
    }
    #[test]
    fn streaming_history_keeps_reactions_after_the_old_8192_limit_and_propagates_storage_failure() {
        let entries = std::iter::once(Ok(cached("original", "original-client", "alice", 1, None)))
            .chain((0..8400).map(|n| {
                Ok(cached(
                    &format!("other-{n}"),
                    &format!("other-client-{n}"),
                    "alice",
                    n + 2,
                    None,
                ))
            }))
            .chain(std::iter::once(Ok(action(
                "late-reaction",
                "late-reaction-client",
                "bob",
                9000,
                Some("original-client"),
                true,
            ))));
        let mut messages = vec![original()];
        ReactionProjection::from_history("scope", entries)
            .unwrap()
            .apply(&mut messages);
        assert_eq!(messages[0].reactions[0].user_ids, vec!["bob"]);
        let failed = [
            Ok(cached("original", "original-client", "alice", 1, None)),
            Err("synthetic damaged archive".into()),
        ];
        assert!(ReactionProjection::from_history("scope", failed).is_err());
    }
    #[test]
    fn authenticated_author_and_original_client_identity_replace_forged_protected_rows() {
        let entries = vec![
            cached("original", "original-client", "alice", 1, None),
            action(
                "action",
                "action-client",
                "bob",
                2,
                Some("original-client"),
                true,
            ),
        ];
        let mut messages = vec![original()];
        ReactionProjection::from_authenticated("scope", &entries).apply(&mut messages);
        assert_eq!(messages[0].reactions.len(), 1);
        assert_eq!(messages[0].reactions[0].emoji, "<3");
        assert_eq!(messages[0].reactions[0].user_ids, vec!["bob"]);
    }
    #[test]
    fn repeated_or_reordered_history_keeps_latest_explicit_state_and_other_authors() {
        let on = action("on", "on-client", "bob", 2, Some("original-client"), true);
        let entries = vec![
            cached("original", "original-client", "alice", 1, None),
            action(
                "off",
                "off-client",
                "bob",
                5,
                Some("original-client"),
                false,
            ),
            on.clone(),
            on,
            action(
                "carol",
                "carol-client",
                "carol",
                4,
                Some("original-client"),
                true,
            ),
        ];
        let projection = ReactionProjection::from_authenticated("scope", &entries);
        let mut messages = vec![original()];
        projection.apply(&mut messages);
        projection.apply(&mut messages);
        assert_eq!(messages[0].reactions[0].user_ids, vec!["carol"]);
    }
    #[test]
    fn cross_scope_unbound_and_wrong_original_actions_cannot_add_a_reaction() {
        let mut foreign = action(
            "foreign",
            "foreign-client",
            "bob",
            2,
            Some("original-client"),
            true,
        );
        foreign.envelope.scope = "other".into();
        let mut unbound = action(
            "unbound",
            "unbound-client",
            "carol",
            3,
            Some("original-client"),
            true,
        );
        unbound.message_id = None;
        let entries = vec![
            cached("original", "original-client", "alice", 1, None),
            foreign,
            unbound,
            action(
                "wrong",
                "wrong-client",
                "dave",
                4,
                Some("different-original-client"),
                true,
            ),
            action("legacy", "legacy-client", "eve", 5, None, true),
        ];
        let mut messages = vec![original()];
        ReactionProjection::from_authenticated("scope", &entries).apply(&mut messages);
        assert!(messages[0].reactions.is_empty());
    }
    #[test]
    fn action_identity_cannot_react_to_itself_or_smuggle_text_and_legacy_rows_are_separate() {
        let mut text = action("text", "text-client", "bob", 2, None, true);
        text.envelope.content = "smuggled".into();
        let mut self_action = action("original", "self-client", "carol", 3, None, true);
        self_action.envelope.client_id = "self-client".into();
        let entries = vec![
            text,
            self_action,
            action("valid", "valid-client", "dave", 4, None, true),
        ];
        let mut legacy = original();
        legacy.client_id = None;
        let mut messages = vec![legacy];
        ReactionProjection::from_authenticated("scope", &entries).apply(&mut messages);
        assert_eq!(messages[0].reactions.len(), 2);
        assert_eq!(messages[0].reactions[0].user_ids, vec!["dave"]);
        assert_eq!(messages[0].reactions[1].emoji, "forged");
    }
}
