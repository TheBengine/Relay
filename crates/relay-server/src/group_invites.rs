//! Explicit invitation and acceptance for ordinary Friends groups.
//!
//! The owner of a group invites their own accepted, unblocked friends. An
//! invitation shows the invitee only who invited them and the group's name:
//! no members, history, keys or files until they accept. Acceptance rechecks
//! everything in one transaction, and nobody is ever added before it. The
//! people in a group need not be friends with each other; being in a group
//! gives no friendship, direct message, call or search access to the others.
use super::*;
use relay_protocol::{
    GroupInvitation, SentGroupInvitation, PEOPLE_GROUP_INVITE_DAYS, PEOPLE_GROUP_MAX_MEMBERS,
    PEOPLE_GROUP_PENDING_PER_PERSON,
};

/// One answer for every reason an invitation cannot be used, so an invitee
/// learns nothing about the group's members or their relationships.
fn gone() -> ApiError {
    ApiError::new(StatusCode::GONE, "This invitation is no longer available")
}

/// Invitations older than this are ignored and removed.
fn horizon() -> u64 {
    now().saturating_sub(PEOPLE_GROUP_INVITE_DAYS * 86400)
}

pub(super) fn prune_expired(db: &Connection) -> ApiResult<()> {
    db.execute(
        "DELETE FROM group_invitations WHERE created < ?1",
        [horizon()],
    )?;
    Ok(())
}
/// Shared by both creation and inviting into an existing group, within the same
/// membership transaction. Listing only fifty rows is not an admission limit.
pub(super) fn reserve_invitee(db: &Connection, user: &str) -> ApiResult<()> {
    let count: usize = db.query_row(
        "SELECT COUNT(*) FROM group_invitations WHERE invitee=?1",
        [user],
        |r| r.get(0),
    )?;
    if count >= PEOPLE_GROUP_PENDING_PER_PERSON {
        return Err(bad("This person cannot be invited right now"));
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Invite {
    user_id: String,
}

/// An ordinary group (not a pair's call room) as the Friends service sees it.
fn ordinary(db: &Connection, channel: &str) -> ApiResult<ConversationInfo> {
    conversations::info(db, channel)?
        .filter(|group| group.kind == "group" && !people::pair_room(db, channel))
        .ok_or_else(missing)
}

/// The owner invites one of their accepted, unblocked friends.
pub(super) async fn invite(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(channel): Path<String>,
    Json(input): Json<Invite>,
) -> ApiResult<Json<SentGroupInvitation>> {
    if !state.0.config.people {
        return Err(missing());
    }
    let auth = state.auth(&headers)?;
    {
        let mut db = lock(&state.0.db);
        let tx = db.transaction()?;
        active_user(&tx, &auth.user.id)?;
        let group = ordinary(&tx, &channel)?;
        // Only the current owner, who is still in the group, may invite.
        if group.owner_id != auth.user.id || !group.member_ids.contains(&auth.user.id) {
            return Err(forbidden());
        }
        if input.user_id == auth.user.id || group.member_ids.contains(&input.user_id) {
            return Err(bad("This person is already in the group"));
        }
        // An accepted friend who is not blocked either way; nobody else.
        social::direct_access(&tx, &auth.user.id, &input.user_id)?;
        prune_expired(&tx)?;
        let waiting: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM group_invitations WHERE channel_id=?1 AND invitee=?2)",
            params![channel, input.user_id],
            |r| r.get(0),
        )?;
        if !waiting {
            let pending: usize = tx.query_row(
                "SELECT COUNT(*) FROM group_invitations WHERE channel_id=?1",
                [&channel],
                |r| r.get(0),
            )?;
            // Waiting invitations hold their place, so a group never overfills.
            if group.member_ids.len() + pending >= PEOPLE_GROUP_MAX_MEMBERS {
                return Err(bad(
                    "A group holds at most 8 people, counting invitations that are still waiting",
                ));
            }
            reserve_invitee(&tx, &input.user_id)?;
            tx.execute(
                "INSERT INTO group_invitations(channel_id,invitee,inviter,created) VALUES(?1,?2,?3,?4)",
                params![channel, input.user_id, auth.user.id, now()],
            )?;
        }
        tx.commit()?;
    }
    state.notify(
        "members",
        None,
        None,
        Some(vec![auth.user.id, input.user_id.clone()]),
    );
    Ok(Json(SentGroupInvitation {
        channel_id: channel,
        invitee_id: input.user_id,
    }))
}

