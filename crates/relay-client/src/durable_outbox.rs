//! Outgoing requests survive restart without changing their send identity.
//! This is metadata for manual retry, not an automatic delivery queue. Uploaded
//! file bytes remain on the server; an expired upload cannot be recreated here.
use crate::network::{CommandOrigin, SendBinding, Target};
use relay_protocol::{Attachment, Message, User};
use serde::{Deserialize, Deserializer, Serialize};

const MAX_ENTRIES: usize = 64;
const MAX_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Entry {
    pub scope: String,
    pub origin: CommandOrigin,
    pub target: Target,
    pub binding: SendBinding,
    pub client_id: String,
    pub content: String,
    pub attachments: Vec<Attachment>,
    pub reply_to_id: Option<String>,
    pub mentions: Vec<String>,
    pub created_at: String,
}

impl Entry {
    pub fn from_message(
        scope: String,
        origin: CommandOrigin,
        target: Target,
        binding: SendBinding,
        message: &Message,
    ) -> Result<Self, String> {
        let client_id = message
            .client_id
            .clone()
            .ok_or("This message has no send identity.")?;
        if message.id != format!("local:{client_id}")
            || !message_matches(message, &origin.user_id, &target)
        {
            return Err("This outgoing message belongs to another conversation or account.".into());
        }
        let entry = Self {
            scope,
            origin,
            target,
            binding,
            client_id,
            content: message.content.clone(),
            attachments: message.attachments.clone(),
            reply_to_id: message.reply_to_id.clone(),
            mentions: message.mentions.clone(),
            created_at: message.created_at.clone(),
        };
        entry.validate()?;
        Ok(entry)
    }

    /// The caller presents restored entries as failed/manual retry only. It
    /// must never replace the current composer or automatically dispatch them.
    pub fn message(&self, author: &User) -> Message {
        Message {
            id: format!("local:{}", self.client_id),
            channel_id: match &self.target {
                Target::Channel(id) => Some(id.clone()),
                Target::Direct(_) => None,
            },
            recipient_id: match &self.target {
                Target::Direct(id) => Some(id.clone()),
                Target::Channel(_) => None,
            },
            author: author.clone(),
            content: self.content.clone(),
            created_at: self.created_at.clone(),
            attachments: self.attachments.clone(),
            reply_to_id: self.reply_to_id.clone(),
            mentions: self.mentions.clone(),
            client_id: Some(self.client_id.clone()),
            ..Default::default()
        }
    }

    fn in_scope(&self, scope: &str, origin: &CommandOrigin) -> bool {
        // conversation_scope resolves only authenticated host relocations. The
        // explicit server/account checks prevent an unrelated server at a reused
        // URL from inheriting an earlier outgoing request.
        self.scope == scope
            && self.origin.server_id == origin.server_id
            && self.origin.user_id == origin.user_id
    }

    fn same_request(&self, other: &Self) -> bool {
        self.binding == other.binding
            && self.content == other.content
            && self.reply_to_id == other.reply_to_id
            && self.mentions == other.mentions
            && self.attachments.len() == other.attachments.len()
            && self
                .attachments
                .iter()
                .zip(&other.attachments)
                .all(|(a, b)| {
                    a.id == b.id
                        && a.name == b.name
                        && a.mime == b.mime
                        && a.size == b.size
                        && a.url == b.url
                })
    }

    fn validate(&self) -> Result<(), String> {
        let target = match &self.target {
            Target::Channel(id) | Target::Direct(id) => id,
        };
        let id = |value: &str| !value.is_empty() && value.len() <= 256;
        if self.scope.is_empty()
            || self.scope.len() > 4096
            || self.origin.url.is_empty()
            || self.origin.url.len() > 2048
            || !id(&self.origin.server_id)
            || !id(&self.origin.user_id)
            || !id(target)
            || self.client_id.is_empty()
            || self.client_id.len() > 100
            || self.content.len() > 10_000
            || self.created_at.len() > 128
            || self.attachments.len() > 8
            || self.mentions.len() > 64
            || self.reply_to_id.as_deref().is_some_and(|value| !id(value))
            || self.mentions.iter().any(|value| !id(value))
            || self.attachments.iter().any(|file| {
                !id(&file.id)
                    || file.name.len() > 1024
                    || file.mime.len() > 256
                    || file.url.len() > 4096
            })
            || self.binding.device_id.len() > 256
            || self.binding.fingerprint.len() > 256
            || (!self.binding.plain
                && (self.binding.device_id.is_empty() || self.binding.fingerprint.is_empty()))
        {
            return Err("This outgoing message exceeds local storage limits or has an invalid send identity. It was not sent.".into());
        }
        Ok(())
    }
}

/// Unlike conversation history, unsent requests are never evicted by recency.
/// A full store rejects the new request without changing any existing entries.
#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct Outbox {
    entries: Vec<Entry>,
}

