//! Current role grants and hierarchy, shared by every authenticated operation.
use super::*;

pub(super) fn permissions(db: &Connection, user_id: &str) -> ApiResult<u32> {
    active_user(db, user_id)?;
    let base = match get_user(db, user_id)?.role.as_str() {
        "admin" => PERMISSION_ALL,
        "moderator" => 15 | PERMISSION_MESSAGES | PERMISSION_MEMBERS | PERMISSION_INVITES,
        _ => 15,
    };
    let mut query = db.prepare("SELECT r.permissions FROM community_roles r JOIN member_roles m ON m.role_id=r.id WHERE m.user_id=?1")?;
    let grants = query
        .query_map([user_id], |r| r.get::<_, u32>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(grants.into_iter().fold(base, |bits, grant| bits | grant))
}
pub(super) fn has(db: &Connection, user_id: &str, permission: u32) -> bool {
    permissions(db, user_id)
        .is_ok_and(|bits| bits & PERMISSION_ADMIN != 0 || bits & permission == permission)
}
pub(super) fn require(db: &Connection, user_id: &str, permission: u32) -> ApiResult<()> {
    if has(db, user_id, permission) {
        Ok(())
    } else {
        Err(forbidden())
    }
}
pub(super) fn rank(db: &Connection, user_id: &str) -> ApiResult<u32> {
    let user = get_user(db, user_id)?;
    if user.role == "admin" {
        return Ok(1002);
    }
    if user.role == "moderator" {
        return Ok(1001);
    }
    Ok(db.query_row("SELECT COALESCE(MAX(r.position),0) FROM community_roles r JOIN member_roles m ON m.role_id=r.id WHERE m.user_id=?1", [user_id], |r| r.get(0))?)
}
pub(super) fn require_below(db: &Connection, actor: &str, target: &str) -> ApiResult<()> {
    if actor == target || rank(db, actor)? <= rank(db, target)? {
        return Err(forbidden());
    }
    Ok(())
}
pub(super) fn audit(db: &Connection, actor: &str, action: &str, target: &str) -> ApiResult<()> {
    db.execute(
        "INSERT INTO audit_log(actor_id,action,target_id,created_at) VALUES(?1,?2,?3,?4)",
        params![actor, action, target, chrono::Utc::now().to_rfc3339()],
    )?;
    // Keep metadata bounded; conversation bodies and decrypted evidence are never logged here.
    db.execute("DELETE FROM audit_log WHERE sequence <= (SELECT COALESCE(MAX(sequence),0)-10000 FROM audit_log)", [])?;
    Ok(())
}
fn row_role(row: &rusqlite::Row<'_>) -> rusqlite::Result<CommunityRole> {
    Ok(CommunityRole {
        id: row.get(0)?,
        name: row.get(1)?,
        color: row.get(2)?,
        position: row.get(3)?,
        permissions: row.get(4)?,
    })
}
fn get_role(db: &Connection, id: &str) -> ApiResult<CommunityRole> {
    db.query_row(
        "SELECT id,name,color,position,permissions FROM community_roles WHERE id=?1",
        [id],
        row_role,
    )
    .optional()?
    .ok_or_else(missing)
}
pub(super) fn list(db: &Connection) -> ApiResult<Vec<CommunityRole>> {
    let mut query = db.prepare(
        "SELECT id,name,color,position,permissions FROM community_roles ORDER BY position DESC,id",
    )?;
    let result = query
        .query_map([], row_role)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(result)
}
pub(super) fn assignments(db: &Connection) -> ApiResult<Vec<MemberRoles>> {
    let mut query=db.prepare("SELECT m.user_id,m.role_id FROM member_roles m JOIN membership u ON u.user_id=m.user_id WHERE u.status='active' ORDER BY m.user_id,m.role_id")?;
    let rows = query
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut result: Vec<MemberRoles> = vec![];
    for (user, role) in rows {
        if let Some(entry) = result.last_mut().filter(|entry| entry.user_id == user) {
            entry.role_ids.push(role);
        } else {
            result.push(MemberRoles {
                user_id: user,
                role_ids: vec![role],
            });
        }
    }
    Ok(result)
}
fn editable(db: &Connection, actor: &str, role: &CommunityRole) -> ApiResult<()> {
    require(db, actor, PERMISSION_ROLES)?;
    let own = permissions(db, actor)?;
    if role.position >= rank(db, actor)?
        || (own & PERMISSION_ADMIN == 0 && role.permissions & !own != 0)
    {
        return Err(forbidden());
    }
    Ok(())
}
pub(super) async fn get_roles(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Vec<CommunityRole>>> {
    state.auth(&headers)?;
    Ok(Json(list(&lock(&state.0.db))?))
}
pub(super) async fn create_role(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut input): Json<CommunityRole>,
) -> ApiResult<Json<CommunityRole>> {
    let auth = state.auth(&headers)?;
    if !input.valid() {
        return Err(bad("Use a role name up to 64 characters, a valid color, hierarchy position and supported permissions"));
    }
    input.id = id();
    input.name = input.name.trim().into();
    {
        let mut db = lock(&state.0.db);
        let tx = db.transaction()?;
        editable(&tx, &auth.user.id, &input)?;
        let count: u32 = tx.query_row("SELECT COUNT(*) FROM community_roles", [], |r| r.get(0))?;
        if count >= 128 {
            return Err(bad("This community has reached its 128-role limit"));
        }
        tx.execute(
            "INSERT INTO community_roles VALUES(?1,?2,?3,?4,?5)",
            params![
                input.id,
                input.name,
                input.color,
                input.position,
                input.permissions
            ],
        )?;
        audit(&tx, &auth.user.id, "role.created", &input.id)?;
        tx.commit()?;
    }
    state.notify("members", None, None, None);
    Ok(Json(input))
}
pub(super) async fn edit_role(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(mut input): Json<CommunityRole>,
) -> ApiResult<Json<CommunityRole>> {
    let auth = state.auth(&headers)?;
    if !input.valid() {
        return Err(bad("Invalid role"));
    }
    input.id = id.clone();
    input.name = input.name.trim().into();
    {
        let mut db = lock(&state.0.db);
        let tx = db.transaction()?;
        editable(&tx, &auth.user.id, &get_role(&tx, &id)?)?;
        editable(&tx, &auth.user.id, &input)?;
        tx.execute(
            "UPDATE community_roles SET name=?2,color=?3,position=?4,permissions=?5 WHERE id=?1",
            params![
                id,
                input.name,
                input.color,
                input.position,
                input.permissions
            ],
        )?;
        audit(&tx, &auth.user.id, "role.updated", &id)?;
        tx.commit()?;
    }
    state.enforce_voice_permissions()?;
    state.notify("members", None, None, None);
    state.notify("channels", None, None, None);
    Ok(Json(input))
}
pub(super) async fn delete_role(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let auth = state.auth(&headers)?;
    {
        let mut db = lock(&state.0.db);
        let tx = db.transaction()?;
        editable(&tx, &auth.user.id, &get_role(&tx, &id)?)?;
        tx.execute(
            "DELETE FROM channel_permissions WHERE subject_kind='role' AND subject_id=?1",
            [&id],
        )?;
        tx.execute("DELETE FROM community_roles WHERE id=?1", [&id])?;
        audit(&tx, &auth.user.id, "role.deleted", &id)?;
        tx.commit()?;
    }
    state.enforce_voice_permissions()?;
    state.notify("members", None, None, None);
    state.notify("channels", None, None, None);
    Ok(StatusCode::NO_CONTENT)
}
pub(super) async fn set_member_roles(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(user_id): Path<String>,
    Json(input): Json<MemberRoles>,
) -> ApiResult<Json<MemberRoles>> {
    let auth = state.auth(&headers)?;
    if input.user_id != user_id || input.role_ids.len() > 128 {
        return Err(bad("Invalid member role assignment"));
    }
    {
        let mut db = lock(&state.0.db);
        let tx = db.transaction()?;
        require(&tx, &auth.user.id, PERMISSION_ROLES)?;
        active_user(&tx, &user_id)?;
        // Built-in administrators may assign themselves; delegated managers never may.
        if get_user(&tx, &auth.user.id)?.role != "admin" {
            require_below(&tx, &auth.user.id, &user_id)?;
        }
        let old = {
            let mut q = tx.prepare("SELECT role_id FROM member_roles WHERE user_id=?1")?;
            let result = q
                .query_map([&user_id], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            result
        };
        for role_id in old.iter().chain(input.role_ids.iter()) {
            editable(&tx, &auth.user.id, &get_role(&tx, role_id)?)?;
        }
        let mut unique = std::collections::HashSet::new();
        if !input.role_ids.iter().all(|id| unique.insert(id)) {
            return Err(bad("Duplicate member role"));
        }
        tx.execute("DELETE FROM member_roles WHERE user_id=?1", [&user_id])?;
        for role in &input.role_ids {
            tx.execute(
                "INSERT INTO member_roles VALUES(?1,?2)",
                params![user_id, role],
            )?;
        }
        audit(&tx, &auth.user.id, "member.roles.updated", &user_id)?;
        tx.commit()?;
    }
    state.enforce_voice_permissions()?;
    state.notify("members", None, Some(user_id), None);
    state.notify("channels", None, None, None);
    Ok(Json(input))
}
pub(super) async fn audit_log(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Vec<AuditEntry>>> {
    let auth = state.auth(&headers)?;
    let db = lock(&state.0.db);
    require(&db, &auth.user.id, PERMISSION_MEMBERS)?;
    let mut query=db.prepare("SELECT sequence,actor_id,action,target_id,created_at FROM audit_log ORDER BY sequence DESC LIMIT 200")?;
    let result = query
        .query_map([], |r| {
            Ok(AuditEntry {
                sequence: r.get(0)?,
                actor_id: r.get(1)?,
                action: r.get(2)?,
                target_id: r.get(3)?,
                created_at: r.get(4)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Json(result))
}

#[cfg(test)]
mod tests;
