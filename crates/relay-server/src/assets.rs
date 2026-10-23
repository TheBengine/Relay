//! Bounded presentation images are separate from encrypted conversation attachments.
use super::*;
use serde_json::json;
#[cfg(test)]
mod tests;
fn valid(kind: &str, name: &str) -> bool {
    match kind {
        "server_icon" | "server_banner" => name == "community",
        "profile_banner" => {
            !name.is_empty()
                && name.len() <= 128
                && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
        }
        "emoji" | "sticker" => {
            (2..=32).contains(&name.len())
                && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
        }
        _ => false,
    }
}
fn require_edit(db: &Connection, auth: &Auth, kind: &str, name: &str) -> ApiResult<()> {
    active_user(db, &auth.user.id)?;
    if !valid(kind, name) {
        return Err(bad("Choose a supported image type and a valid name"));
    }
    if kind == "profile_banner" {
        if name != auth.user.id {
            return Err(forbidden());
        }
    } else {
        roles::require(
            db,
            &auth.user.id,
            if kind.starts_with("server_") {
                PERMISSION_ADMIN
            } else {
                PERMISSION_CHANNELS
            },
        )?;
    }
    Ok(())
}
fn row_asset(r: &rusqlite::Row<'_>) -> rusqlite::Result<MediaAsset> {
    let kind: String = r.get(1)?;
    let name: String = r.get(2)?;
    let version: u64 = r.get(4)?;
    Ok(MediaAsset {
        id: r.get(0)?,
        url: format!("/api/assets/{kind}/{name}?v={version}"),
        kind,
        name,
        owner_id: r.get(3)?,
        version,
    })
}
pub(super) fn list(db: &Connection) -> ApiResult<Vec<MediaAsset>> {
    let mut q=db.prepare("SELECT a.id,a.kind,a.name,a.owner_id,a.version FROM presentation_assets a WHERE a.kind!='profile_banner' OR EXISTS(SELECT 1 FROM membership m WHERE m.user_id=a.owner_id AND m.status='active') ORDER BY a.kind,a.name COLLATE NOCASE")?;
    let result = q.query_map([], row_asset)?.collect::<Result<_, _>>()?;
    Ok(result)
}
pub(super) async fn get_assets(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Vec<MediaAsset>>> {
    state.auth(&headers)?;
    Ok(Json(list(&lock(&state.0.db))?))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AssetUpload {
    image_base64: String,
    #[serde(default)]
    crop: Option<ImageCrop>,
}
pub(super) async fn upload(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((kind, name)): Path<(String, String)>,
    Json(input): Json<AssetUpload>,
) -> ApiResult<Json<MediaAsset>> {
    let auth = state.auth(&headers)?;
    require_edit(&lock(&state.0.db), &auth, &kind, &name)?;
    if input.image_base64.len() > profiles::MAX_AVATAR_UPLOAD_BYTES.div_ceil(3) * 4 {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Images can be up to 2 MiB",
        ));
    }
    let bytes = STANDARD
        .decode(&input.image_base64)
        .map_err(|_| bad("Invalid image encoding"))?;
    let _permit = state
        .0
        .avatar_jobs
        .clone()
        .try_acquire_owned()
        .map_err(|_| {
            ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "Image service is busy; retry shortly",
            )
        })?;
    let (width, height) = match kind.as_str() {
        "emoji" => (128, 128),
        "sticker" => (320, 320),
        "server_icon" => (256, 256),
        _ => (1200, 480),
    };
    let image = profiles::normalize_image(&bytes, width, height, input.crop.as_ref())?;
    let result = {
        let mut db = lock(&state.0.db);
        let tx = db.transaction()?;
        require_edit(&tx, &auth, &kind, &name)?;
        let previous:Option<(String,u64,u64,String)>=tx.query_row("SELECT id,version,length(data),owner_id FROM presentation_assets WHERE kind=?1 AND name=?2",params![kind,name],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
        let count: u32 = tx.query_row(
            "SELECT COUNT(*) FROM presentation_assets WHERE kind=?1",
            [&kind],
            |r| r.get(0),
        )?;
        if previous.is_none() && matches!(kind.as_str(), "emoji" | "sticker") && count >= 250 {
            return Err(bad("A community can have up to 250 images of this type"));
        }
        let (total, owned) = profiles::storage_usage(&tx, &auth.user.id)?;
        let old_size = previous.as_ref().map_or(0, |p| p.2);
        let own_old_size = previous
            .as_ref()
            .filter(|p| p.3 == auth.user.id)
            .map_or(0, |p| p.2);
        if total
            .saturating_sub(old_size)
            .saturating_add(image.len() as u64)
            > state.0.config.storage_limit_bytes
            || owned
                .saturating_sub(own_old_size)
                .saturating_add(image.len() as u64)
                > state.0.config.user_storage_limit_bytes
        {
            return Err(ApiError::new(
                StatusCode::INSUFFICIENT_STORAGE,
                "Image storage quota reached",
            ));
        }
        let asset_id = previous.as_ref().map(|p| p.0.clone()).unwrap_or_else(id);
        let version = previous.as_ref().map_or(Ok(1), |p| {
            p.1.checked_add(1)
                .ok_or_else(|| bad("Image version exhausted"))
        })?;
        tx.execute("INSERT INTO presentation_assets VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(kind,name) DO UPDATE SET owner_id=excluded.owner_id,version=excluded.version,data=excluded.data",params![asset_id,kind,name,auth.user.id,version,image])?;
        if kind != "profile_banner" {
            roles::audit(&tx, &auth.user.id, "asset.saved", &asset_id)?;
        }
        let result=tx.query_row("SELECT id,kind,name,owner_id,version FROM presentation_assets WHERE kind=?1 AND name=?2",params![kind,name],row_asset)?;
        tx.commit()?;
        result
    };
    state.notify("members", None, None, None);
    Ok(Json(result))
}
pub(super) async fn download(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((kind, name)): Path<(String, String)>,
) -> ApiResult<Response> {
    state.auth(&headers)?;
    if !valid(&kind, &name) {
        return Err(missing());
    }
    let db = lock(&state.0.db);
    if kind == "profile_banner" {
        active_user(&db, &name)?;
    }
    let bytes: Vec<u8> = db
        .query_row(
            "SELECT data FROM presentation_assets WHERE kind=?1 AND name=?2",
            params![kind, name],
            |r| r.get(0),
        )
        .optional()?
        .ok_or_else(missing)?;
    Ok((
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, "private, no-store"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        bytes,
    )
        .into_response())
}
pub(super) async fn remove(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((kind, name)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = state.auth(&headers)?;
    {
        let mut db = lock(&state.0.db);
        let tx = db.transaction()?;
        require_edit(&tx, &auth, &kind, &name)?;
        tx.execute(
            "DELETE FROM presentation_assets WHERE kind=?1 AND name=?2",
            params![kind, name],
        )?;
        if kind != "profile_banner" {
            roles::audit(&tx, &auth.user.id, "asset.removed", &name)?;
        }
        tx.commit()?;
    }
    state.notify("members", None, None, None);
    Ok(Json(json!({"ok":true})))
}