impl<'de> Deserialize<'de> for Outbox {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Wire {
            #[serde(default)]
            entries: Vec<Entry>,
        }
        let wire = Wire::deserialize(deserializer)?;
        let mut outbox = Self::default();
        for entry in wire.entries {
            if !outbox.insert(entry).map_err(serde::de::Error::custom)? {
                return Err(serde::de::Error::custom(
                    "Duplicate outgoing send identity.",
                ));
            }
        }
        Ok(outbox)
    }
}

impl Outbox {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn scoped<'a>(
        &'a self,
        scope: &'a str,
        origin: &'a CommandOrigin,
    ) -> impl Iterator<Item = &'a Entry> + 'a {
        self.entries
            .iter()
            .filter(move |entry| entry.in_scope(scope, origin))
    }

    pub fn get(
        &self,
        scope: &str,
        origin: &CommandOrigin,
        target: &Target,
        client_id: &str,
    ) -> Option<&Entry> {
        self.entries.iter().find(|entry| {
            entry.in_scope(scope, origin) && entry.target == *target && entry.client_id == client_id
        })
    }

    pub fn insert(&mut self, entry: Entry) -> Result<bool, String> {
        entry.validate()?;
        if let Some(existing) = self.entries.iter().find(|saved| {
            saved.in_scope(&entry.scope, &entry.origin) && saved.client_id == entry.client_id
        }) {
            return if existing.target == entry.target && existing.same_request(&entry) {
                Ok(false)
            } else {
                Err("This send identity already belongs to a different outgoing request. The original message was preserved.".into())
            };
        }
        if self.entries.len() >= MAX_ENTRIES {
            return Err("Your outgoing message list is full. Retry or remove an earlier message before sending another.".into());
        }
        self.entries.push(entry);
        let size = serde_json::to_vec(self).map(|bytes| bytes.len());
        if !matches!(size, Ok(bytes) if bytes <= MAX_BYTES) {
            self.entries.pop();
            return Err("Your outgoing message storage is full. Retry or remove an earlier message before sending another.".into());
        }
        Ok(true)
    }

    pub fn remove(
        &mut self,
        scope: &str,
        origin: &CommandOrigin,
        target: &Target,
        client_id: &str,
    ) -> bool {
        let previous = self.entries.len();
        self.entries.retain(|entry| {
            !(entry.in_scope(scope, origin)
                && entry.target == *target
                && entry.client_id == client_id)
        });
        previous != self.entries.len()
    }

    /// Only call with a server acknowledgement or authenticated message feed.
    /// Local optimistic rows are never proof of successful delivery.
    pub fn acknowledge(
        &mut self,
        scope: &str,
        origin: &CommandOrigin,
        target: &Target,
        message: &Message,
    ) -> bool {
        let Some(client_id) = message.client_id.as_deref() else {
            return false;
        };
        if message.id.starts_with("local:") || !message_matches(message, &origin.user_id, target) {
            return false;
        }
        self.remove(scope, origin, target, client_id)
    }
}

