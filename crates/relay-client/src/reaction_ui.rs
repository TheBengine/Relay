//! Ephemeral UI intent only. Authenticated message history is never modified.
use super::*;

struct Pending {
    origin: crate::network::CommandOrigin,
    target: Target,
    message_id: String,
    client_id: Option<String>,
    emoji: String,
    active: bool,
    request_id: u64,
}

#[derive(Default)]
pub(super) struct PendingReactions(Vec<Pending>);

impl PendingReactions {
    pub(super) fn can_stage(
        &self,
        origin: &crate::network::CommandOrigin,
        target: &Target,
        message: &Message,
        emoji: &str,
    ) -> bool {
        self.0.iter().filter(|p| &p.origin == origin).count() < 256
            || self
                .0
                .iter()
                .any(|p| p.matches(origin, target, message) && p.emoji == emoji)
    }
    pub(super) fn stage(
        &mut self,
        origin: crate::network::CommandOrigin,
        target: Target,
        message: &Message,
        emoji: &str,
        active: bool,
        request_id: u64,
    ) {
        self.0.retain(|p| {
            p.origin == origin && !(p.matches(&origin, &target, message) && p.emoji == emoji)
        });
        self.0.push(Pending {
            origin,
            target,
            message_id: message.id.clone(),
            client_id: message.client_id.clone(),
            emoji: emoji.into(),
            active,
            request_id,
        });
    }
    pub(super) fn rows(
        &self,
        origin: &crate::network::CommandOrigin,
        target: &Target,
        message: &Message,
    ) -> Vec<relay_protocol::Reaction> {
        let mut rows = message.reactions.clone();
        for pending in self.0.iter().filter(|p| p.matches(origin, target, message)) {
            if let Some(row) = rows.iter_mut().find(|r| r.emoji == pending.emoji) {
                row.user_ids.retain(|id| id != &origin.user_id);
                if pending.active {
                    row.user_ids.push(origin.user_id.clone());
                }
            } else if pending.active {
                rows.push(relay_protocol::Reaction {
                    emoji: pending.emoji.clone(),
                    user_ids: vec![origin.user_id.clone()],
                });
            }
        }
        rows.retain(|r| !r.user_ids.is_empty());
        rows
    }
    pub(super) fn finish(
        &mut self,
        origin: &crate::network::CommandOrigin,
        request_id: u64,
    ) -> Option<String> {
        let index = self
            .0
            .iter()
            .position(|p| &p.origin == origin && p.request_id == request_id)?;
        Some(self.0.remove(index).message_id)
    }
}
impl Pending {
    fn matches(
        &self,
        origin: &crate::network::CommandOrigin,
        target: &Target,
        message: &Message,
    ) -> bool {
        self.origin == *origin
            && self.target == *target
            && self.message_id == message.id
            && self.client_id == message.client_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn immediate_reactions_survive_stale_snapshots_and_rapid_clicks_without_mutating_history() {
        let origin = crate::network::CommandOrigin {
            url: "local".into(),
            server_id: "server".into(),
            user_id: "me".into(),
        };
        let target = Target::Channel("chat".into());
        let mut message = Message {
            id: "message".into(),
            client_id: Some("original".into()),
            reactions: vec![relay_protocol::Reaction {
                emoji: "heart".into(),
                user_ids: vec!["friend".into()],
            }],
            ..Default::default()
        };
        let mut pending = PendingReactions::default();
        pending.stage(origin.clone(), target.clone(), &message, "heart", true, 1);
        assert_eq!(
            pending.rows(&origin, &target, &message)[0].user_ids,
            ["friend", "me"]
        );
        assert_eq!(message.reactions[0].user_ids, ["friend"]);
        pending.stage(origin.clone(), target.clone(), &message, "heart", false, 2);
        assert!(pending.finish(&origin, 1).is_none());
        message.reactions[0].user_ids.push("me".into()); // earlier server acknowledgement
        assert_eq!(
            pending.rows(&origin, &target, &message)[0].user_ids,
            ["friend"]
        );
        assert!(pending.finish(&origin, 2).is_some()); // failure: fall back to authoritative state
        assert_eq!(
            pending.rows(&origin, &target, &message)[0].user_ids,
            ["friend", "me"]
        );
    }
    #[test]
    fn pending_reactions_do_not_leak_across_accounts_communities_or_message_identity() {
        let origin = crate::network::CommandOrigin {
            url: "local".into(),
            server_id: "server".into(),
            user_id: "me".into(),
        };
        let target = Target::Channel("chat".into());
        let message = Message {
            id: "message".into(),
            client_id: Some("original".into()),
            ..Default::default()
        };
        let mut pending = PendingReactions::default();
        pending.stage(origin.clone(), target.clone(), &message, "heart", true, 1);
        let mut other = origin.clone();
        other.user_id = "other".into();
        assert!(pending.rows(&other, &target, &message).is_empty());
        assert!(pending.finish(&other, 1).is_none());
        other = origin.clone();
        other.server_id = "other".into();
        assert!(pending.rows(&other, &target, &message).is_empty());
        assert!(pending
            .rows(&origin, &Target::Channel("other".into()), &message)
            .is_empty());
        let mut replaced = message.clone();
        replaced.client_id = Some("replaced".into());
        assert!(pending.rows(&origin, &target, &replaced).is_empty());
        pending.finish(&origin, 1);
        assert!(pending.rows(&origin, &target, &message).is_empty());
    }
}
