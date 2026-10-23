use crate::network::Target;
use relay_protocol::Attachment;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Draft {
    pub content: String,
    pub attachments: Vec<Attachment>,
    #[serde(default)]
    pub reply: Option<ReplyDraft>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReplyDraft {
    pub message_id: String,
    pub author: String,
    pub excerpt: String,
    #[serde(default)]
    pub author_id: Option<String>,
    #[serde(default)]
    pub notify_author: bool,
}

impl Draft {
    fn is_empty(&self) -> bool {
        self.content.is_empty() && self.attachments.is_empty() && self.reply.is_none()
    }
}

#[derive(Default)]
pub struct DraftStore {
    drafts: HashMap<Target, Draft>,
}

/// Session-only snapshots remain available when restart persistence is disabled.
#[derive(Default)]
pub struct ScopedDrafts {
    scopes: Vec<ScopeDraftSnapshot>,
}

struct ScopeDraftSnapshot {
    scope: String,
    drafts: Vec<(Target, Draft)>,
}

impl ScopedDrafts {
    pub fn store(&mut self, scope: &str, mut drafts: Vec<(Target, Draft)>) {
        self.scopes.retain(|saved| saved.scope != scope);
        if drafts.len() > 128 {
            drafts.drain(..drafts.len() - 128);
        }
        self.scopes.push(ScopeDraftSnapshot {
            scope: scope.into(),
            drafts,
        });
        if self.scopes.len() > 12 {
            self.scopes.remove(0);
        }
    }
    pub fn load(&self, scope: &str) -> Option<Vec<(Target, Draft)>> {
        self.scopes
            .iter()
            .find(|saved| saved.scope == scope)
            .map(|saved| saved.drafts.clone())
    }
    pub fn retain_targets(&mut self, scope: &str, allowed: impl Fn(&Target) -> bool) {
        if let Some(saved) = self.scopes.iter_mut().find(|saved| saved.scope == scope) {
            saved.drafts.retain(|(target, _)| allowed(target));
        }
    }
    pub fn clear(&mut self) {
        self.scopes.clear();
    }
}

impl DraftStore {
    pub fn snapshot(&self) -> Vec<(Target, Draft)> {
        self.drafts
            .iter()
            .map(|(target, draft)| (target.clone(), draft.clone()))
            .collect()
    }

    pub fn restore(&mut self, entries: impl IntoIterator<Item = (Target, Draft)>) {
        self.drafts.clear();
        for (target, draft) in entries {
            self.store(target, draft);
        }
    }
    pub fn retain(&mut self, allowed: impl Fn(&Target) -> bool) {
        self.drafts.retain(|target, _| allowed(target));
    }
    /// Save the current editor before changing conversations or applying an update.
    pub fn store(&mut self, target: Target, draft: Draft) {
        if draft.is_empty() {
            self.drafts.remove(&target);
        } else {
            self.drafts.insert(target, draft);
        }
    }

    /// Restore a conversation's draft, transferring ownership back to the editor.
    pub fn take(&mut self, target: &Target) -> Draft {
        self.drafts.remove(target).unwrap_or_default()
    }

    /// An acknowledgement removes only the submitted text and attachment IDs.
    /// Text typed while the request was pending must survive the acknowledgement.
    #[cfg(test)]
    pub fn apply_sent(&mut self, target: &Target, content: &str, attachment_ids: &[String]) {
        self.apply_sent_rich(target, content, attachment_ids, None);
    }

    #[cfg(test)]
    pub fn apply_sent_rich(
        &mut self,
        target: &Target,
        content: &str,
        attachment_ids: &[String],
        reply_to_id: Option<&str>,
    ) {
        let Some(draft) = self.drafts.get_mut(target) else {
            return;
        };
        if draft.content.trim() == content.trim() {
            draft.content.clear();
        }
        draft
            .attachments
            .retain(|attachment| !attachment_ids.iter().any(|id| id == &attachment.id));
        if draft.reply.as_ref().map(|reply| reply.message_id.as_str()) == reply_to_id {
            draft.reply = None;
        }
        if draft.is_empty() {
            self.drafts.remove(target);
        }
    }

    /// Upload completion belongs to the conversation in which it was requested.
    pub fn apply_uploaded(&mut self, target: &Target, attachment: Attachment) {
        let draft = self.drafts.entry(target.clone()).or_default();
        if !draft
            .attachments
            .iter()
            .any(|existing| existing.id == attachment.id)
        {
            draft.attachments.push(attachment);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attachment(id: &str) -> Attachment {
        Attachment {
            id: id.into(),
            name: format!("{id}.txt"),
            mime: "text/plain".into(),
            size: 12,
            url: format!("/api/uploads/{id}"),
        }
    }

    fn draft(content: &str, attachment_ids: &[&str]) -> Draft {
        Draft {
            content: content.into(),
            attachments: attachment_ids.iter().map(|id| attachment(id)).collect(),
            reply: None,
        }
    }

    fn ids(draft: &Draft) -> Vec<&str> {
        draft
            .attachments
            .iter()
            .map(|attachment| attachment.id.as_str())
            .collect()
    }

    #[test]
    fn switching_conversations_restores_each_draft_without_copying_it() {
        let channel = Target::Channel("shared-id".into());
        let direct = Target::Direct("shared-id".into());
        let mut store = DraftStore::default();
        store.store(channel.clone(), draft("Ready for tonight?", &["schedule"]));
        store.store(direct.clone(), draft("Private message", &[]));

        let restored = store.take(&channel);
        assert_eq!(restored.content, "Ready for tonight?");
        assert_eq!(ids(&restored), ["schedule"]);
        assert!(store.take(&channel).is_empty());
        assert_eq!(store.take(&direct).content, "Private message");

        store.store(channel.clone(), restored);
        assert_eq!(store.take(&channel).content, "Ready for tonight?");
    }

    #[test]
    fn late_send_acknowledgement_preserves_later_text_and_attachments() {
        let original = Target::Channel("general".into());
        let current = Target::Direct("friend".into());
        let mut store = DraftStore::default();
        store.store(
            original.clone(),
            draft("Another thing…", &["submitted", "new"]),
        );
        store.store(
            current.clone(),
            draft("Different conversation", &["private"]),
        );

        store.apply_sent(&original, "First message", &["submitted".into()]);

        let restored = store.take(&original);
        assert_eq!(restored.content, "Another thing…");
        assert_eq!(ids(&restored), ["new"]);
        let untouched = store.take(&current);
        assert_eq!(untouched.content, "Different conversation");
        assert_eq!(ids(&untouched), ["private"]);
    }

    #[test]
    fn acknowledged_trimmed_text_is_cleared_without_removing_new_attachments() {
        let target = Target::Channel("general".into());
        let mut store = DraftStore::default();
        store.store(
            target.clone(),
            draft(" \n Sent message \t", &["submitted", "new"]),
        );

        store.apply_sent(&target, "Sent message", &["submitted".into()]);

        let restored = store.take(&target);
        assert!(restored.content.is_empty());
        assert_eq!(ids(&restored), ["new"]);
    }

    #[test]
    fn completed_upload_stays_with_original_conversation_after_switching() {
        let original = Target::Channel("clips".into());
        let current = Target::Direct("friend".into());
        let mut store = DraftStore::default();
        store.store(current.clone(), draft("Keep this draft", &[]));

        store.apply_uploaded(&original, attachment("clip"));
        store.apply_uploaded(&original, attachment("clip"));

        let original_draft = store.take(&original);
        assert!(original_draft.content.is_empty());
        assert_eq!(ids(&original_draft), ["clip"]);
        let current_draft = store.take(&current);
        assert_eq!(current_draft.content, "Keep this draft");
        assert!(current_draft.attachments.is_empty());
    }

    #[test]
    fn acknowledgements_for_an_empty_draft_do_not_recreate_it() {
        let target = Target::Channel("general".into());
        let mut store = DraftStore::default();
        store.store(target.clone(), draft("Message", &["file"]));
        store.apply_sent(&target, "Message", &["file".into()]);
        assert!(store.drafts.is_empty());

        store.apply_sent(&target, "Message", &["file".into()]);
        assert!(store.drafts.is_empty());
        store.store(target.clone(), Draft::default());
        assert!(store.take(&target).is_empty());
    }

    #[test]
    fn rich_ack_preserves_a_reply_changed_while_sending() {
        let target = Target::Direct("friend".into());
        let mut store = DraftStore::default();
        store.store(
            target.clone(),
            Draft {
                content: "Next reply".into(),
                reply: Some(ReplyDraft {
                    message_id: "new-parent".into(),
                    author: "Friend".into(),
                    excerpt: "Second question".into(),
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        store.apply_sent_rich(&target, "Earlier reply", &[], Some("old-parent"));
        let restored = store.take(&target);
        assert_eq!(restored.content, "Next reply");
        assert_eq!(restored.reply.unwrap().message_id, "new-parent");
    }

    #[test]
    fn acknowledged_reply_is_removed_and_drafts_round_trip() {
        let target = Target::Channel("general".into());
        let mut store = DraftStore::default();
        store.store(
            target.clone(),
            Draft {
                content: "answer".into(),
                reply: Some(ReplyDraft {
                    message_id: "question".into(),
                    author: "Friend".into(),
                    excerpt: "Question".into(),
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        let encoded = serde_json::to_vec(&store.snapshot()).unwrap();
        let mut restored = DraftStore::default();
        restored.restore(serde_json::from_slice::<Vec<(Target, Draft)>>(&encoded).unwrap());
        restored.apply_sent_rich(&target, "answer", &[], Some("question"));
        assert!(restored.take(&target).is_empty());
    }
}
