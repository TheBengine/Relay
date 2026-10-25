//! Who this account may open a private conversation with.
//!
//! On a Friends server only an accepted, unblocked friend is a private-message
//! target. People who merely share a group are shown in that group (profile,
//! messages) and nowhere else as someone to message. Every place that offers or
//! restores a private conversation asks here, so they cannot drift apart. A
//! community server keeps its ordinary rule: any other member.
use relay_protocol::Bootstrap;

pub(crate) fn actionable(bootstrap: &Bootstrap, peer: &str) -> bool {
    if peer == bootstrap.user.id {
        return false;
    }
    if !bootstrap.capabilities.iter().any(|c| c == "people-v1") {
        return true;
    }
    bootstrap.social.friends.iter().any(|id| id == peer)
        && !bootstrap.social.blocked.iter().any(|id| id == peer)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn friends_server() -> Bootstrap {
        let mut b = Bootstrap::default();
        b.user.id = "me".into();
        b.capabilities.push("people-v1".into());
        b.social.friends = vec!["friend".into(), "was-friend".into()];
        b.social.blocked = vec!["was-friend".into(), "stranger-blocked".into()];
        b
    }

    #[test]
    fn on_a_friends_server_only_an_unblocked_friend_can_be_messaged() {
        let b = friends_server();
        assert!(actionable(&b, "friend"));
        // Shares a group but is not a friend.
        assert!(!actionable(&b, "co-member"));
        // Blocked, even if a stale friend entry is still listed.
        assert!(!actionable(&b, "was-friend"));
        assert!(!actionable(&b, "stranger-blocked"));
        assert!(!actionable(&b, "me"));
    }

    #[test]
    fn a_community_server_keeps_messaging_every_other_member() {
        let mut b = friends_server();
        b.capabilities.clear();
        assert!(actionable(&b, "co-member"));
        assert!(actionable(&b, "friend"));
        assert!(!actionable(&b, "me"));
    }
}