fn message_matches(message: &Message, own_id: &str, target: &Target) -> bool {
    message.author.id == own_id
        && match target {
            Target::Channel(id) => {
                message.channel_id.as_ref() == Some(id) && message.recipient_id.is_none()
            }
            Target::Direct(id) => {
                message.recipient_id.as_ref() == Some(id) && message.channel_id.is_none()
            }
        }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str) -> Entry {
        Entry {
            scope: "https://relay.example|own-user".into(),
            origin: CommandOrigin {
                url: "https://relay.example".into(),
                server_id: "server".into(),
                user_id: "own-user".into(),
            },
            target: Target::Channel("general".into()),
            binding: SendBinding {
                protection: Default::default(),
                plain: false,
                device_id: "device".into(),
                fingerprint: "fingerprint".into(),
            },
            client_id: id.into(),
            content: "A reply with @OriginalName".into(),
            attachments: vec![Attachment {
                id: "file".into(),
                name: "private.txt".into(),
                mime: "text/plain".into(),
                size: 42,
                url: "/files/file".into(),
            }],
            reply_to_id: Some("original-message".into()),
            mentions: vec!["reply-author".into(), "renamed-user".into()],
            created_at: "2026-10-26T12:00:00Z".into(),
        }
    }

    #[test]
    fn restart_preserves_exact_request_and_protection_binding() {
        let original = entry("stable-send");
        let mut outbox = Outbox::default();
        outbox.insert(original.clone()).unwrap();
        let restored: Outbox =
            serde_json::from_slice(&serde_json::to_vec(&outbox).unwrap()).unwrap();
        let saved = restored
            .get(
                &original.scope,
                &original.origin,
                &original.target,
                &original.client_id,
            )
            .unwrap();
        assert!(saved.same_request(&original));
        let user = User {
            id: original.origin.user_id.clone(),
            ..Default::default()
        };
        let message = saved.message(&user);
        assert_eq!(message.id, "local:stable-send");
        assert_eq!(message.client_id.as_deref(), Some("stable-send"));
        assert_eq!(message.created_at, original.created_at);
        assert_eq!(message.mentions, original.mentions);
        assert!(Entry::from_message(
            original.scope.clone(),
            original.origin.clone(),
            original.target.clone(),
            original.binding.clone(),
            &message
        )
        .unwrap()
        .same_request(&original));
    }

    #[test]
    fn scope_requires_server_and_account_but_allows_verified_host_relocation() {
        let original = entry("stable-send");
        let mut outbox = Outbox::default();
        outbox.insert(original.clone()).unwrap();
        let mut active = original.origin.clone();
        active.url = "https://approved-new-host.example".into();
        assert_eq!(outbox.scoped(&original.scope, &active).count(), 1);
        assert_eq!(outbox.scoped("unrelated-scope", &active).count(), 0);
        active.user_id = "someone-else".into();
        assert_eq!(outbox.scoped(&original.scope, &active).count(), 0);
        active = original.origin.clone();
        active.server_id = "unrelated-server".into();
        assert_eq!(outbox.scoped(&original.scope, &active).count(), 0);
    }

    #[test]
    fn changed_retry_is_rejected_without_overwriting_original() {
        let original = entry("stable-send");
        let mut outbox = Outbox::default();
        assert!(outbox.insert(original.clone()).unwrap());
        assert!(!outbox.insert(original.clone()).unwrap());
        let mut changed = original.clone();
        changed.mentions.clear();
        assert!(outbox.insert(changed).is_err());
        let mut changed = original.clone();
        changed.binding.plain = true;
        assert!(outbox.insert(changed).is_err());
        let mut changed = original.clone();
        changed.target = Target::Channel("different-conversation".into());
        assert!(outbox.insert(changed).is_err());
        assert!(outbox.iter().next().unwrap().same_request(&original));
    }

    #[test]
    fn full_store_rejects_new_request_without_evicting_unsent_messages() {
        let mut outbox = Outbox::default();
        for index in 0..MAX_ENTRIES {
            outbox.insert(entry(&format!("send-{index}"))).unwrap();
        }
        let before = serde_json::to_vec(&outbox).unwrap();
        assert!(outbox.insert(entry("new-send")).is_err());
        assert_eq!(serde_json::to_vec(&outbox).unwrap(), before);
        let mut invalid = entry("oversize");
        invalid.content = "x".repeat(10_001);
        assert!(Outbox::default().insert(invalid).is_err());
    }

    #[test]
    fn unicode_content_uses_the_existing_ten_thousand_byte_limit() {
        let mut message = entry("unicode-send");
        message.content = "🦀".repeat(2_500);
        assert_eq!(message.content.len(), 10_000);
        assert!(Outbox::default().insert(message.clone()).is_ok());
        message.content.push('æ');
        assert!(Outbox::default().insert(message).is_err());
    }

    #[test]
    fn byte_quota_rejection_preserves_every_earlier_request() {
        let mut outbox = Outbox::default();
        for index in 0..MAX_ENTRIES {
            let mut candidate = entry(&format!("large-{index}"));
            candidate.content = "x".repeat(10_000);
            candidate.attachments = (0..8)
                .map(|file| Attachment {
                    id: format!("file-{file}"),
                    name: "n".repeat(1024),
                    mime: "text/plain".into(),
                    size: 42,
                    url: "u".repeat(4096),
                })
                .collect();
            let before = serde_json::to_vec(&outbox).unwrap();
            if outbox.insert(candidate).is_err() {
                assert!(
                    index < MAX_ENTRIES - 1,
                    "The byte limit must apply before the record limit"
                );
                assert_eq!(serde_json::to_vec(&outbox).unwrap(), before);
                return;
            }
        }
        panic!("The byte quota was not enforced");
    }

    #[test]
    fn acknowledgement_requires_remote_own_message_in_exact_conversation() {
        let original = entry("stable-send");
        let mut outbox = Outbox::default();
        outbox.insert(original.clone()).unwrap();
        let user = User {
            id: original.origin.user_id.clone(),
            ..Default::default()
        };
        let mut message = original.message(&user);
        assert!(!outbox.acknowledge(
            &original.scope,
            &original.origin,
            &original.target,
            &message
        ));
        message.id = "server-message".into();
        message.author.id = "someone-else".into();
        assert!(!outbox.acknowledge(
            &original.scope,
            &original.origin,
            &original.target,
            &message
        ));
        message.author = user;
        message.channel_id = Some("other-channel".into());
        assert!(!outbox.acknowledge(
            &original.scope,
            &original.origin,
            &original.target,
            &message
        ));
        message.channel_id = Some("general".into());
        assert!(outbox.acknowledge(
            &original.scope,
            &original.origin,
            &original.target,
            &message
        ));
        assert!(outbox.is_empty());
    }

    #[test]
    fn invalid_or_duplicate_saved_requests_are_rejected_without_silent_truncation() {
        let original = entry("stable-send");
        let duplicate = serde_json::json!({"entries":[original.clone(),original.clone()]});
        assert!(serde_json::from_value::<Outbox>(duplicate).is_err());
        let mut invalid = original;
        invalid.binding.device_id.clear();
        assert!(
            serde_json::from_value::<Outbox>(serde_json::json!({"entries":[invalid]})).is_err()
        );
    }
}
