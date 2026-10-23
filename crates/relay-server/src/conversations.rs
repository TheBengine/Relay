//! Additional conversations reuse ordinary channel MLS targets and current membership checks.
use super::*;
use serde_json::{json, Value};
use std::collections::HashSet;
#[cfg(test)]
mod tests;

pub(super) fn info(db: &Connection, channel: &str) -> ApiResult<Option<ConversationInfo>> {
    let mut result=db.query_row("SELECT channel_id,kind,owner_id,parent_id,archived FROM conversations WHERE channel_id=?1",[channel],|r|Ok(ConversationInfo{channel_id:r.get(0)?,kind:r.get(1)?,owner_id:r.get(2)?,parent_id:r.get(3)?,archived:r.get(4)?,member_ids:vec![]})).optional()?;
    if let Some(info) = &mut result {
        let mut q=db.prepare("SELECT m.user_id FROM conversation_members m JOIN membership u ON u.user_id=m.user_id WHERE m.channel_id=?1 AND u.status='active' ORDER BY m.user_id")?;
        info.member_ids = q
            .query_map([channel], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
    }
    Ok(result)
}
pub(super) fn access(
    db: &Connection,
    channel: &str,
    user: &str,
    permission: u32,
) -> ApiResult<Option<ApiResult<()>>> {
    let Some(c) = info(db, channel)? else {
        return Ok(None);
    };
    if c.kind == "group" || c.kind == "group_voice" {
        let members = if c.kind == "group_voice" {
            info(db, c.parent_id.as_deref().ok_or_else(missing)?)?
                .ok_or_else(missing)?
                .member_ids
        } else {
            c.member_ids
        };
        return Ok(Some(if members.iter().any(|id| id == user) {
            Ok(())
        } else {
            Err(forbidden())
        }));
    }
    if c.archived && permission & SEND != 0 {
        return Ok(Some(Err(forbidden())));
    }
    if c.kind == "announcement"
        && permission & SEND != 0
        && !roles::has(db, user, PERMISSION_MESSAGES)
    {
        return Ok(Some(Err(forbidden())));
    }
    if c.kind == "thread" {
        return Ok(Some(channel_access(
            db,
            c.parent_id.as_deref().ok_or_else(missing)?,
            user,
            permission,
        )));
    }
    if c.kind == "stage" && permission & STREAM != 0 && !stage_speaker(db, channel, user)? {
        return Ok(Some(Err(forbidden())));
    }
    Ok(None)
}
pub(super) fn list(db: &Connection, user: &str) -> ApiResult<Vec<ConversationInfo>> {
    let mut q = db.prepare("SELECT channel_id FROM conversations ORDER BY channel_id")?;
    let ids = q
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut result = vec![];
    for id in ids {
        if channel_access(db, &id, user, READ).is_ok() {
            if let Some(c) = info(db, &id)? {
                result.push(c);
            }
        }
    }
    Ok(result)
}
pub(super) fn require_regular(db: &Connection, channel: &str) -> ApiResult<()> {
    if info(db, channel)?
        .is_some_and(|c| matches!(c.kind.as_str(), "group" | "group_voice" | "thread"))
    {
        return Err(bad("Use conversation settings to manage this conversation"));
    }
    Ok(())
}
pub(super) fn delete_tree(db: &Connection, root: &str) -> ApiResult<()> {
    let mut q=db.prepare("WITH RECURSIVE children(id,depth) AS (SELECT ?1,0 UNION ALL SELECT c.id,children.depth+1 FROM channels c JOIN children ON c.voice_parent_id=children.id UNION ALL SELECT c.channel_id,children.depth+1 FROM conversations c JOIN children ON c.parent_id=children.id) SELECT id FROM children ORDER BY depth DESC")?;
    let ids = q
        .query_map([root], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    for channel in ids {
        let mut q = db.prepare("SELECT id FROM messages WHERE channel_id=?1")?;
        let messages = q
            .query_map([&channel], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        for message in messages {
            record_change(db, &get_message(db, &message)?, "deleted")?;
        }
        db.execute("DELETE FROM messages WHERE channel_id=?1", [&channel])?;
        db.execute("DELETE FROM channels WHERE id=?1", [channel])?;
    }
    Ok(())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CreateConversation {
    name: String,
    kind: String,
    #[serde(default)]
    parent_id: Option<String>,
    #[serde(default)]
    member_ids: Vec<String>,
}
pub(super) async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<CreateConversation>,
) -> ApiResult<Json<ConversationInfo>> {
    let auth = state.auth(&headers)?;
    if input.name.trim().is_empty()
        || input.name.chars().count() > 64
        || input.name.chars().any(char::is_control)
        || !matches!(
            input.kind.as_str(),
            "group" | "thread" | "forum" | "announcement" | "stage"
        )
    {
        return Err(bad(
            "Choose a supported conversation type and a name up to 64 characters",
        ));
    }
    let result = {
        let mut db = lock(&state.0.db);
        let tx = db.transaction()?;
        active_user(&tx, &auth.user.id)?;
        let count: u32 = tx.query_row(
            "SELECT COUNT(*) FROM conversations WHERE owner_id=?1",
            [&auth.user.id],
            |r| r.get(0),
        )?;
        if count >= 200 {
            return Err(bad("You can own up to 200 conversations"));
        }
        if input.kind == "group" {
            if input.parent_id.is_some()
                || input.member_ids.is_empty()
                || input.member_ids.len() > 9
            {
                return Err(bad("Choose between 1 and 9 other group members"));
            }
            let mut unique = HashSet::new();
            for peer in &input.member_ids {
                social::direct_access(&tx, &auth.user.id, peer)?;
                if !unique.insert(peer) {
                    return Err(bad("Duplicate group member"));
                }
                let accepted:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM friendships WHERE accepted=1 AND ((sender=?1 AND recipient=?2) OR (sender=?2 AND recipient=?1)))",params![auth.user.id,peer],|r|r.get(0))?;
                if !accepted {
                    return Err(bad("Add a friend before inviting them into a group"));
                }
            }
        } else if input.kind == "thread" {
            let parent = input
                .parent_id
                .as_deref()
                .ok_or_else(|| bad("Choose a parent text or forum channel"))?;
            channel_access(&tx, parent, &auth.user.id, SEND)?;
            if get_channel(&tx, parent)?.kind != "text"
                || info(&tx, parent)?
                    .is_some_and(|c| matches!(c.kind.as_str(), "group" | "group_voice" | "thread"))
            {
                return Err(bad("Threads need a community text or forum channel"));
            }
            if !input.member_ids.is_empty() {
                return Err(bad("Threads inherit the parent channel's permissions"));
            }
        } else {
            roles::require(&tx, &auth.user.id, PERMISSION_CHANNELS)?;
            if input.parent_id.is_some() || !input.member_ids.is_empty() {
                return Err(bad(
                    "This channel does not have a parent or private member list",
                ));
            }
        }
        let channel = id();
        let kind = if input.kind == "stage" {
            "voice"
        } else {
            "text"
        };
        let visibility = if matches!(input.kind.as_str(), "group" | "thread") {
            "private"
        } else {
            "public"
        };
        tx.execute(
            "INSERT INTO channels(id,name,kind,topic,visibility) VALUES(?1,?2,?3,'',?4)",
            params![channel, input.name.trim(), kind, visibility],
        )?;
        tx.execute(
            "INSERT INTO conversations(channel_id,kind,owner_id,parent_id) VALUES(?1,?2,?3,?4)",
            params![channel, input.kind, auth.user.id, input.parent_id],
        )?;
        if input.kind == "group" {
            for member in input
                .member_ids
                .iter()
                .chain(std::iter::once(&auth.user.id))
            {
                tx.execute(
                    "INSERT INTO conversation_members VALUES(?1,?2)",
                    params![channel, member],
                )?;
            }
            let voice = id();
            tx.execute("INSERT INTO channels(id,name,kind,topic,visibility) VALUES(?1,?2,'voice','','private')",params![voice,format!("{} call",input.name.trim())])?;
            tx.execute("INSERT INTO conversations(channel_id,kind,owner_id,parent_id) VALUES(?1,'group_voice',?2,?3)",params![voice,auth.user.id,channel])?;
        }
        if input.kind == "stage" {
            tx.execute(
                "INSERT INTO stage_members(channel_id,user_id,speaker) VALUES(?1,?2,1)",
                params![channel, auth.user.id],
            )?;
        }
        voice_chat::ensure_rooms(&tx)?;
        if input.kind != "group" {
            roles::audit(&tx, &auth.user.id, "conversation.created", &channel)?;
        }
        let result = info(&tx, &channel)?.ok_or_else(missing)?;
        tx.commit()?;
        result
    };
    state.notify("channels", Some(result.channel_id.clone()), None, None);
    Ok(Json(result))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct EditConversation {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    archived: Option<bool>,
    #[serde(default)]
    member_ids: Option<Vec<String>>,
}
pub(super) async fn edit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(channel): Path<String>,
    Json(input): Json<EditConversation>,
) -> ApiResult<Json<ConversationInfo>> {
    let auth = state.auth(&headers)?;
    let result = {
        let mut db = lock(&state.0.db);
        let tx = db.transaction()?;
        channel_access(&tx, &channel, &auth.user.id, READ)?;
        let c = info(&tx, &channel)?.ok_or_else(missing)?;
        if c.kind == "group_voice" {
            return Err(bad("Edit the group instead of its call room"));
        }
        if auth.user.id != c.owner_id
            && (c.kind == "group" || !roles::has(&tx, &auth.user.id, PERMISSION_CHANNELS))
        {
            return Err(forbidden());
        }
        if let Some(name) = input.name {
            if name.trim().is_empty()
                || name.chars().count() > 64
                || name.chars().any(char::is_control)
            {
                return Err(bad("Invalid conversation name"));
            }
            tx.execute(
                "UPDATE channels SET name=?2 WHERE id=?1",
                params![channel, name.trim()],
            )?;
        }
        if let Some(archived) = input.archived {
            if c.kind != "thread" {
                return Err(bad("Only threads can be archived"));
            }
            tx.execute(
                "UPDATE conversations SET archived=?2 WHERE channel_id=?1",
                params![channel, archived],
            )?;
        }
        if let Some(members) = input.member_ids {
            if c.kind != "group"
                || members.is_empty()
                || members.len() > 10
                || !members.contains(&auth.user.id)
            {
                return Err(bad("A group has 2 to 10 members including its owner"));
            }
            let mut unique = HashSet::new();
            for member in &members {
                active_user(&tx, member)?;
                if !unique.insert(member) {
                    return Err(bad("Duplicate group member"));
                }
                if !c.member_ids.contains(member) {
                    social::direct_access(&tx, &auth.user.id, member)?;
                    let friends = social::relationships(&tx, &auth.user.id)?;
                    if !friends.friends.contains(member) {
                        return Err(bad("New group members must be friends"));
                    }
                }
            }
            if members.len() < 2 {
                return Err(bad("A group needs at least two members"));
            }
            tx.execute(
                "DELETE FROM conversation_members WHERE channel_id=?1",
                [&channel],
            )?;
            for member in members {
                tx.execute(
                    "INSERT INTO conversation_members VALUES(?1,?2)",
                    params![channel, member],
                )?;
            }
        }
        let result = info(&tx, &channel)?.ok_or_else(missing)?;
        tx.commit()?;
        result
    };
    state.enforce_voice_permissions()?;
    state.notify("channels", None, None, None);
    Ok(Json(result))
}
pub(super) async fn leave(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(channel): Path<String>,
) -> ApiResult<StatusCode> {
    let auth = state.auth(&headers)?;
    {
        let mut db = lock(&state.0.db);
        let tx = db.transaction()?;
        let c = info(&tx, &channel)?.ok_or_else(missing)?;
        if c.kind != "group" {
            return Err(bad("Only group members can leave a group"));
        }
        if !c.member_ids.contains(&auth.user.id) {
            return Err(forbidden());
        }
        tx.execute(
            "DELETE FROM conversation_members WHERE channel_id=?1 AND user_id=?2",
            params![channel, auth.user.id],
        )?;
        if c.owner_id == auth.user.id {
            if let Some(next) = c.member_ids.iter().find(|id| **id != auth.user.id) {
                tx.execute(
                    "UPDATE conversations SET owner_id=?2 WHERE channel_id=?1 OR parent_id=?1",
                    params![channel, next],
                )?;
            }
        }
        tx.commit()?;
    }
    state.enforce_voice_permissions()?;
    state.notify("channels", None, None, None);
    Ok(StatusCode::NO_CONTENT)
}
pub(super) fn stage_speaker(db: &Connection, channel: &str, user: &str) -> ApiResult<bool> {
    if info(db, channel)?.is_none_or(|c| c.kind != "stage") {
        return Ok(true);
    }
    Ok(roles::has(db,user,PERMISSION_MEMBERS)||db.query_row("SELECT EXISTS(SELECT 1 FROM stage_members WHERE channel_id=?1 AND user_id=?2 AND speaker=1)",params![channel,user],|r|r.get(0))?)
}
pub(super) fn stage_members(db: &Connection, user: &str) -> ApiResult<Vec<StageMember>> {
    let mut q = db.prepare(
        "SELECT channel_id,user_id,speaker,raised FROM stage_members ORDER BY channel_id,user_id",
    )?;
    let rows = q
        .query_map([], |r| {
            Ok(StageMember {
                channel_id: r.get(0)?,
                user_id: r.get(1)?,
                speaker: r.get(2)?,
                raised: r.get(3)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows
        .into_iter()
        .filter(|s| channel_access(db, &s.channel_id, user, READ).is_ok())
        .collect())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StageChange {
    user_id: String,
    #[serde(default)]
    speaker: Option<bool>,
    #[serde(default)]
    raised: Option<bool>,
}
pub(super) async fn stage_change(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(channel): Path<String>,
    Json(input): Json<StageChange>,
) -> ApiResult<Json<Value>> {
    let auth = state.auth(&headers)?;
    {
        let mut db = lock(&state.0.db);
        let tx = db.transaction()?;
        channel_access(&tx, &channel, &auth.user.id, JOIN)?;
        if info(&tx, &channel)?.is_none_or(|c| c.kind != "stage") {
            return Err(bad("Choose a stage channel"));
        }
        active_user(&tx, &input.user_id)?;
        if input.speaker.is_some() {
            roles::require(&tx, &auth.user.id, PERMISSION_MEMBERS)?;
        }
        if input.raised.is_some() && input.user_id != auth.user.id {
            roles::require(&tx, &auth.user.id, PERMISSION_MEMBERS)?;
        }
        tx.execute(
            "INSERT OR IGNORE INTO stage_members(channel_id,user_id) VALUES(?1,?2)",
            params![channel, input.user_id],
        )?;
        if let Some(speaker) = input.speaker {
            tx.execute(
                "UPDATE stage_members SET speaker=?3,raised=0 WHERE channel_id=?1 AND user_id=?2",
                params![channel, input.user_id, speaker],
            )?;
            roles::audit(&tx, &auth.user.id, "stage.speaker", &input.user_id)?;
        }
        if let Some(raised) = input.raised {
            tx.execute(
                "UPDATE stage_members SET raised=?3 WHERE channel_id=?1 AND user_id=?2",
                params![channel, input.user_id, raised],
            )?;
        }
        tx.commit()?;
    }
    state.enforce_voice_permissions()?;
    state.notify("voice", Some(channel), None, None);
    Ok(Json(json!({"ok":true})))
}
pub(super) async fn stage_state(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(channel): Path<String>,
) -> ApiResult<Json<Value>> {
    let auth = state.auth(&headers)?;
    let db = lock(&state.0.db);
    channel_access(&db, &channel, &auth.user.id, READ)?;
    if info(&db, &channel)?.is_none_or(|c| c.kind != "stage") {
        return Err(missing());
    }
    let mut q = db.prepare(
        "SELECT user_id,speaker,raised FROM stage_members WHERE channel_id=?1 ORDER BY user_id",
    )?;
    let result=q.query_map([channel],|r|Ok(json!({"user_id":r.get::<_,String>(0)?,"speaker":r.get::<_,bool>(1)?,"raised":r.get::<_,bool>(2)?})))?.collect::<Result<Vec<_>,_>>()?;
    Ok(Json(json!(result)))
}
pub(super) fn events(db: &Connection, user: &str) -> ApiResult<Vec<ScheduledEvent>> {
    let mut q=db.prepare("SELECT id,name,description,channel_id,starts_at,ends_at FROM scheduled_events ORDER BY starts_at LIMIT 500")?;
    let rows = q
        .query_map([], |r| {
            Ok(ScheduledEvent {
                id: r.get(0)?,
                name: r.get(1)?,
                description: r.get(2)?,
                channel_id: r.get(3)?,
                starts_at: r.get(4)?,
                ends_at: r.get(5)?,
                interested: vec![],
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut result = vec![];
    for mut event in rows {
        if channel_access(db, &event.channel_id, user, READ).is_err() {
            continue;
        }
        let mut q =
            db.prepare("SELECT user_id FROM event_interest WHERE event_id=?1 ORDER BY user_id")?;
        event.interested = q
            .query_map([&event.id], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        result.push(event);
    }
    Ok(result)
}
pub(super) async fn save_event(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut input): Json<ScheduledEvent>,
) -> ApiResult<Json<ScheduledEvent>> {
    let auth = state.auth(&headers)?;
    if input.name.trim().is_empty()
        || input.name.chars().count() > 100
        || input.description.chars().count() > 1000
        || input.starts_at <= now()
        || input.starts_at > now() + 365 * 86400
        || input.ends_at <= input.starts_at
        || input.ends_at > input.starts_at + 7 * 86400
    {
        return Err(bad(
            "Use a name, a future start within a year and an end within seven days",
        ));
    }
    {
        let mut db = lock(&state.0.db);
        let tx = db.transaction()?;
        roles::require(&tx, &auth.user.id, PERMISSION_CHANNELS)?;
        channel_access(&tx, &input.channel_id, &auth.user.id, READ)?;
        if info(&tx, &input.channel_id)?
            .is_some_and(|c| matches!(c.kind.as_str(), "group" | "group_voice"))
        {
            return Err(bad("Scheduled events need a community channel"));
        }
        if input.id.is_empty() {
            let count: u32 =
                tx.query_row("SELECT COUNT(*) FROM scheduled_events", [], |r| r.get(0))?;
            if count >= 500 {
                return Err(bad("This community has reached its 500-event limit"));
            }
            input.id = id();
        } else if !tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM scheduled_events WHERE id=?1)",
            [&input.id],
            |r| r.get::<_, bool>(0),
        )? {
            return Err(missing());
        }
        tx.execute("INSERT INTO scheduled_events VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(id) DO UPDATE SET name=excluded.name,description=excluded.description,channel_id=excluded.channel_id,starts_at=excluded.starts_at,ends_at=excluded.ends_at",params![input.id,input.name.trim(),input.description,input.channel_id,input.starts_at,input.ends_at])?;
        roles::audit(&tx, &auth.user.id, "event.saved", &input.id)?;
        tx.commit()?;
    }
    input.interested.clear();
    state.notify("channels", None, None, None);
    Ok(Json(input))
}
pub(super) async fn delete_event(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(event): Path<String>,
) -> ApiResult<StatusCode> {
    let auth = state.auth(&headers)?;
    {
        let mut db = lock(&state.0.db);
        let tx = db.transaction()?;
        roles::require(&tx, &auth.user.id, PERMISSION_CHANNELS)?;
        if tx.execute("DELETE FROM scheduled_events WHERE id=?1", [&event])? == 0 {
            return Err(missing());
        }
        roles::audit(&tx, &auth.user.id, "event.deleted", &event)?;
        tx.commit()?;
    }
    state.notify("channels", None, None, None);
    Ok(StatusCode::NO_CONTENT)
}
pub(super) async fn interest(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(event): Path<String>,
) -> ApiResult<StatusCode> {
    let auth = state.auth(&headers)?;
    let mut db = lock(&state.0.db);
    let tx = db.transaction()?;
    let channel: String = tx
        .query_row(
            "SELECT channel_id FROM scheduled_events WHERE id=?1",
            [&event],
            |r| r.get(0),
        )
        .optional()?
        .ok_or_else(missing)?;
    channel_access(&tx, &channel, &auth.user.id, READ)?;
    if tx.execute(
        "DELETE FROM event_interest WHERE event_id=?1 AND user_id=?2",
        params![event, auth.user.id],
    )? == 0
    {
        tx.execute(
            "INSERT INTO event_interest VALUES(?1,?2)",
            params![event, auth.user.id],
        )?;
    }
    tx.commit()?;
    drop(db);
    state.notify("channels", None, None, None);
    Ok(StatusCode::NO_CONTENT)
}