/// The owner takes an invitation back.
pub(super) async fn revoke(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((channel, invitee)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    if !state.0.config.people {
        return Err(missing());
    }
    let auth = state.auth(&headers)?;
    {
        let mut db = lock(&state.0.db);
        let tx = db.transaction()?;
        let group = ordinary(&tx, &channel)?;
        if group.owner_id != auth.user.id {
            return Err(forbidden());
        }
        tx.execute(
            "DELETE FROM group_invitations WHERE channel_id=?1 AND invitee=?2",
            params![channel, invitee],
        )?;
        tx.commit()?;
    }
    state.notify("members", None, None, Some(vec![auth.user.id, invitee]));
    Ok(StatusCode::NO_CONTENT)
}

/// The invitee joins, if everything still holds. Every check and the new
/// membership happen in one transaction; anything stale removes the invitation
/// and answers the same "no longer available".
pub(super) async fn accept(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(channel): Path<String>,
) -> ApiResult<Json<ConversationInfo>> {
    if !state.0.config.people {
        return Err(missing());
    }
    let auth = state.auth(&headers)?;
    let joined = {
        let mut db = lock(&state.0.db);
        let tx = db.transaction()?;
        active_user(&tx, &auth.user.id)?;
        let row: Option<(String, u64)> = tx
            .query_row(
                "SELECT inviter,created FROM group_invitations WHERE channel_id=?1 AND invitee=?2",
                params![channel, auth.user.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((inviter, created)) = row else {
            return Err(gone());
        };
        let valid = (|| -> ApiResult<bool> {
            if created < horizon() {
                return Ok(false);
            }
            let Ok(group) = ordinary(&tx, &channel) else {
                return Ok(false);
            };
            // The person who invited is still the owner, still in the group, still
            // active and still this person's accepted, unblocked friend; and this
            // person is not blocked by, or blocking, anyone already inside.
            if group.owner_id != inviter
                || !group.member_ids.contains(&inviter)
                || group.member_ids.contains(&auth.user.id)
                || group.member_ids.len() >= PEOPLE_GROUP_MAX_MEMBERS
                || active_user(&tx, &inviter).is_err()
                || !people::friend(&tx, &inviter, &auth.user.id)
            {
                return Ok(false);
            }
            for member in &group.member_ids {
                if social::blocked(&tx, &auth.user.id, member)? {
                    return Ok(false);
                }
            }
            Ok(true)
        })()?;
        tx.execute(
            "DELETE FROM group_invitations WHERE channel_id=?1 AND invitee=?2",
            params![channel, auth.user.id],
        )?;
        if !valid {
            tx.commit()?;
            return Err(gone());
        }
        tx.execute(
            "INSERT INTO conversation_members VALUES(?1,?2)",
            params![channel, auth.user.id],
        )?;
        let info = conversations::info(&tx, &channel)?.ok_or_else(missing)?;
        tx.commit()?;
        info
    };
    state.notify("channels", None, None, None);
    Ok(Json(joined))
}

/// The invitee says no. Saying it again, or about nothing, changes nothing.
pub(super) async fn decline(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(channel): Path<String>,
) -> ApiResult<StatusCode> {
    if !state.0.config.people {
        return Err(missing());
    }
    let auth = state.auth(&headers)?;
    let inviter: Option<String> = {
        let db = lock(&state.0.db);
        let inviter = db
            .query_row(
                "SELECT inviter FROM group_invitations WHERE channel_id=?1 AND invitee=?2",
                params![channel, auth.user.id],
                |r| r.get(0),
            )
            .optional()?;
        db.execute(
            "DELETE FROM group_invitations WHERE channel_id=?1 AND invitee=?2",
            params![channel, auth.user.id],
        )?;
        inviter
    };
    if let Some(inviter) = inviter {
        state.notify("members", None, None, Some(vec![auth.user.id, inviter]));
    }
    Ok(StatusCode::NO_CONTENT)
}

/// What the invitee may see before accepting: who invited them and the name.
/// An invitation whose inviter is no longer this person's friend is hidden.
pub(super) fn incoming(db: &Connection, user: &str) -> ApiResult<Vec<GroupInvitation>> {
    let mut query = db.prepare(
        "SELECT i.channel_id,c.name,i.inviter FROM group_invitations i JOIN channels c ON c.id=i.channel_id WHERE i.invitee=?1 AND i.created>=?2 ORDER BY i.created,i.channel_id LIMIT 50",
    )?;
    let rows = query
        .query_map(params![user, horizon()], |r| {
            Ok(GroupInvitation {
                channel_id: r.get(0)?,
                name: r.get(1)?,
                inviter_id: r.get(2)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows
        .into_iter()
        .filter(|invitation| people::friend(db, &invitation.inviter_id, user))
        .collect())
}

/// The owner's own waiting invitations, to show as pending and to revoke.
pub(super) fn sent(db: &Connection, owner: &str) -> ApiResult<Vec<SentGroupInvitation>> {
    let mut query = db.prepare(
        "SELECT i.channel_id,i.invitee FROM group_invitations i JOIN conversations c ON c.channel_id=i.channel_id WHERE c.owner_id=?1 AND i.inviter=?1 AND i.created>=?2 ORDER BY i.created,i.channel_id,i.invitee LIMIT 400",
    )?;
    let rows = query
        .query_map(params![owner, horizon()], |r| {
            Ok(SentGroupInvitation {
                channel_id: r.get(0)?,
                invitee_id: r.get(1)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// A block between two people ends every waiting invitation between them, and
/// every invitation of one into a group the other is in. Returns whether any went.
pub(super) fn drop_between(db: &Connection, a: &str, b: &str) -> ApiResult<bool> {
    let removed = db.execute(
        "DELETE FROM group_invitations WHERE (invitee=?1 AND (inviter=?2 OR channel_id IN (SELECT channel_id FROM conversation_members WHERE user_id=?2))) OR (invitee=?2 AND (inviter=?1 OR channel_id IN (SELECT channel_id FROM conversation_members WHERE user_id=?1)))",
        params![a, b],
    )?;
    Ok(removed > 0)
}

/// Ending a friendship ends the invitations between the two (an invitation
/// needs the friendship at the moment of acceptance). Nobody leaves a group.
pub(super) fn drop_pair(db: &Connection, a: &str, b: &str) -> ApiResult<bool> {
    let removed = db.execute(
        "DELETE FROM group_invitations WHERE (inviter=?1 AND invitee=?2) OR (inviter=?2 AND invitee=?1)",
        params![a, b],
    )?;
    Ok(removed > 0)
}

/// Whoever held the group's authority changed: invitations they issued do not
/// pass to the next owner.
pub(super) fn drop_channel(db: &Connection, channel: &str) -> ApiResult<()> {
    db.execute(
        "DELETE FROM group_invitations WHERE channel_id=?1",
        [channel],
    )?;
    Ok(())
}
