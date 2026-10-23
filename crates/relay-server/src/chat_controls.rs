//! Durable channel controls and short-lived, permission-scoped typing hints.
use super::*;
use std::time::Instant;

pub(super) const MESSAGE_BURST_WINDOW_MS: u64 = 5_000;
const MESSAGE_BURST_LIMIT: u64 = 5;
const MAX_TYPING_ENTRIES: usize = 2048;
const MAX_TYPING_PER_CHANNEL: usize = 128;
const MAX_TYPING_PER_USER: usize = 2;

#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub(super) enum TypingTarget {
    Channel(String),
    Direct(String),
}
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub(super) struct TypingKey {
    pub user_id: String,
    pub target: TypingTarget,
}
pub(super) struct TypingEntry {
    token_hash: String,
    pub(super) expires: Instant,
    pub(super) expires_at_ms: u64,
}

/// Called after a send receipt lookup and inside the same transaction as the
/// new message. Failed validation rolls back both quotas. Deletion never refunds
/// a successful send, and a new login/restart cannot bypass either cooldown.
pub(super) fn record_message_quota(
    db: &Connection,
    input: &SendMessage,
    user_id: &str,
    timestamp: u64,
) -> ApiResult<()> {
    if let Some(channel) = &input.channel_id {
        let seconds = get_channel(db, channel)?.slowmode_seconds;
        let staff = roles::has(db, user_id, PERMISSION_MESSAGES);
        if seconds > 0 && !staff {
            let previous = db.query_row("SELECT last_sent_ms FROM channel_send_cooldowns WHERE user_id=?1 AND channel_id=?2",params![user_id,channel],|r|r.get::<_,u64>(0)).optional()?;
            if let Some(previous) = previous {
                let interval = u64::from(seconds) * 1000;
                // A backwards system clock fails closed for at most the stated
                // interval per response; opening a database rebases future rows.
                let remaining = previous
                    .min(timestamp)
                    .saturating_add(interval)
                    .saturating_sub(timestamp);
                if remaining > 0 {
                    return Err(ApiError::cooldown(
                        "slowmode",
                        "This channel has slowmode; wait before sending again",
                        remaining,
                        Some(channel.clone()),
                    ));
                }
            }
        }
    }
    db.execute(
        "DELETE FROM message_send_log WHERE sent_at_ms<=?1",
        [timestamp.saturating_sub(MESSAGE_BURST_WINDOW_MS)],
    )?;
    let (count, oldest) = db.query_row(
        "SELECT COUNT(*),MIN(sent_at_ms) FROM message_send_log WHERE user_id=?1",
        [user_id],
        |r| Ok((r.get::<_, u64>(0)?, r.get::<_, Option<u64>>(1)?)),
    )?;
    if count >= MESSAGE_BURST_LIMIT {
        let remaining = oldest
            .unwrap_or(timestamp)
            .min(timestamp)
            .saturating_add(MESSAGE_BURST_WINDOW_MS)
            .saturating_sub(timestamp)
            .max(1);
        return Err(ApiError::cooldown(
            "message_rate_limit",
            "Messages are being sent too quickly; wait before sending again",
            remaining,
            None,
        ));
    }
    db.execute(
        "INSERT INTO message_send_log(user_id,sent_at_ms) VALUES(?1,?2)",
        params![user_id, timestamp],
    )?;
    if let Some(channel) = &input.channel_id {
        db.execute("INSERT INTO channel_send_cooldowns(user_id,channel_id,last_sent_ms) VALUES(?1,?2,?3) ON CONFLICT(user_id,channel_id) DO UPDATE SET last_sent_ms=excluded.last_sent_ms",params![user_id,channel,timestamp])?;
    }
    Ok(())
}

