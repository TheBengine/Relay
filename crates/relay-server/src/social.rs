//! Relationships are private to their owner. Reports contain only explicitly shared evidence.
use super::*;
use serde_json::Value;
#[cfg(test)]
mod tests;

pub(super) fn blocked(db: &Connection, a: &str, b: &str) -> ApiResult<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM user_blocks WHERE (owner=?1 AND peer=?2) OR (owner=?2 AND peer=?1))",params![a,b],|r|r.get(0))?)
}
pub(super) fn direct_access(db: &Connection, a: &str, b: &str) -> ApiResult<()> {
    relationship_access(db, a, b)?;
    if people::enabled(db) && !people::friend(db, a, b) {
        return Err(forbidden());
    }
    Ok(())
}
fn relationship_access(db: &Connection, a: &str, b: &str) -> ApiResult<()> {
    active_user(db, a)?;
    active_user(db, b)?;
    if a == b || blocked(db, a, b)? {
        return Err(forbidden());
    }
    Ok(())
}
pub(super) fn heartbeat(db: &Connection, user: &str) -> ApiResult<()> {
    db.execute("INSERT INTO user_presence(user_id,last_seen) VALUES(?1,?2) ON CONFLICT(user_id) DO UPDATE SET last_seen=excluded.last_seen WHERE user_presence.last_seen<?2-30",params![user,now()])?;
    Ok(())
}
pub(super) fn relationships(db: &Connection, user: &str) -> ApiResult<SocialState> {
    let mut result = SocialState {
        server_time: now(),
        ..Default::default()
    };
    let mut query = db.prepare(
        "SELECT sender,recipient,accepted FROM friendships WHERE sender=?1 OR recipient=?1",
    )?;
    for row in query.query_map([user], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, bool>(2)?,
        ))
    })? {
        let (sender, recipient, accepted) = row?;
        let peer = if sender == user { &recipient } else { &sender };
        if active_user(db, peer).is_err() || blocked(db, user, peer)? {
            continue;
        }
        if accepted {
            result.friends.push(peer.clone());
        } else if sender == user {
            result.outgoing.push(peer.clone());
        } else {
            result.incoming.push(peer.clone());
        }
    }
    let mut query = db.prepare("SELECT peer FROM user_blocks WHERE owner=?1 ORDER BY peer")?;
    result.blocked = query
        .query_map([user], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    result.friends.sort();
    result.incoming.sort();
    result.outgoing.sort();
    Ok(result)
}
pub(super) fn presence(db: &Connection, viewer: &str) -> ApiResult<Vec<UserPresence>> {
    let mut query=db.prepare("SELECT p.user_id,p.status,p.text,p.expires_at,p.last_seen FROM user_presence p JOIN membership m ON m.user_id=p.user_id WHERE m.status='active' ORDER BY p.user_id")?;
    let rows = query
        .query_map([], |r| {
            Ok((
                UserPresence {
                    user_id: r.get(0)?,
                    status: r.get(1)?,
                    text: r.get(2)?,
                    expires_at: r.get(3)?,
                },
                r.get::<_, u64>(4)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows
        .into_iter()
        .filter(|(p, _)| {
            !people::enabled(db) || p.user_id == viewer || people::friend(db, viewer, &p.user_id)
        })
        .map(|(mut p, last)| {
            if p.expires_at.is_some_and(|expiry| expiry <= now()) {
                p.text.clear();
                p.expires_at = None;
            }
            if p.user_id != viewer
                && (p.status == "invisible"
                    || last + 90 < now()
                    || blocked(db, viewer, &p.user_id).unwrap_or(true))
            {
                p.status = "offline".into();
                p.text.clear();
                p.expires_at = None;
            }
            p
        })
        .collect())
}
pub(super) async fn get_social(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<SocialState>> {
    let auth = state.auth(&headers)?;
    Ok(Json(private_calls::social(
        &state,
        &lock(&state.0.db),
        &auth.user.id,
    )?))
}
pub(super) async fn friend_request(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(peer): Path<String>,
) -> ApiResult<Json<SocialState>> {
    let auth = state.auth(&headers)?;
    let result = {
        let mut db = lock(&state.0.db);
        let tx = db.transaction()?;
        relationship_access(&tx, &auth.user.id, &peer)?;
        let reverse: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM friendships WHERE sender=?1 AND recipient=?2)",
            params![peer, auth.user.id],
            |r| r.get(0),
        )?;
        let existing = reverse
            || tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM friendships WHERE sender=?1 AND recipient=?2)",
                params![auth.user.id, peer],
                |r| r.get::<_, bool>(0),
            )?;
        // Accept/retry never grows a list. Only a genuinely new relationship
        // needs space, and it needs space at both ends of the same transaction.
        if !existing {
            for (user, full) in [
                (
                    &auth.user.id,
                    "You have reached the 1,000 relationship limit",
                ),
                (&peer, "This person's friend list is full"),
            ] {
                let count: u32 = tx.query_row(
                    "SELECT COUNT(*) FROM friendships WHERE sender=?1 OR recipient=?1",
                    [user],
                    |r| r.get(0),
                )?;
                if count >= 1000 {
                    return Err(bad(full));
                }
            }
        }
        if reverse {
            tx.execute(
                "UPDATE friendships SET accepted=1 WHERE sender=?1 AND recipient=?2",
                params![peer, auth.user.id],
            )?;
        } else {
            tx.execute(
                "INSERT OR IGNORE INTO friendships(sender,recipient) VALUES(?1,?2)",
                params![auth.user.id, peer],
            )?;
        }
        let result = relationships(&tx, &auth.user.id)?;
        tx.commit()?;
        result
    };
    state.notify("members", None, None, Some(vec![auth.user.id, peer]));
    Ok(Json(result))
}
pub(super) async fn remove_friend(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(peer): Path<String>,
) -> ApiResult<Json<SocialState>> {
    let auth = state.auth(&headers)?;
    let db = lock(&state.0.db);
    db.execute("DELETE FROM friendships WHERE (sender=?1 AND recipient=?2) OR (sender=?2 AND recipient=?1)",params![auth.user.id,peer])?;
    let result = relationships(&db, &auth.user.id)?;
    drop(db);
    state.enforce_voice_permissions()?;
    private_calls::end_matching(&state, &auth.user.id, Some(&peer), None)?;
    state.notify("members", None, None, Some(vec![auth.user.id, peer]));
    Ok(Json(result))
}
pub(super) async fn block_user(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(peer): Path<String>,
) -> ApiResult<Json<SocialState>> {
    let auth = state.auth(&headers)?;
    let result = {
        let mut db = lock(&state.0.db);
        let tx = db.transaction()?;
        active_user(&tx, &peer)?;
        if peer == auth.user.id {
            return Err(bad("You cannot block yourself"));
        }
        let count: u32 = tx.query_row(
            "SELECT COUNT(*) FROM user_blocks WHERE owner=?1",
            [&auth.user.id],
            |r| r.get(0),
        )?;
        if count >= 1000 {
            return Err(bad("You have reached the 1,000 block limit"));
        }
        tx.execute(
            "INSERT OR IGNORE INTO user_blocks VALUES(?1,?2)",
            params![auth.user.id, peer],
        )?;
        tx.execute("DELETE FROM friendships WHERE (sender=?1 AND recipient=?2) OR (sender=?2 AND recipient=?1)",params![auth.user.id,peer])?;
        let result = relationships(&tx, &auth.user.id)?;
        tx.commit()?;
        result
    };
    state.enforce_voice_permissions()?;
    private_calls::end_matching(&state, &auth.user.id, Some(&peer), None)?;
    state.notify("members", None, None, Some(vec![auth.user.id, peer]));
    Ok(Json(result))
}
pub(super) async fn unblock_user(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(peer): Path<String>,
) -> ApiResult<Json<SocialState>> {
    let auth = state.auth(&headers)?;
    let db = lock(&state.0.db);
    db.execute(
        "DELETE FROM user_blocks WHERE owner=?1 AND peer=?2",
        params![auth.user.id, peer],
    )?;
    let result = relationships(&db, &auth.user.id)?;
    drop(db);
    state.notify("members", None, None, Some(vec![auth.user.id, peer]));
    Ok(Json(result))
}
pub(super) async fn set_presence(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<UserPresence>,
) -> ApiResult<Json<UserPresence>> {
    let auth = state.auth(&headers)?;
    if !matches!(
        input.status.as_str(),
        "online" | "away" | "dnd" | "invisible"
    ) || input.text.chars().count() > 128
        || input.text.chars().any(char::is_control)
        || input
            .expires_at
            .is_some_and(|expiry| expiry <= now() || expiry > now() + 30 * 86400)
    {
        return Err(bad(
            "Choose a supported status, up to 128 characters, and an expiry within 30 days",
        ));
    }
    let result = UserPresence {
        user_id: auth.user.id.clone(),
        ..input
    };
    let audience = {
        let db = lock(&state.0.db);
        db.execute("INSERT INTO user_presence VALUES(?1,?2,?3,?4,?5) ON CONFLICT(user_id) DO UPDATE SET status=excluded.status,text=excluded.text,expires_at=excluded.expires_at,last_seen=excluded.last_seen",params![result.user_id,result.status,result.text,result.expires_at,now()])?;
        if state.0.config.people {
            // Pending requests may see a profile, but not presence-update timing.
            let mut users = relationships(&db, &auth.user.id)?.friends;
            users.push(auth.user.id.clone());
            Some(users)
        } else {
            None
        }
    };
    state.notify("members", None, Some(auth.user.id), audience);
    Ok(Json(result))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ReportInput {
    reported_id: String,
    reason: String,
    #[serde(default)]
    evidence: String,
    share_evidence: bool,
}
pub(super) async fn report(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<ReportInput>,
) -> ApiResult<Json<Value>> {
    let auth = state.auth(&headers)?;
    if input.reported_id == auth.user.id
        || input.reason.trim().is_empty()
        || input.reason.chars().count() > 1000
        || input.evidence.len() > 16384
        || (!input.share_evidence && !input.evidence.is_empty())
    {
        return Err(bad(
            "Supply a reason and explicitly consent before sharing message evidence",
        ));
    }
    let db = lock(&state.0.db);
    active_user(&db, &input.reported_id)?;
    let count: u32 = db.query_row(
        "SELECT COUNT(*) FROM user_reports WHERE reporter_id=?1 AND created_at>?2",
        params![
            auth.user.id,
            chrono::Utc::now()
                .checked_sub_signed(chrono::Duration::days(1))
                .unwrap()
                .to_rfc3339()
        ],
        |r| r.get(0),
    )?;
    if count >= 20 {
        return Err(bad("You can submit up to 20 reports per day"));
    }
    let report_id = id();
    db.execute("INSERT INTO user_reports(id,reporter_id,reported_id,reason,evidence,created_at) VALUES(?1,?2,?3,?4,?5,?6)",params![report_id,auth.user.id,input.reported_id,input.reason.trim(),input.evidence,chrono::Utc::now().to_rfc3339()])?;
    Ok(Json(serde_json::json!({"id":report_id})))
}
pub(super) async fn reports(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Vec<UserReport>>> {
    let auth = state.auth(&headers)?;
    let db = lock(&state.0.db);
    roles::require(&db, &auth.user.id, PERMISSION_MEMBERS)?;
    let mut q=db.prepare("SELECT id,reporter_id,reported_id,reason,evidence,created_at,resolved FROM user_reports ORDER BY created_at DESC LIMIT 200")?;
    let result = q
        .query_map([], |r| {
            Ok(UserReport {
                id: r.get(0)?,
                reporter_id: r.get(1)?,
                reported_id: r.get(2)?,
                reason: r.get(3)?,
                evidence: r.get(4)?,
                created_at: r.get(5)?,
                resolved: r.get(6)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    Ok(Json(result))
}
pub(super) async fn resolve_report(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let auth = state.auth(&headers)?;
    let mut db = lock(&state.0.db);
    let tx = db.transaction()?;
    roles::require(&tx, &auth.user.id, PERMISSION_MEMBERS)?;
    if tx.execute("UPDATE user_reports SET resolved=1 WHERE id=?1", [&id])? == 0 {
        return Err(missing());
    }
    roles::audit(&tx, &auth.user.id, "report.resolved", &id)?;
    tx.commit()?;
    Ok(Json(serde_json::json!({"ok":true})))
}
