//! Coarse presence inside an ordinary Friends group.
//!
//! A person in an accepted group sees the other current members' state there
//! (online, idle, do not disturb, offline) and nothing more. It is a separate
//! list from the friends-only `Bootstrap.presence`: it never carries a status
//! text, expiry, time, device, game activity or lobby, and sharing a group gives
//! no friendship, direct message, call or global presence. The groups always
//! come from the viewer's own current conversation list, never from the client.
use super::*;
use relay_protocol::GroupPresence;

/// A person is in at most this many groups for the purpose of this list.
const MAX_GROUPS: usize = 64;
/// Same stale-heartbeat window as the friends presence list.
const STALE_SECONDS: u64 = 90;

/// The ordinary text groups `user` can read right now: not a pair's call room,
/// and not one they were invited to but have not accepted.
pub(super) fn groups(db: &Connection, user: &str) -> ApiResult<Vec<ConversationInfo>> {
    Ok(conversations::list(db, user)?
        .into_iter()
        .filter(|group| {
            group.kind == "group"
                && !group.archived
                && group.member_ids.len() <= relay_protocol::PEOPLE_GROUP_MAX_MEMBERS
                && group.member_ids.iter().any(|member| member == user)
                && !people::pair_room(db, &group.channel_id)
                && people::group_access(db, group, user)
                && channel_access(db, &group.channel_id, user, READ).is_ok()
        })
        .take(MAX_GROUPS)
        .collect())
}

/// The state shown to others, from what is stored. A hidden person, a stale
/// heartbeat and anyone without a record are all simply offline.
pub(super) fn coarse(status: &str, last_seen: u64, at: u64) -> &'static str {
    if last_seen + STALE_SECONDS < at {
        return "offline";
    }
    match status {
        "online" => "online",
        "away" => "idle",
        "dnd" => "dnd",
        _ => "offline",
    }
}

/// The other active members of each of `viewer`'s groups, one row each.
pub(super) fn rows(db: &Connection, viewer: &str) -> ApiResult<Vec<GroupPresence>> {
    let at = now();
    let mut result = vec![];
    for group in groups(db, viewer)? {
        for member in group.member_ids.iter().filter(|member| *member != viewer) {
            if active_user(db, member).is_err() || social::blocked(db, viewer, member)? {
                continue;
            }
            let stored: Option<(String, u64)> = db
                .query_row(
                    "SELECT status,last_seen FROM user_presence WHERE user_id=?1",
                    [member],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let status = stored
                .map(|(status, last_seen)| coarse(&status, last_seen, at))
                .unwrap_or("offline");
            result.push(GroupPresence {
                channel_id: group.channel_id.clone(),
                user_id: member.clone(),
                status: status.into(),
            });
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_four_coarse_states_exist_and_hidden_or_stale_is_offline() {
        let at = 1_000;
        assert_eq!(coarse("online", at, at), "online");
        assert_eq!(coarse("away", at - 10, at), "idle");
        assert_eq!(coarse("dnd", at - 90, at), "dnd");
        // Invisible is offline, and so is anything unknown.
        assert_eq!(coarse("invisible", at, at), "offline");
        assert_eq!(coarse("lurking", at, at), "offline");
        // A heartbeat older than the window is offline whatever was chosen.
        assert_eq!(coarse("online", at - 91, at), "offline");
        assert_eq!(coarse("dnd", 0, at), "offline");
    }
}
