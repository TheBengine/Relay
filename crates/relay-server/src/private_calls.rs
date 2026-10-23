//! Ringing is volatile; accepted calls and ended tombstones retain their original
//! IDs across restart. Media still requires ordinary ACL/protection admission.
use super::*;

const RING_SECONDS: u64 = 45;
const RESULT_SECONDS: u64 = 30;
const MAX_CALLS: usize = 1024;

fn friends(db: &Connection, a: &str, b: &str) -> ApiResult<()> {
    social::direct_access(db, a, b)?;
    let accepted: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM friendships WHERE accepted=1 AND ((sender=?1 AND recipient=?2) OR (sender=?2 AND recipient=?1)))",
        params![a, b], |r| r.get(0),
    )?;
    if !accepted {
        return Err(bad("Accept the friend request before calling"));
    }
    Ok(())
}

fn prune(calls: &mut HashMap<String, PrivateCall>, timestamp: u64) {
    calls.retain(|_, call| {
        call.state == "accepted" || timestamp < call.expires_at.saturating_add(RESULT_SECONDS)
    });
    for call in calls.values_mut() {
        if call.state == "ringing" && timestamp >= call.expires_at {
            call.state = "missed".into();
        }
    }
}

pub(super) fn durable(db: &Connection) -> ApiResult<Vec<PrivateCall>> {
    let mut q = db.prepare("SELECT id,channel_id,caller_id,recipient_id,state,CASE WHEN state='accepted' THEN join_expires ELSE retained_until END FROM private_call_recovery")?;
    let rows = q
        .query_map([], |r| {
            Ok(PrivateCall {
                id: r.get(0)?,
                channel_id: r.get(1)?,
                caller_id: r.get(2)?,
                recipient_id: r.get(3)?,
                state: r.get(4)?,
                expires_at: r.get(5)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn bound_lease(db: &Connection, call: &str, participant: &str, at: u64) -> ApiResult<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM voice_resume_leases l JOIN sessions s ON s.token_hash=l.token_hash AND s.user_id=l.user_id WHERE l.private_call_id=?1 AND l.user_id=?2 AND l.expires>?3 AND s.expires>?3 AND (l.device_id IS NULL OR EXISTS(SELECT 1 FROM crypto_devices d WHERE d.device_id=l.device_id AND d.user_id=l.user_id AND d.active=1)))", params![call,participant,at], |r| r.get(0))?)
}

fn access_valid(result: ApiResult<()>) -> ApiResult<bool> {
    match result {
        Ok(()) => Ok(true),
        Err(error) if error.status.is_client_error() => Ok(false),
        Err(error) => Err(error),
    }
}

/// Caller holds DB first. All durable decisions commit before runtime state or
/// notices change. A failed write leaves the prior map and both seats untouched.
/// This also consumes revocation tombstones committed by database triggers.
pub(super) fn reconcile(state: &AppState, db: &Connection) -> ApiResult<()> {
    let at = now();
    let mut calls = lock(&state.0.private_calls);
    let seats: Vec<_> = lock(&state.0.voice)
        .values()
        .filter_map(|v| {
            v.private_call_id.as_ref().map(|id| {
                (
                    id.clone(),
                    v.user.id.clone(),
                    v.token_hash.clone(),
                    v.device_id.clone(),
                    v.alive_at(at),
                )
            })
        })
        .collect();
    let tx = db.unchecked_transaction()?;
    let protection = protection::current(&tx)?;
    let mut live = std::collections::HashSet::new();
    let mut revoked = std::collections::HashSet::new();
    for (id, user, token, device, alive) in seats {
        let device = if protection.level == ProtectionLevel::Open
            && device.as_deref() == Some(protection::open_device(&token).as_str())
        {
            None
        } else {
            device
        };
        let valid: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM sessions WHERE token_hash=?1 AND user_id=?2 AND expires>?3) AND (?4 IS NULL OR EXISTS(SELECT 1 FROM crypto_devices WHERE device_id=?4 AND user_id=?2 AND active=1))", params![token,user,at,device], |r| r.get(0))?;
        if !valid {
            revoked.insert(id);
        } else if alive {
            live.insert((id, user));
        }
    }
    let previous = durable(&tx)?;
    let known: std::collections::HashSet<_> = previous.iter().map(|c| c.id.clone()).collect();
    for call in previous {
        // The original join window admits both peers without inventing another
        // absence timer. Afterwards EACH exact participant needs its own live
        // seat or still-valid lease; one survivor cannot sustain an empty call.
        let mut missing_participant = false;
        if call.state == "accepted" && at >= call.expires_at {
            for participant in [&call.caller_id, &call.recipient_id] {
                if !live.contains(&(call.id.clone(), participant.clone()))
                    && !bound_lease(&tx, &call.id, participant, at)?
                {
                    missing_participant = true;
                    break;
                }
            }
        }
        if call.state == "accepted"
            && (revoked.contains(&call.id)
                || !access_valid(friends(&tx, &call.caller_id, &call.recipient_id))?
                || !access_valid(valid_room(
                    &tx,
                    &call.channel_id,
                    &call.caller_id,
                    &call.recipient_id,
                ))?
                || missing_participant)
        {
            persist_end(&tx, &call.id)?;
        }
    }
    tx.execute(
        "DELETE FROM private_call_recovery WHERE state='ended' AND retained_until<=?1",
        [at],
    )?;
    let saved = durable(&tx)?;
    tx.commit()?;
    apply_saved(state, &mut calls, saved, &known, at);
    Ok(())
}

/// Apply a snapshot read in the transaction that has just committed. There are
/// no fallible database operations after the first runtime mutation.
pub(super) fn committed(state: &AppState, saved: Vec<PrivateCall>) {
    let known = saved.iter().map(|c| c.id.clone()).collect();
    let mut calls = lock(&state.0.private_calls);
    apply_saved(state, &mut calls, saved, &known, now());
}

fn apply_saved(
    state: &AppState,
    calls: &mut HashMap<String, PrivateCall>,
    saved: Vec<PrivateCall>,
    known: &std::collections::HashSet<String>,
    at: u64,
) {
    let accepted: std::collections::HashSet<_> = saved
        .iter()
        .filter(|c| c.state == "accepted")
        .map(|c| c.id.clone())
        .collect();
    let changed: Vec<_> = saved
        .iter()
        .filter(|c| calls.get(&c.id) != Some(c))
        .cloned()
        .collect();
    calls.retain(|_, c| c.state != "accepted" && !(c.state == "ended" && known.contains(&c.id)));
    prune(calls, at);
    for call in saved {
        calls.insert(call.id.clone(), call);
    }
    remove_ended_seats(state, &accepted);
    for call in changed {
        notify(state, &call);
    }
}

fn remove_ended_seats(state: &AppState, accepted: &std::collections::HashSet<String>) {
    let removed: Vec<_> = {
        let mut voice = lock(&state.0.voice);
        let removed: Vec<_> = voice
            .values()
            .filter(|v| {
                v.private_call_id
                    .as_ref()
                    .is_some_and(|id| !accepted.contains(id))
            })
            .map(|v| (v.user.id.clone(), v.token_hash.clone()))
            .collect();
        voice.retain(|_, v| {
            v.private_call_id
                .as_ref()
                .is_none_or(|id| accepted.contains(id))
        });
        removed
    };
    for (user, token) in removed {
        state.revoke_rtc(&user, Some(&token));
        lock(&state.0.frames).retain(|(owner, _), _| owner != &user);
        state.notify("voice", None, Some(user), None);
    }
}

pub(super) fn persist_end(db: &Connection, id: &str) -> ApiResult<()> {
    db.execute("UPDATE private_call_recovery SET state='ended',retained_until=?2 WHERE id=?1 AND state='accepted'", params![id,now().saturating_add(RESULT_SECONDS)])?;
    Ok(())
}

/// End exact admitted calls for this login, including offline recovery seats.
/// No channel-to-call inference: a stale call can never terminate its successor.
pub(super) fn leave(
    state: &AppState,
    db: &Connection,
    user: &str,
    token: Option<&str>,
) -> ApiResult<()> {
    let mut ids = {
        let mut q = db.prepare("SELECT private_call_id FROM voice_resume_leases WHERE user_id=?1 AND (?2 IS NULL OR token_hash=?2) AND private_call_id IS NOT NULL")?;
        let rows = q
            .query_map(params![user, token], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        rows
    };
    ids.extend(
        lock(&state.0.voice)
            .values()
            .filter(|v| v.user.id == user && token.is_none_or(|t| t == v.token_hash))
            .filter_map(|v| v.private_call_id.clone()),
    );
    let tx = db.unchecked_transaction()?;
    for id in ids {
        persist_end(&tx, &id)?;
    }
    tx.execute(
        "DELETE FROM voice_resume_leases WHERE user_id=?1 AND (?2 IS NULL OR token_hash=?2)",
        params![user, token],
    )?;
    let saved = durable(&tx)?;
    tx.commit()?;
    committed(state, saved);
    Ok(())
}

/// Initial admission may bind only to an existing, accepted exact pair call.
/// Resume must supply the binding read from its authenticated lease instead.
pub(super) fn admission_call(
    db: &Connection,
    user: &str,
    channel: &str,
) -> ApiResult<Option<String>> {
    let id = db.query_row("SELECT id FROM private_call_recovery WHERE channel_id=?1 AND state='accepted' AND (caller_id=?2 OR recipient_id=?2)", params![channel,user], |r| r.get::<_,String>(0)).optional()?;
    if let Some(id) = &id {
        validate_binding(db, id, user, channel)?;
    }
    Ok(id)
}

pub(super) fn validate_binding(
    db: &Connection,
    id: &str,
    user: &str,
    channel: &str,
) -> ApiResult<()> {
    let call = durable(db)?
        .into_iter()
        .find(|c| {
            c.id == id
                && c.state == "accepted"
                && c.channel_id == channel
                && (c.caller_id == user || c.recipient_id == user)
        })
        .ok_or_else(forbidden)?;
    friends(db, &call.caller_id, &call.recipient_id)?;
    valid_room(db, channel, &call.caller_id, &call.recipient_id)
}

pub(super) fn social(state: &AppState, db: &Connection, user: &str) -> ApiResult<SocialState> {
    let mut social = social::relationships(db, user)?;
    reconcile(state, db)?;
    let mut calls = lock(&state.0.private_calls);
    prune(&mut calls, now());
    social.calls = calls
        .values()
        .filter(|call| {
            (call.caller_id == user || call.recipient_id == user)
                && friends(db, &call.caller_id, &call.recipient_id).is_ok()
                && valid_room(db, &call.channel_id, &call.caller_id, &call.recipient_id).is_ok()
        })
        .cloned()
        .collect();
    social.calls.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(social)
}

fn busy(state: &AppState, db: &Connection, user: &str) -> ApiResult<bool> {
    Ok(lock(&state.0.voice)
        .values()
        .any(|session| session.user.id == user && session.alive_at(now()))
        || db.query_row("SELECT EXISTS(SELECT 1 FROM voice_resume_leases l JOIN private_call_recovery c ON c.id=l.private_call_id JOIN sessions s ON s.token_hash=l.token_hash AND s.user_id=l.user_id WHERE l.user_id=?1 AND c.state='accepted' AND l.expires>?2 AND s.expires>?2)", params![user,now()], |r| r.get::<_,bool>(0))?)
}

fn valid_room(db: &Connection, channel: &str, caller: &str, recipient: &str) -> ApiResult<()> {
    let voice = conversations::info(db, channel)?.ok_or_else(missing)?;
    let group = conversations::info(db, voice.parent_id.as_deref().ok_or_else(missing)?)?
        .ok_or_else(missing)?;
    if voice.kind != "group_voice"
        || group.kind != "group"
        || voice.archived
        || group.archived
        || group.member_ids.len() != 2
        || !group.member_ids.iter().any(|id| id == caller)
        || !group.member_ids.iter().any(|id| id == recipient)
    {
        return Err(forbidden());
    }
    channel_access(db, channel, caller, JOIN)?;
    channel_access(db, channel, recipient, JOIN)
}

/// Reuse exactly the same friend pair. Never select a group containing a third
/// member or overwrite an existing name, membership, history or key target.
fn room(db: &mut Connection, caller: &str, recipient: &str) -> ApiResult<String> {
    let tx = db.transaction()?;
    friends(&tx, caller, recipient)?;
    let existing: Option<String> = tx.query_row(
        "SELECT v.channel_id FROM conversations v JOIN conversations g ON v.parent_id=g.channel_id WHERE v.kind='group_voice' AND g.kind='group' AND v.archived=0 AND g.archived=0 AND (g.owner_id=?1 OR g.owner_id=?2) AND (SELECT COUNT(*) FROM conversation_members m WHERE m.channel_id=g.channel_id)=2 AND EXISTS(SELECT 1 FROM conversation_members m WHERE m.channel_id=g.channel_id AND m.user_id=?1) AND EXISTS(SELECT 1 FROM conversation_members m WHERE m.channel_id=g.channel_id AND m.user_id=?2) ORDER BY v.channel_id LIMIT 1",
        params![caller, recipient], |r| r.get(0),
    ).optional()?;
    if let Some(channel) = existing {
        valid_room(&tx, &channel, caller, recipient)?;
        return Ok(channel);
    }
    let count: u32 = tx.query_row(
        "SELECT COUNT(*) FROM conversations WHERE owner_id=?1",
        [caller],
        |r| r.get(0),
    )?;
    if count >= 200 {
        return Err(bad("You can own up to 200 conversations"));
    }
    let group = id();
    let voice = id();
    let caller_name: String =
        tx.query_row("SELECT username FROM users WHERE id=?1", [caller], |row| {
            row.get(0)
        })?;
    let peer_name: String = tx.query_row(
        "SELECT username FROM users WHERE id=?1",
        [recipient],
        |row| row.get(0),
    )?;
    let mut name = format!("{caller_name} + {peer_name}");
    while name.len() > 64 {
        name.pop();
    }
    tx.execute(
        "INSERT INTO channels(id,name,kind,topic,visibility) VALUES(?1,?2,'text','','private')",
        params![group, name],
    )?;
    tx.execute(
        "INSERT INTO conversations(channel_id,kind,owner_id) VALUES(?1,'group',?2)",
        params![group, caller],
    )?;
    for user in [caller, recipient] {
        tx.execute(
            "INSERT INTO conversation_members VALUES(?1,?2)",
            params![group, user],
        )?;
    }
    tx.execute(
        "INSERT INTO channels(id,name,kind,topic,visibility) VALUES(?1,?2,'voice','','private')",
        params![voice, name],
    )?;
    tx.execute("INSERT INTO conversations(channel_id,kind,owner_id,parent_id) VALUES(?1,'group_voice',?2,?3)", params![voice, caller, group])?;
    voice_chat::ensure_rooms(&tx)?;
    tx.commit()?;
    Ok(voice)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StartCall {
    peer_id: String,
}

pub(super) async fn start(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<StartCall>,
) -> ApiResult<Json<PrivateCall>> {
    let auth = state.auth(&headers)?;
    let call = {
        let mut db = lock(&state.0.db);
        friends(&db, &auth.user.id, &input.peer_id)?;
        reconcile(&state, &db)?;
        let timestamp = now();
        let mut calls = lock(&state.0.private_calls);
        prune(&mut calls, timestamp);
        // Repeating Start does not create a second ring or extend its deadline.
        if let Some(call) = calls.values().find(|call| {
            call.state == "ringing"
                && call.caller_id == auth.user.id
                && call.recipient_id == input.peer_id
        }) {
            return Ok(Json(call.clone()));
        }
        if busy(&state, &db, &auth.user.id)?
            || busy(&state, &db, &input.peer_id)?
            || calls.values().any(|call| {
                (call.state == "ringing" || call.state == "accepted")
                    && [&call.caller_id, &call.recipient_id]
                        .iter()
                        .any(|user| **user == auth.user.id || **user == input.peer_id)
            })
        {
            return Err(bad(
                "One of you is already in a call. Leave that call before starting another.",
            ));
        }
        if calls.len() >= MAX_CALLS {
            return Err(bad("The server is busy. Try calling again shortly."));
        }
        let channel = room(&mut db, &auth.user.id, &input.peer_id)?;
        let call = PrivateCall {
            id: id(),
            channel_id: channel,
            caller_id: auth.user.id.clone(),
            recipient_id: input.peer_id,
            state: "ringing".into(),
            expires_at: timestamp.saturating_add(RING_SECONDS),
        };
        calls.insert(call.id.clone(), call.clone());
        call
    };
    notify(&state, &call);
    Ok(Json(call))
}

/// Validate and stage the exact invitation inside voice admission's transaction.
/// The volatile ring and current seat are unchanged until that transaction commits.
pub(super) fn prepare_switch(
    state: &AppState,
    db: &Connection,
    auth: &Auth,
    call_id: &str,
    channel: &str,
) -> ApiResult<PrivateCall> {
    let mut calls = lock(&state.0.private_calls);
    prune(&mut calls, now());
    let mut call = calls.get(call_id).cloned().ok_or_else(missing)?;
    if call.recipient_id != auth.user.id
        || call.channel_id != channel
        || call.caller_id == auth.user.id
    {
        return Err(forbidden());
    }
    friends(db, &call.caller_id, &call.recipient_id)?;
    valid_room(db, &call.channel_id, &call.caller_id, &call.recipient_id)?;
    if call.state != "ringing" && call.state != "accepted" {
        return Err(bad("This call is no longer ringing"));
    }
    if call.state == "ringing" && call.expires_at <= now() {
        return Err(bad("This call is no longer ringing"));
    }
    if lock(&state.0.voice).values().any(|v| {
        v.user.id == call.caller_id
            && v.alive_at(now())
            && v.private_call_id.as_deref() != Some(call_id)
    }) {
        return Err(bad(
            "The caller joined another call. Your current call was kept.",
        ));
    }
    let other_lease: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM voice_resume_leases l JOIN sessions s ON s.token_hash=l.token_hash AND s.user_id=l.user_id WHERE l.user_id=?1 AND l.expires>?2 AND s.expires>?2 AND l.private_call_id IS NOT ?3)", params![call.caller_id,now(),call_id], |r|r.get(0))?;
    if other_lease {
        return Err(bad(
            "The caller is reconnecting another call. Your current call was kept.",
        ));
    }
    if call.state == "ringing" {
        call.state = "accepted".into();
        call.expires_at = now().saturating_add(RESULT_SECONDS);
        db.execute("INSERT INTO private_call_recovery(id,channel_id,caller_id,recipient_id,state,join_expires,retained_until) VALUES(?1,?2,?3,?4,'accepted',?5,?5)", params![call.id,call.channel_id,call.caller_id,call.recipient_id,call.expires_at])?;
    }
    Ok(call)
}

pub(super) async fn answer_and_join(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(call_id): Path<String>,
    Json(input): Json<PrivateCallJoin>,
) -> ApiResult<Json<PrivateCallAdmission>> {
    voice_recovery::answer(&state, state.auth(&headers)?, &call_id, input)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AnswerCall {
    action: String,
}

pub(super) async fn answer(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(call_id): Path<String>,
    Json(input): Json<AnswerCall>,
) -> ApiResult<Json<PrivateCall>> {
    let auth = state.auth(&headers)?;
    let (result, caller_moved) = {
        let db = lock(&state.0.db);
        reconcile(&state, &db)?;
        let mut calls = lock(&state.0.private_calls);
        prune(&mut calls, now());
        let mut call = calls.get(&call_id).cloned().ok_or_else(missing)?;
        let (actor, target) = match input.action.as_str() {
            "accept" => (&call.recipient_id, "accepted"),
            "decline" => (&call.recipient_id, "declined"),
            "cancel" => (
                &call.caller_id,
                if call.state == "accepted" {
                    "ended"
                } else {
                    "cancelled"
                },
            ),
            "end" if auth.user.id == call.caller_id || auth.user.id == call.recipient_id => {
                (&auth.user.id, "ended")
            }
            _ => return Err(bad("Choose accept, decline, cancel or end")),
        };
        if auth.user.id != *actor {
            return Err(forbidden());
        }
        friends(&db, &call.caller_id, &call.recipient_id)?;
        valid_room(&db, &call.channel_id, &call.caller_id, &call.recipient_id)?;
        if call.state == target {
            return Ok(Json(call.clone()));
        }
        if call.state != "ringing" && !(call.state == "accepted" && target == "ended") {
            return Err(bad("This call is no longer ringing"));
        }
        if target == "accepted" && busy(&state, &db, &call.recipient_id)? {
            return Err(bad("Leave your current call before accepting this one"));
        }
        let caller_moved = target == "accepted" && busy(&state, &db, &call.caller_id)?;
        call.state = if caller_moved { "cancelled" } else { target }.into();
        call.expires_at = now().saturating_add(RESULT_SECONDS);
        // Commit accepted identity or its irreversible tombstone before touching
        // the volatile invitation, admitted seats, frames or notifications.
        if call.state == "accepted" {
            let tx = db.unchecked_transaction()?;
            tx.execute("INSERT INTO private_call_recovery(id,channel_id,caller_id,recipient_id,state,join_expires,retained_until) VALUES(?1,?2,?3,?4,'accepted',?5,?5)", params![call.id,call.channel_id,call.caller_id,call.recipient_id,call.expires_at])?;
            tx.commit()?;
        } else if call.state == "ended" {
            let tx = db.unchecked_transaction()?;
            persist_end(&tx, &call.id)?;
            tx.commit()?;
        }
        calls.insert(call.id.clone(), call.clone());
        if call.state == "ended" {
            let accepted = calls
                .values()
                .filter(|c| c.state == "accepted")
                .map(|c| c.id.clone())
                .collect();
            remove_ended_seats(&state, &accepted);
        }
        (call, caller_moved)
    };
    notify(&state, &result);
    if caller_moved {
        return Err(bad(
            "The caller joined another call. This call was cancelled.",
        ));
    }
    Ok(Json(result))
}

/// Joining another room cancels only this user's outgoing, unanswered rings.
/// Run after admission commits and releases DB/voice locks; an acceptance racing
/// this update sees the new voice seat and cancels the same ring in `answer`.
pub(super) fn joined_other_room(state: &AppState, user: &str, channel: &str) {
    let changed: Vec<_> = {
        let mut calls = lock(&state.0.private_calls);
        calls
            .values_mut()
            .filter(|call| {
                call.caller_id == user && call.channel_id != channel && call.state == "ringing"
            })
            .map(|call| {
                call.state = "cancelled".into();
                call.expires_at = now().saturating_add(RESULT_SECONDS);
                call.clone()
            })
            .collect()
    };
    for call in changed {
        notify(state, &call);
    }
}

fn notify(state: &AppState, call: &PrivateCall) {
    state.notify(
        "members",
        None,
        None,
        Some(vec![call.caller_id.clone(), call.recipient_id.clone()]),
    );
}

/// Explicit relationship revocation or leaving this exact admitted room ends
/// signalling too. No other conversation is stopped by a stale call notice.
pub(super) fn end_matching(
    state: &AppState,
    user: &str,
    peer: Option<&str>,
    channel: Option<&str>,
) -> ApiResult<()> {
    let db = lock(&state.0.db);
    reconcile(state, &db)?;
    let changed: Vec<_> = {
        let mut calls = lock(&state.0.private_calls);
        let matching: Vec<_> = calls
            .values()
            .filter(|call| {
                (call.caller_id == user || call.recipient_id == user)
                    && peer.is_none_or(|peer| call.caller_id == peer || call.recipient_id == peer)
                    && channel.is_none_or(|channel| call.channel_id == channel)
                    && matches!(call.state.as_str(), "ringing" | "accepted")
            })
            .cloned()
            .collect();
        let tx = db.unchecked_transaction()?;
        for call in &matching {
            if call.state == "accepted" {
                persist_end(&tx, &call.id)?;
            }
        }
        tx.commit()?;
        let changed = matching
            .into_iter()
            .map(|mut call| {
                call.state = if call.state == "accepted" {
                    "ended"
                } else {
                    "cancelled"
                }
                .into();
                call.expires_at = now().saturating_add(RESULT_SECONDS);
                calls.insert(call.id.clone(), call.clone());
                call
            })
            .collect();
        let accepted = calls
            .values()
            .filter(|c| c.state == "accepted")
            .map(|c| c.id.clone())
            .collect();
        remove_ended_seats(state, &accepted);
        changed
    };
    for call in changed {
        notify(state, &call);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{new_invite, request, signup};
    use serde_json::{json, Value};

    #[tokio::test]
    async fn private_call_requires_mutual_friends_and_only_the_recipient_can_accept() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::open(temp.path(), "call-fixture", 0).unwrap();
        let app = router(state.clone());
        let a = signup(&app, "alice", "").await;
        let b = signup(&app, "bob", &new_invite(&app, &a).await).await;
        let at = a.token.as_str();
        let bt = b.token.as_str();
        let aid = a.user.id.as_str();
        let bid = b.user.id.as_str();
        let outsider = signup(&app, "outsider", &new_invite(&app, &a).await).await;
        let payload = json!({"peer_id":bid});
        assert_eq!(
            request(
                &app,
                "POST",
                "/api/private-calls",
                Some(at),
                payload.clone()
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        request(
            &app,
            "POST",
            &format!("/api/friends/{bid}"),
            Some(at),
            json!({}),
        )
        .await;
        request(
            &app,
            "POST",
            &format!("/api/friends/{aid}"),
            Some(bt),
            json!({}),
        )
        .await;
        let (status, call) = request(
            &app,
            "POST",
            "/api/private-calls",
            Some(at),
            payload.clone(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(call["state"], "ringing");
        assert_eq!(
            request(&app, "POST", "/api/private-calls", Some(at), payload)
                .await
                .1["id"],
            call["id"]
        );
        let endpoint = format!("/api/private-calls/{}", call["id"].as_str().unwrap());
        assert_eq!(
            request(
                &app,
                "GET",
                "/api/social",
                Some(&outsider.token),
                Value::Null
            )
            .await
            .1
            .get("calls"),
            None
        );
        assert_eq!(
            request(
                &app,
                "POST",
                &endpoint,
                Some(&outsider.token),
                json!({"action":"decline"})
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            request(
                &app,
                "POST",
                &endpoint,
                Some(at),
                json!({"action":"accept"})
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            request(
                &app,
                "POST",
                &endpoint,
                Some(bt),
                json!({"action":"accept"})
            )
            .await
            .1["state"],
            "accepted"
        );
        assert_eq!(
            request(
                &app,
                "POST",
                &endpoint,
                Some(at),
                json!({"action":"cancel"})
            )
            .await
            .1["state"],
            "ended"
        );
        assert!(
            lock(&state.0.voice).is_empty(),
            "signalling never opens devices or grants voice admission"
        );
    }

    #[tokio::test]
    async fn expired_or_changed_private_rooms_cannot_be_accepted_and_never_join_voice() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::open(temp.path(), "call-expiry", 0).unwrap();
        let app = router(state.clone());
        let a = signup(&app, "alice", "").await;
        let b = signup(&app, "bob", &new_invite(&app, &a).await).await;
        let third = signup(&app, "third", &new_invite(&app, &a).await).await;
        request(
            &app,
            "POST",
            &format!("/api/friends/{}", b.user.id),
            Some(&a.token),
            Value::Null,
        )
        .await;
        request(
            &app,
            "POST",
            &format!("/api/friends/{}", a.user.id),
            Some(&b.token),
            Value::Null,
        )
        .await;
        let (_, call) = request(
            &app,
            "POST",
            "/api/private-calls",
            Some(&a.token),
            json!({"peer_id":b.user.id}),
        )
        .await;
        let cid = call["id"].as_str().unwrap();
        let channel = call["channel_id"].as_str().unwrap();
        lock(&state.0.private_calls)
            .get_mut(cid)
            .unwrap()
            .expires_at = now().saturating_sub(1);
        assert_eq!(
            request(
                &app,
                "POST",
                &format!("/api/private-calls/{cid}"),
                Some(&b.token),
                json!({"action":"accept"})
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        let (_, fresh) = request(
            &app,
            "POST",
            "/api/private-calls",
            Some(&a.token),
            json!({"peer_id":b.user.id}),
        )
        .await;
        assert_eq!(
            fresh["channel_id"], channel,
            "retry reuses the pair's room and key target"
        );
        {
            let db = lock(&state.0.db);
            let voice = conversations::info(&db, channel).unwrap().unwrap();
            db.execute(
                "INSERT INTO conversation_members VALUES(?1,?2)",
                params![voice.parent_id.unwrap(), third.user.id],
            )
            .unwrap();
        }
        assert_eq!(
            request(
                &app,
                "POST",
                &format!("/api/private-calls/{}", fresh["id"].as_str().unwrap()),
                Some(&b.token),
                json!({"action":"accept"})
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        assert!(lock(&state.0.voice).is_empty());
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use crate::tests::{new_invite, request, signup};
    use serde_json::{json, Value};
    #[tokio::test]
    async fn a_longer_call_keeps_end_signalling_and_explicit_room_leave_ends_it() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::open(temp.path(), "long-call", 0).unwrap();
        let app = router(state.clone());
        let a = signup(&app, "alice", "").await;
        let b = signup(&app, "bob", &new_invite(&app, &a).await).await;
        request(
            &app,
            "POST",
            &format!("/api/friends/{}", b.user.id),
            Some(&a.token),
            json!({}),
        )
        .await;
        request(
            &app,
            "POST",
            &format!("/api/friends/{}", a.user.id),
            Some(&b.token),
            json!({}),
        )
        .await;
        let call = request(
            &app,
            "POST",
            "/api/private-calls",
            Some(&a.token),
            json!({"peer_id":b.user.id}),
        )
        .await
        .1;
        let id = call["id"].as_str().unwrap();
        let path = format!("/api/private-calls/{id}");
        assert_eq!(
            request(
                &app,
                "POST",
                &path,
                Some(&b.token),
                json!({"action":"accept"})
            )
            .await
            .0,
            StatusCode::OK
        );
        assert_eq!(
            request(
                &app,
                "POST",
                "/api/voice/join",
                Some(&a.token),
                json!({"channel_id":call["channel_id"]})
            )
            .await
            .0,
            StatusCode::OK
        );
        assert_eq!(
            request(
                &app,
                "POST",
                "/api/voice/join",
                Some(&b.token),
                json!({"channel_id":call["channel_id"]})
            )
            .await
            .0,
            StatusCode::OK
        );
        lock(&state.0.db)
            .execute(
                "UPDATE private_call_recovery SET join_expires=?1 WHERE id=?2",
                params![now().saturating_sub(90), id],
            )
            .unwrap();
        assert_eq!(
            social(&state, &lock(&state.0.db), &b.user.id)
                .unwrap()
                .calls
                .len(),
            1,
            "live media retains signalling after the join timeout"
        );
        assert_eq!(
            request(
                &app,
                "POST",
                "/api/voice/leave",
                Some(&a.token),
                Value::Null
            )
            .await
            .0,
            StatusCode::NO_CONTENT
        );
        assert_eq!(lock(&state.0.private_calls)[id].state, "ended");
        assert!(lock(&state.0.voice).is_empty());
    }
}

#[cfg(test)]
#[path = "private_call_busy_tests.rs"]
mod busy_tests;

#[cfg(test)]
#[path = "private_call_recovery_tests.rs"]
mod recovery_tests;

#[cfg(test)]
#[path = "private_call_switch_tests.rs"]
mod switch_tests;
