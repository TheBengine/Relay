//! Presentation metadata stays separate from the immutable encryption server ID.
use super::*;
#[cfg(test)]
mod tests;

pub(super) fn presentation(db: &Connection, fallback: &str) -> ApiResult<ServerPresentation> {
    let value: Option<String> = db
        .query_row(
            "SELECT value FROM metadata WHERE key='presentation'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    match value {
        Some(value) => {
            let mut value: ServerPresentation =
                serde_json::from_str(&value).map_err(|_| bad("Server settings are damaged"))?;
            for channel in [&mut value.welcome_channel_id, &mut value.system_channel_id] {
                if let Some(id) = channel.as_ref() {
                    if id.is_empty() || !notice_channel(db, id)? {
                        *channel = None;
                    }
                }
            }
            Ok(value)
        }
        None => Ok(ServerPresentation {
            name: fallback.into(),
            ..Default::default()
        }),
    }
}
pub(super) async fn get_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<ServerPresentation>> {
    let auth = state.auth(&headers)?;
    let db = lock(&state.0.db);
    Ok(Json(for_user(&db, &state.0.name, &auth.user.id)?))
}
pub(super) async fn save_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut input): Json<ServerPresentation>,
) -> ApiResult<Json<ServerPresentation>> {
    let auth = state.auth(&headers)?;
    if !input.valid() {
        return Err(bad(
            "Use a server name of 1–100 characters, a description up to 300 and valid colors",
        ));
    }
    input.name = input.name.trim().into();
    {
        let mut db = lock(&state.0.db);
        let tx = db.transaction()?;
        active_user(&tx, &auth.user.id)?;
        roles::require(&tx, &auth.user.id, PERMISSION_ADMIN)?;
        let previous = presentation(&tx, &state.0.name)?;
        input.welcome_channel_id = input.welcome_channel_id.or(previous.welcome_channel_id);
        input.system_channel_id = input.system_channel_id.or(previous.system_channel_id);
        for channel in [&input.welcome_channel_id, &input.system_channel_id]
            .into_iter()
            .flatten()
        {
            if !channel.is_empty() && !notice_channel(&tx, channel)? {
                return Err(bad(
                    "Choose a public community text channel for welcome and system messages",
                ));
            }
        }
        let json = serde_json::to_string(&input).map_err(|_| bad("Invalid server settings"))?;
        tx.execute("INSERT INTO metadata(key,value) VALUES('presentation',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [json])?;
        roles::audit(&tx, &auth.user.id, "server.updated", "presentation")?;
        tx.commit()?;
    }
    state.notify("channels", None, None, None);
    Ok(Json(input))
}

fn notice_channel(db: &Connection, id: &str) -> ApiResult<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM channels c WHERE c.id=?1 AND c.kind='text' AND c.visibility='public' AND c.voice_parent_id IS NULL AND NOT EXISTS(SELECT 1 FROM conversations t WHERE t.channel_id=c.id AND t.kind IN('group','thread')))",[id],|r|r.get(0))?)
}

pub(super) fn for_user(
    db: &Connection,
    fallback: &str,
    user: &str,
) -> ApiResult<ServerPresentation> {
    let mut value = presentation(db, fallback)?;
    let channels = visible_channels(db, user)?;
    for channel in [&mut value.welcome_channel_id, &mut value.system_channel_id] {
        if channel
            .as_ref()
            .is_some_and(|id| !channels.iter().any(|c| c.id == *id))
        {
            *channel = None;
        }
    }
    Ok(value)
}

pub(super) fn record_system_event(db: &Connection, user: &str, kind: &str) -> rusqlite::Result<()> {
    db.execute(
        "INSERT INTO community_system_events(user_id,kind,created_at) VALUES(?1,?2,?3)",
        params![
            user,
            kind,
            chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        ],
    )?;
    db.execute("DELETE FROM community_system_events WHERE sequence <= (SELECT COALESCE(MAX(sequence),0)-10000 FROM community_system_events)",[])?;
    Ok(())
}

pub(super) async fn system_events(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Vec<CommunitySystemEvent>>> {
    let auth = state.auth(&headers)?;
    let db = lock(&state.0.db);
    let presentation = for_user(&db, &state.0.name, &auth.user.id)?;
    let channel = presentation
        .system_channel_id
        .as_ref()
        .ok_or_else(missing)?;
    channel_access(&db, channel, &auth.user.id, PERMISSION_READ)?;
    let mut query = db.prepare("SELECT sequence,user_id,kind,created_at FROM community_system_events e WHERE NOT EXISTS(SELECT 1 FROM user_blocks b WHERE b.owner=?1 AND b.peer=e.user_id) ORDER BY sequence DESC LIMIT 50")?;
    let rows = query
        .query_map([&auth.user.id], |r| {
            Ok((
                r.get::<_, u64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut events = Vec::with_capacity(rows.len());
    for (sequence, user, kind, created_at) in rows {
        events.push(CommunitySystemEvent {
            sequence,
            user: get_user(&db, &user)?,
            kind,
            created_at,
        });
    }
    Ok(Json(events))
}
