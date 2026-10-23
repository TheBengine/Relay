//! Atomic reorder keeps hidden rows and concurrent changes intact.
use super::*;
#[cfg(test)]
mod tests;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct MoveChannel {
    #[serde(default)]
    before_id: Option<String>,
    #[serde(default)]
    category_id: Option<String>,
}
pub(super) async fn channel(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(input): Json<MoveChannel>,
) -> ApiResult<StatusCode> {
    let auth = state.auth(&headers)?;
    {
        let mut db = lock(&state.0.db);
        let tx = db.transaction()?;
        roles::require(&tx, &auth.user.id, PERMISSION_CHANNELS)?;
        let channel = get_channel(&tx, &id)?;
        conversations::require_regular(&tx, &id)?;
        if channel.voice_parent_id.is_some() {
            return Err(bad("Move the parent voice channel instead"));
        }
        if let Some(category) = &input.category_id {
            if !tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM categories WHERE id=?1)",
                [category],
                |r| r.get::<_, bool>(0),
            )? {
                return Err(missing());
            }
        }
        if input.before_id.as_deref() == Some(&id) {
            return Ok(StatusCode::NO_CONTENT);
        }
        if let Some(before) = &input.before_id {
            let target = get_channel(&tx, before)?;
            conversations::require_regular(&tx, before)?;
            if target.voice_parent_id.is_some()
                || target.kind != channel.kind
                || target.category_id != input.category_id
            {
                return Err(bad(
                    "Drop onto a channel of the same type in the destination category",
                ));
            }
        }
        let mut q=tx.prepare("SELECT c.id FROM channels c WHERE c.kind=?1 AND c.category_id IS ?2 AND c.voice_parent_id IS NULL AND NOT EXISTS(SELECT 1 FROM conversations i WHERE i.channel_id=c.id AND i.kind IN('group','group_voice','thread')) ORDER BY c.position,c.rowid")?;
        let mut rows = q
            .query_map(params![channel.kind, input.category_id], |r| {
                r.get::<_, String>(0)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(q);
        rows.retain(|row| row != &id);
        let index = input
            .before_id
            .as_ref()
            .and_then(|before| rows.iter().position(|row| row == before))
            .unwrap_or(rows.len());
        rows.insert(index, id.clone());
        tx.execute(
            "UPDATE channels SET category_id=?2 WHERE id=?1",
            params![id, input.category_id],
        )?;
        for (position, row) in rows.into_iter().enumerate() {
            tx.execute(
                "UPDATE channels SET position=?2 WHERE id=?1",
                params![row, position as i64],
            )?;
        }
        roles::audit(&tx, &auth.user.id, "channel.moved", &id)?;
        tx.commit()?;
    }
    state.notify("channels", None, None, None);
    Ok(StatusCode::NO_CONTENT)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct MoveCategory {
    #[serde(default)]
    before_id: Option<String>,
}
pub(super) async fn category(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(input): Json<MoveCategory>,
) -> ApiResult<StatusCode> {
    let auth = state.auth(&headers)?;
    {
        let mut db = lock(&state.0.db);
        let tx = db.transaction()?;
        roles::require(&tx, &auth.user.id, PERMISSION_CHANNELS)?;
        let mut q = tx.prepare("SELECT id FROM categories ORDER BY position,rowid")?;
        let mut rows = q
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        drop(q);
        if !rows.contains(&id)
            || input
                .before_id
                .as_ref()
                .is_some_and(|before| !rows.contains(before))
        {
            return Err(missing());
        }
        if input.before_id.as_ref() == Some(&id) {
            return Ok(StatusCode::NO_CONTENT);
        }
        rows.retain(|row| row != &id);
        let index = input
            .before_id
            .as_ref()
            .and_then(|before| rows.iter().position(|row| row == before))
            .unwrap_or(rows.len());
        rows.insert(index, id.clone());
        for (position, row) in rows.into_iter().enumerate() {
            tx.execute(
                "UPDATE categories SET position=?2 WHERE id=?1",
                params![row, position as i64],
            )?;
        }
        roles::audit(&tx, &auth.user.id, "category.moved", &id)?;
        tx.commit()?;
    }
    state.notify("channels", None, None, None);
    Ok(StatusCode::NO_CONTENT)
}