fn text_channel(db: &Connection, channel: &str, user: &str, permission: u32) -> ApiResult<()> {
    channel_access(db, channel, user, permission)?;
    if get_channel(db, channel)?.kind != "text" {
        return Err(bad("Choose a text channel"));
    }
    Ok(())
}
fn get_pin(db: &Connection, message_id: &str) -> ApiResult<PinnedMessage> {
    let (user, pinned_at) = db
        .query_row(
            "SELECT pinned_by,pinned_at FROM channel_pins WHERE message_id=?1",
            [message_id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )
        .optional()?
        .ok_or_else(missing)?;
    Ok(PinnedMessage {
        message: get_message(db, message_id)?,
        pinned_by: get_user(db, &user)?,
        pinned_at,
    })
}
pub(super) async fn pins(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(channel): Path<String>,
) -> ApiResult<Json<Vec<PinnedMessage>>> {
    let auth = state.auth(&headers)?;
    let db = lock(&state.0.db);
    text_channel(&db, &channel, &auth.user.id, READ)?;
    let ids = {
        let mut query=db.prepare("SELECT message_id FROM channel_pins WHERE channel_id=?1 ORDER BY pinned_ms DESC,message_id LIMIT ?2")?;
        let rows = query
            .query_map(params![channel, MAX_CHANNEL_PINS], |r| {
                r.get::<_, String>(0)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows
    };
    Ok(Json(
        ids.iter()
            .map(|id| get_pin(&db, id))
            .collect::<ApiResult<Vec<_>>>()?,
    ))
}
pub(super) async fn pin_message(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((channel, message_id)): Path<(String, String)>,
) -> ApiResult<Json<PinnedMessage>> {
    let auth = state.auth(&headers)?;
    roles::require(&lock(&state.0.db), &auth.user.id, PERMISSION_MESSAGES)?;
    let (pin, changed) = {
        let mut db = lock(&state.0.db);
        let tx = db.transaction()?;
        roles::require(&tx, &auth.user.id, PERMISSION_MESSAGES)?;
        text_channel(&tx, &channel, &auth.user.id, READ)?;
        if message_access(&tx, &message_id, &auth.user.id)?
            .0
            .as_deref()
            != Some(channel.as_str())
        {
            return Err(forbidden());
        }
        let exists = tx.query_row(
            "SELECT COUNT(*) FROM channel_pins WHERE message_id=?1",
            [&message_id],
            |r| r.get::<_, u64>(0),
        )? != 0;
        if !exists {
            let count = tx.query_row(
                "SELECT COUNT(*) FROM channel_pins WHERE channel_id=?1",
                [&channel],
                |r| r.get::<_, usize>(0),
            )?;
            if count >= MAX_CHANNEL_PINS {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "This channel already has 50 pinned messages",
                ));
            }
            let at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
            tx.execute("INSERT INTO channel_pins(message_id,channel_id,pinned_by,pinned_at,pinned_ms) VALUES(?1,?2,?3,?4,?5)",params![message_id,channel,auth.user.id,at,now_ms()])?;
            roles::audit(&tx, &auth.user.id, "message.pinned", &message_id)?;
            record_change(&tx, &get_message(&tx, &message_id)?, "updated")?;
        }
        let pin = get_pin(&tx, &message_id)?;
        tx.commit()?;
        (pin, !exists)
    };
    if changed {
        message_notice(&state, "messages", &pin.message);
        state.notify("pins", Some(channel), Some(auth.user.id), None);
    }
    Ok(Json(pin))
}
pub(super) async fn unpin_message(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((channel, message_id)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let auth = state.auth(&headers)?;
    roles::require(&lock(&state.0.db), &auth.user.id, PERMISSION_MESSAGES)?;
    let changed = {
        let mut db = lock(&state.0.db);
        let tx = db.transaction()?;
        roles::require(&tx, &auth.user.id, PERMISSION_MESSAGES)?;
        text_channel(&tx, &channel, &auth.user.id, READ)?;
        if message_access(&tx, &message_id, &auth.user.id)?
            .0
            .as_deref()
            != Some(channel.as_str())
        {
            return Err(forbidden());
        }
        let changed = tx.execute(
            "DELETE FROM channel_pins WHERE message_id=?1 AND channel_id=?2",
            params![message_id, channel],
        )? != 0;
        let message = get_message(&tx, &message_id)?;
        if changed {
            roles::audit(&tx, &auth.user.id, "message.unpinned", &message_id)?;
            record_change(&tx, &message, "updated")?;
        }
        tx.commit()?;
        changed.then_some(message)
    };
    if let Some(message) = changed {
        message_notice(&state, "messages", &message);
        state.notify("pins", Some(channel), Some(auth.user.id), None);
    }
    Ok(StatusCode::NO_CONTENT)
}

fn target(channel: Option<String>, peer: Option<String>) -> ApiResult<TypingTarget> {
    match (channel, peer) {
        (Some(id), None) if !id.is_empty() && id.len() <= 128 => Ok(TypingTarget::Channel(id)),
        (None, Some(id)) if !id.is_empty() && id.len() <= 128 => Ok(TypingTarget::Direct(id)),
        _ => Err(bad("Choose one text channel or peer")),
    }
}
fn typing_access(
    db: &Connection,
    target: &TypingTarget,
    user: &str,
    permission: u32,
) -> ApiResult<()> {
    active_user(db, user)?;
    match target {
        TypingTarget::Channel(channel) => text_channel(db, channel, user, permission),
        TypingTarget::Direct(peer) => social::direct_access(db, user, peer),
    }
}
fn entry_valid(db: &Connection, key: &TypingKey, value: &TypingEntry, instant: Instant) -> bool {
    value.expires > instant
        && db
            .query_row(
                "SELECT COUNT(*) FROM sessions WHERE token_hash=?1 AND user_id=?2 AND expires>?3",
                params![value.token_hash, key.user_id, now()],
                |r| r.get::<_, u64>(0),
            )
            .unwrap_or(0)
            == 1
        && typing_access(db, &key.target, &key.user_id, SEND).is_ok()
}
fn emit_typing(state: &AppState, key: &TypingKey, typing: bool, expires_at_ms: u64) {
    let (channel, recipient, audience) = match &key.target {
        TypingTarget::Channel(id) => (Some(id.clone()), None, None),
        TypingTarget::Direct(id) => (
            None,
            Some(id.clone()),
            Some(vec![key.user_id.clone(), id.clone()]),
        ),
    };
    let _ = state.0.events.send(Notice {
        event: Event {
            kind: "typing".into(),
            channel_id: channel,
            user_id: Some(key.user_id.clone()),
            recipient_id: recipient,
            typing: Some(typing),
            expires_at_ms: Some(expires_at_ms),
        },
        audience,
    });
}
pub(super) async fn set_typing(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<TypingRequest>,
) -> ApiResult<StatusCode> {
    let auth = state.auth(&headers)?;
    let target = target(input.channel_id, input.peer_id)?;
    let key = TypingKey {
        user_id: auth.user.id.clone(),
        target,
    };
    let timestamp = now_ms();
    let instant = Instant::now();
    let expires_at_ms = timestamp.saturating_add(TYPING_TTL_MS);
    let changed = {
        let db = lock(&state.0.db);
        typing_access(
            &db,
            &key.target,
            &key.user_id,
            if input.typing { SEND } else { READ },
        )?;
        let mut entries = lock(&state.0.typing);
        entries.retain(|key, value| entry_valid(&db, key, value, instant));
        if input.typing {
            if !entries.contains_key(&key) {
                let per_user = entries.keys().filter(|k| k.user_id == key.user_id).count();
                let per_channel = entries.keys().filter(|k| k.target == key.target).count();
                if entries.len() >= MAX_TYPING_ENTRIES
                    || per_user >= MAX_TYPING_PER_USER
                    || per_channel >= MAX_TYPING_PER_CHANNEL
                {
                    return Err(ApiError::cooldown(
                        "service_busy",
                        "Typing capacity reached; retry shortly",
                        TYPING_TTL_MS,
                        None,
                    ));
                }
            }
            entries.insert(
                key.clone(),
                TypingEntry {
                    token_hash: auth.token_hash,
                    expires: instant + Duration::from_millis(TYPING_TTL_MS),
                    expires_at_ms,
                },
            );
            true
        } else if entries
            .get(&key)
            .is_some_and(|e| e.token_hash == auth.token_hash)
        {
            entries.remove(&key);
            true
        } else {
            false
        }
    };
    if changed {
        emit_typing(
            &state,
            &key,
            input.typing,
            if input.typing {
                expires_at_ms
            } else {
                timestamp
            },
        );
    }
    Ok(StatusCode::NO_CONTENT)
}
#[derive(Deserialize)]
pub(super) struct TypingQuery {
    channel_id: Option<String>,
    peer_id: Option<String>,
}
pub(super) async fn typing_state(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<TypingQuery>,
) -> ApiResult<Json<TypingState>> {
    let auth = state.auth(&headers)?;
    let target = target(query.channel_id, query.peer_id)?;
    let db = lock(&state.0.db);
    typing_access(&db, &target, &auth.user.id, READ)?;
    let instant = Instant::now();
    let server_time_ms = now_ms();
    let mut entries = lock(&state.0.typing);
    entries.retain(|key, value| entry_valid(&db, key, value, instant));
    let mut users = Vec::new();
    for (key, entry) in entries.iter() {
        let matches = match (&target, &key.target) {
            (TypingTarget::Channel(channel), TypingTarget::Channel(other)) => {
                channel == other && key.user_id != auth.user.id
            }
            (TypingTarget::Direct(peer), TypingTarget::Direct(recipient)) => {
                &key.user_id == peer && recipient == &auth.user.id
            }
            _ => false,
        };
        if matches {
            users.push(TypingUser {
                user: get_user(&db, &key.user_id)?,
                expires_at_ms: server_time_ms.saturating_add(
                    entry.expires.saturating_duration_since(instant).as_millis() as u64,
                ),
            });
        }
    }
    users.sort_by(|a, b| a.user.id.cmp(&b.user.id));
    Ok(Json(TypingState {
        users,
        server_time_ms,
    }))
}
pub(super) fn clear_sent_typing(state: &AppState, user: &str, input: &SendMessage) {
    let target = match (&input.channel_id, &input.recipient_id) {
        (Some(channel), None) => TypingTarget::Channel(channel.clone()),
        (None, Some(peer)) => TypingTarget::Direct(peer.clone()),
        _ => return,
    };
    let key = TypingKey {
        user_id: user.into(),
        target,
    };
    if lock(&state.0.typing).remove(&key).is_some() {
        emit_typing(state, &key, false, now_ms());
    }
}
pub(super) fn prune_typing(state: &AppState) {
    let db = lock(&state.0.db);
    let instant = Instant::now();
    lock(&state.0.typing).retain(|key, value| entry_valid(&db, key, value, instant));
}
/// A queued hint must not expose an author's activity after access is revoked.
pub(super) fn typing_notice_valid(state: &AppState, event: &Event) -> bool {
    let Some(user) = event.user_id.as_deref() else {
        return false;
    };
    let Ok(target) = target(event.channel_id.clone(), event.recipient_id.clone()) else {
        return false;
    };
    let db = lock(&state.0.db);
    if typing_access(&db, &target, user, SEND).is_err() {
        return false;
    }
    if event.typing != Some(true) {
        return true;
    }
    let key = TypingKey {
        user_id: user.into(),
        target,
    };
    lock(&state.0.typing).get(&key).is_some_and(|value| {
        entry_valid(&db, &key, value, Instant::now())
            && event.expires_at_ms == Some(value.expires_at_ms)
    })
}
