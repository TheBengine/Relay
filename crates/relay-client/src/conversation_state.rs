use crate::{chat_state::Draft, network::Target};
use serde::{Deserialize, Serialize};

const MAX_SCOPES: usize = 12;
const MAX_CONVERSATIONS: usize = 128;

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct ConversationMemory {
    #[serde(default)]
    scopes: Vec<ScopeMemory>,
    #[serde(default)]
    conversations: Vec<SavedConversation>,
    #[serde(default)]
    clock: u64,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct ScopeMemory {
    scope: String,
    channel: Option<Target>,
    direct: Option<Target>,
    current: Option<Target>,
    updated: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScrollAnchor {
    pub message_id: String,
    pub offset: f32,
}

#[derive(Clone, Serialize, Deserialize)]
struct SavedConversation {
    scope: String,
    target: Target,
    #[serde(default)]
    draft: Draft,
    #[serde(default)]
    scroll: f32,
    #[serde(default)]
    anchor: Option<ScrollAnchor>,
    #[serde(default)]
    updated: u64,
}

impl ConversationMemory {
    /// User IDs may overlap between independently hosted servers.
    pub fn scope(server: &str, user_id: &str) -> String {
        format!("{}\n{user_id}", server.trim().trim_end_matches('/'))
    }

    pub fn remember(&mut self, scope: &str, target: &Target) {
        self.clock = self.clock.saturating_add(1);
        let index = self
            .scopes
            .iter()
            .position(|entry| entry.scope == scope)
            .unwrap_or_else(|| {
                self.scopes.push(ScopeMemory {
                    scope: scope.into(),
                    ..Default::default()
                });
                self.scopes.len() - 1
            });
        let entry = &mut self.scopes[index];
        entry.current = Some(target.clone());
        match target {
            Target::Channel(_) => entry.channel = Some(target.clone()),
            Target::Direct(_) => entry.direct = Some(target.clone()),
        }
        entry.updated = self.clock;
        let index = self.conversation_index(scope, target);
        self.conversations[index].updated = self.clock;
        self.bound();
    }

    pub fn last(&self, scope: &str, direct: Option<bool>) -> Option<Target> {
        self.scopes
            .iter()
            .find(|entry| entry.scope == scope)
            .and_then(|entry| match direct {
                Some(true) => entry.direct.clone(),
                Some(false) => entry.channel.clone(),
                None => entry.current.clone(),
            })
    }

    pub fn recent_directs(&self, scope: &str) -> Vec<Target> {
        let mut entries: Vec<_> = self
            .conversations
            .iter()
            .filter(|entry| entry.scope == scope && matches!(entry.target, Target::Direct(_)))
            .collect();
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.updated));
        entries
            .into_iter()
            .map(|entry| entry.target.clone())
            .collect()
    }

    pub fn drafts(&self, scope: &str) -> Vec<(Target, Draft)> {
        self.conversations
            .iter()
            .filter(|entry| entry.scope == scope)
            .map(|entry| (entry.target.clone(), entry.draft.clone()))
            .collect()
    }

    pub fn save_drafts(&mut self, scope: &str, drafts: Vec<(Target, Draft)>) {
        for entry in self
            .conversations
            .iter_mut()
            .filter(|entry| entry.scope == scope)
        {
            entry.draft = Draft::default();
        }
        for (target, draft) in drafts {
            let index = self.conversation_index(scope, &target);
            self.conversations[index].draft = draft;
        }
        self.bound();
    }

    pub fn clear_drafts(&mut self) {
        for entry in &mut self.conversations {
            entry.draft = Draft::default();
        }
    }
    pub fn retain_targets(&mut self, scope: &str, allowed: impl Fn(&Target) -> bool) {
        self.conversations
            .retain(|entry| entry.scope != scope || allowed(&entry.target));
        if let Some(entry) = self.scopes.iter_mut().find(|entry| entry.scope == scope) {
            if entry
                .channel
                .as_ref()
                .is_some_and(|target| !allowed(target))
            {
                entry.channel = None;
            }
            if entry.direct.as_ref().is_some_and(|target| !allowed(target)) {
                entry.direct = None;
            }
            if entry
                .current
                .as_ref()
                .is_some_and(|target| !allowed(target))
            {
                entry.current = None;
            }
        }
    }

    pub fn scroll(&self, scope: &str, target: &Target) -> Option<f32> {
        self.conversations
            .iter()
            .find(|entry| entry.scope == scope && &entry.target == target)
            .map(|entry| entry.scroll)
    }

    pub fn anchor(&self, scope: &str, target: &Target) -> Option<ScrollAnchor> {
        self.conversations
            .iter()
            .find(|entry| entry.scope == scope && &entry.target == target)
            .and_then(|entry| entry.anchor.clone())
    }
    pub fn set_anchor(&mut self, scope: &str, target: &Target, mut anchor: ScrollAnchor) {
        if anchor.message_id.is_empty() || anchor.message_id.len() > 128 {
            return;
        }
        anchor.offset = if anchor.offset.is_finite() {
            anchor.offset.clamp(0.0, 10000.0)
        } else {
            0.0
        };
        let index = self.conversation_index(scope, target);
        self.conversations[index].anchor = Some(anchor);
        self.bound();
    }

    pub fn set_scroll(&mut self, scope: &str, target: &Target, scroll: f32) {
        let index = self.conversation_index(scope, target);
        self.conversations[index].scroll = if scroll.is_finite() {
            scroll.clamp(0.0, 10_000_000.0)
        } else {
            0.0
        };
        self.bound();
    }

    fn conversation_index(&mut self, scope: &str, target: &Target) -> usize {
        self.conversations
            .iter()
            .position(|entry| entry.scope == scope && &entry.target == target)
            .unwrap_or_else(|| {
                self.conversations.push(SavedConversation {
                    scope: scope.into(),
                    target: target.clone(),
                    draft: Draft::default(),
                    scroll: 0.0,
                    anchor: None,
                    updated: self.clock,
                });
                self.conversations.len() - 1
            })
    }

    fn bound(&mut self) {
        self.scopes
            .sort_by_key(|entry| std::cmp::Reverse(entry.updated));
        self.scopes.truncate(MAX_SCOPES);
        self.conversations
            .retain(|entry| self.scopes.iter().any(|saved| saved.scope == entry.scope));
        self.conversations
            .sort_by_key(|entry| std::cmp::Reverse(entry.updated));
        self.conversations.truncate(MAX_CONVERSATIONS);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat_state::ReplyDraft;

    #[test]
    fn server_and_user_boundaries_keep_overlapping_conversation_ids_private() {
        let mut memory = ConversationMemory::default();
        let target = Target::Direct("same-user-id".into());
        let first = ConversationMemory::scope("https://one.example/", "account");
        let second = ConversationMemory::scope("https://two.example", "account");
        let other_user = ConversationMemory::scope("https://one.example", "another-account");
        memory.remember(&first, &target);
        memory.set_scroll(&first, &target, 385.0);
        memory.save_drafts(
            &first,
            vec![(
                target.clone(),
                Draft {
                    content: "Private note".into(),
                    reply: Some(ReplyDraft {
                        message_id: "reply".into(),
                        author: "Friend".into(),
                        excerpt: "Earlier message".into(),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            )],
        );
        assert_eq!(memory.last(&first, Some(true)), Some(target.clone()));
        assert!(memory.last(&second, None).is_none());
        assert!(memory.drafts(&other_user).is_empty());
        assert_eq!(memory.scroll(&first, &target), Some(385.0));
        assert!(memory.scroll(&second, &target).is_none());
        let round_trip: ConversationMemory =
            serde_json::from_slice(&serde_json::to_vec(&memory).unwrap()).unwrap();
        assert_eq!(round_trip.drafts(&first)[0].1.content, "Private note");
        assert_eq!(
            round_trip.drafts(&first)[0]
                .1
                .reply
                .as_ref()
                .unwrap()
                .message_id,
            "reply"
        );
    }

    #[test]
    fn permission_revocation_removes_private_drafts_and_navigation_in_only_that_community() {
        let mut memory = ConversationMemory::default();
        let target = Target::Channel("private".into());
        for scope in ["one", "two"] {
            memory.remember(scope, &target);
            memory.save_drafts(
                scope,
                vec![(
                    target.clone(),
                    Draft {
                        content: "private draft".into(),
                        ..Default::default()
                    },
                )],
            );
        }
        memory.retain_targets("one", |_| false);
        assert!(memory.last("one", None).is_none());
        assert!(memory.drafts("one").is_empty());
        assert_eq!(memory.last("two", None), Some(target));
        assert_eq!(memory.drafts("two")[0].1.content, "private draft");
    }

    #[test]
    fn older_message_anchor_round_trips_and_is_scoped_and_bounded() {
        let mut memory = ConversationMemory::default();
        let target = Target::Channel("general".into());
        memory.remember("server-a", &target);
        memory.set_anchor(
            "server-a",
            &target,
            ScrollAnchor {
                message_id: "older-message".into(),
                offset: 18.5,
            },
        );
        let memory: ConversationMemory =
            serde_json::from_slice(&serde_json::to_vec(&memory).unwrap()).unwrap();
        assert_eq!(
            memory.anchor("server-a", &target),
            Some(ScrollAnchor {
                message_id: "older-message".into(),
                offset: 18.5
            })
        );
        assert!(memory.anchor("server-b", &target).is_none());
        let mut memory = memory;
        memory.set_anchor(
            "server-a",
            &target,
            ScrollAnchor {
                message_id: "valid".into(),
                offset: f32::NAN,
            },
        );
        assert_eq!(memory.anchor("server-a", &target).unwrap().offset, 0.0);
    }

    #[test]
    fn last_channel_and_recent_directs_follow_actual_navigation() {
        let mut memory = ConversationMemory::default();
        let scope = "server\nuser";
        let channel = Target::Channel("general".into());
        let a = Target::Direct("a".into());
        let b = Target::Direct("b".into());
        memory.remember(scope, &channel);
        memory.remember(scope, &a);
        memory.remember(scope, &b);
        memory.remember(scope, &a);
        assert_eq!(memory.last(scope, Some(false)), Some(channel));
        assert_eq!(memory.last(scope, None), Some(a.clone()));
        assert_eq!(memory.recent_directs(scope), [a, b]);
    }

    #[test]
    fn cache_is_bounded_and_clearing_drafts_preserves_navigation() {
        let mut memory = ConversationMemory::default();
        for id in 0..180 {
            let target = Target::Channel(id.to_string());
            memory.remember("scope", &target);
            memory.save_drafts(
                "scope",
                vec![(
                    target.clone(),
                    Draft {
                        content: "note".into(),
                        ..Default::default()
                    },
                )],
            );
        }
        assert_eq!(memory.conversations.len(), MAX_CONVERSATIONS);
        memory.clear_drafts();
        assert!(memory
            .drafts("scope")
            .iter()
            .all(|(_, draft)| draft.content.is_empty()));
        assert_eq!(
            memory.last("scope", None),
            Some(Target::Channel("179".into()))
        );
    }
}
