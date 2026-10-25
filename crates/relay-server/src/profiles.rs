//! Community profiles and purpose-specific, authenticated avatar storage.
use super::*;
use image::{DynamicImage, ImageDecoder, ImageFormat, Limits};
use std::io::Cursor;

pub const MAX_AVATAR_UPLOAD_BYTES: usize = 2 * 1024 * 1024;
const MAX_AVATAR_DIMENSION: u32 = 2048;
const MAX_AVATAR_DECODE_BYTES: u64 = 32 * 1024 * 1024;
const AVATAR_DIMENSION: u32 = 256;

pub fn read_profile(
    row: &rusqlite::Row<'_>,
    user_column: usize,
    first: usize,
) -> rusqlite::Result<UserProfile> {
    let user_id: String = row.get(user_column)?;
    let version = row.get::<_, Option<u64>>(first + 2)?.unwrap_or(0);
    Ok(UserProfile {
        appearance: serde_json::from_str(
            &row.get::<_, Option<String>>(first + 4)?
                .unwrap_or_else(|| "{}".into()),
        )
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                first + 4,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
        display_name: row.get::<_, Option<String>>(first)?.unwrap_or_default(),
        bio: row.get::<_, Option<String>>(first + 1)?.unwrap_or_default(),
        version,
        avatar_url: row
            .get::<_, bool>(first + 3)?
            .then(|| format!("/api/profiles/{user_id}/avatar?v={version}")),
    })
}

/// Avatars count against the same durable quotas as attachments. Replacement
/// subtracts the previous avatar in the committing transaction.
pub fn storage_usage(db: &Connection, user: &str) -> rusqlite::Result<(u64, u64)> {
    let (total, owned): (u64,u64) = db.query_row(
        "SELECT (SELECT COALESCE(SUM(size),0) FROM uploads)+(SELECT COALESCE(SUM(length(avatar)),0) FROM user_profiles),
         (SELECT COALESCE(SUM(size),0) FROM uploads WHERE owner_id=?1)+COALESCE((SELECT length(avatar) FROM user_profiles WHERE user_id=?1),0)",
        [user],
        |row| Ok((row.get(0)?, row.get::<_, Option<u64>>(1)?.unwrap_or(0))),
    )?;
    let (crypto_total, crypto_owned) = e2ee::extra_storage_usage(db, user)?;
    let (assets_total,assets_owned):(u64,u64)=db.query_row("SELECT COALESCE(SUM(length(data)),0),COALESCE(SUM(CASE WHEN owner_id=?1 THEN length(data) ELSE 0 END),0) FROM presentation_assets",[user],|r|Ok((r.get(0)?,r.get(1)?)))?;
    Ok((
        total
            .saturating_add(crypto_total)
            .saturating_add(assets_total),
        owned
            .saturating_add(crypto_owned)
            .saturating_add(assets_owned),
    ))
}

pub async fn own_profile(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<User>> {
    Ok(Json(state.auth(&headers)?.user))
}

pub async fn member_profile(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(user_id): Path<String>,
) -> ApiResult<Json<User>> {
    let auth = state.auth(&headers)?;
    let db = lock(&state.0.db);
    if state.0.config.people
        && !people::visible(&db, &auth.user.id, &user_id)
        && !people::group_visible(&db, &auth.user.id, &user_id)
    {
        return Err(forbidden());
    }
    active_user(&db, &user_id)?;
    Ok(Json(get_user(&db, &user_id)?))
}

fn valid_profile(input: &UpdateProfile) -> bool {
    let forbidden_format = |c: char| matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}');
    input
        .appearance
        .as_ref()
        .is_none_or(ProfileAppearance::valid)
        && input.display_name.chars().count() <= 32
        && input.bio.chars().count() <= 190
        && !input
            .display_name
            .chars()
            .any(|c| c.is_control() || forbidden_format(c))
        && !input
            .bio
            .chars()
            .any(|c| (c.is_control() && c != '\n') || forbidden_format(c))
}

fn publish_profile(state: &AppState, user: &User) {
    // Voice and legacy JPEG metadata hold snapshots; replace only presentation.
    for session in lock(&state.0.voice).values_mut() {
        if session.user.id == user.id {
            session.user.profile = user.profile.clone();
        }
    }
    for frame in lock(&state.0.frames).values_mut() {
        if frame.metadata.user.id == user.id {
            frame.metadata.user.profile = user.profile.clone();
        }
    }
    state.notify("profile", None, Some(user.id.clone()), None);
    // Older clients already understand this bootstrap invalidation.
    state.notify("members", None, Some(user.id.clone()), None);
}

pub async fn update_profile(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut input): Json<UpdateProfile>,
) -> ApiResult<Json<User>> {
    let auth = state.auth(&headers)?;
    if !valid_profile(&input) {
        return Err(bad("Display name must be at most 32 characters and About Me at most 190; unsupported control characters are not allowed"));
    }
    input.display_name = input.display_name.trim().to_owned();
    let (user, changed) = {
        let mut db = lock(&state.0.db);
        let tx = db.transaction()?;
        tx.execute(
            "INSERT OR IGNORE INTO user_profiles(user_id) VALUES(?1)",
            [&auth.user.id],
        )?;
        let previous = get_user(&tx, &auth.user.id)?;
        let mut appearance = input
            .appearance
            .unwrap_or(previous.profile.appearance.clone());
        if appearance.effect.is_none() {
            appearance.effect = previous.profile.appearance.effect.clone();
        }
        let changed = previous.profile.display_name != input.display_name
            || previous.profile.bio != input.bio
            || previous.profile.appearance != appearance;
        if changed {
            next_version(previous.profile.version)?;
            tx.execute("UPDATE user_profiles SET display_name=?2,bio=?3,appearance=?4,version=version+1 WHERE user_id=?1", params![auth.user.id,input.display_name,input.bio,serde_json::to_string(&appearance).map_err(|_| bad("Invalid profile colors"))?])?;
        }
        let user = get_user(&tx, &auth.user.id)?;
        tx.commit()?;
        (user, changed)
    };
    if changed {
        publish_profile(&state, &user);
    }
    Ok(Json(user))
}

fn next_version(version: u64) -> ApiResult<()> {
    if version >= i64::MAX as u64 {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "Profile version limit reached",
        ));
    }
    Ok(())
}

fn image_limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_AVATAR_DIMENSION);
    limits.max_image_height = Some(MAX_AVATAR_DIMENSION);
    limits.max_alloc = Some(MAX_AVATAR_DECODE_BYTES);
    limits
}

fn bounded_decode(decoder: impl ImageDecoder) -> ApiResult<DynamicImage> {
    let (width, height) = decoder.dimensions();
    if width == 0
        || height == 0
        || width > MAX_AVATAR_DIMENSION
        || height > MAX_AVATAR_DIMENSION
        || decoder.total_bytes() > MAX_AVATAR_DECODE_BYTES
    {
        return Err(bad("Avatar image dimensions must be at most 2048 by 2048"));
    }
    DynamicImage::from_decoder(decoder).map_err(|_| bad("Cannot decode avatar image"))
}

fn normalize_avatar(bytes: &[u8]) -> ApiResult<Vec<u8>> {
    normalize_image(bytes, AVATAR_DIMENSION, AVATAR_DIMENSION, None)
}
pub(super) fn normalize_image(
    bytes: &[u8],
    max_width: u32,
    max_height: u32,
    crop: Option<&ImageCrop>,
) -> ApiResult<Vec<u8>> {
    if bytes.is_empty() || bytes.len() > MAX_AVATAR_UPLOAD_BYTES {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Avatar files must be nonempty and at most 2 MiB",
        ));
    }
    let format =
        image::guess_format(bytes).map_err(|_| bad("Choose a static PNG, JPEG or WebP avatar"))?;
    let image = match format {
        ImageFormat::Png => {
            let decoder =
                image::codecs::png::PngDecoder::with_limits(Cursor::new(bytes), image_limits())
                    .map_err(|_| bad("Invalid or oversized PNG avatar"))?;
            if decoder.is_apng().map_err(|_| bad("Invalid PNG avatar"))? {
                return Err(bad("Animated avatars are not supported"));
            }
            bounded_decode(decoder)?
        }
        ImageFormat::WebP => {
            let mut decoder = image::codecs::webp::WebPDecoder::new(Cursor::new(bytes))
                .map_err(|_| bad("Invalid WebP avatar"))?;
            if decoder.has_animation() {
                return Err(bad("Animated avatars are not supported"));
            }
            decoder
                .set_limits(image_limits())
                .map_err(|_| bad("Oversized WebP avatar"))?;
            bounded_decode(decoder)?
        }
        ImageFormat::Jpeg => {
            let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format);
            reader.limits(image_limits());
            bounded_decode(
                reader
                    .into_decoder()
                    .map_err(|_| bad("Invalid or oversized JPEG avatar"))?,
            )?
        }
        _ => return Err(bad("Choose a static PNG, JPEG or WebP avatar")),
    };
    let image = if let Some(crop) = crop {
        if !crop.valid() {
            return Err(bad("Invalid crop rectangle"));
        }
        let x = (crop.x * image.width() as f32).floor() as u32;
        let y = (crop.y * image.height() as f32).floor() as u32;
        let width = ((crop.width * image.width() as f32).round() as u32).max(1);
        let height = ((crop.height * image.height() as f32).round() as u32).max(1);
        if x >= image.width() || y >= image.height() {
            return Err(bad("Crop is outside the image"));
        }
        image.crop_imm(
            x,
            y,
            width.min(image.width() - x),
            height.min(image.height() - y),
        )
    } else {
        image
    };
    let normalized = if image.width() > max_width || image.height() > max_height {
        image.thumbnail(max_width, max_height).to_rgba8()
    } else {
        image.to_rgba8()
    };
    let mut output = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(normalized)
        .write_to(&mut output, ImageFormat::Png)
        .map_err(|_| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "Cannot normalize avatar"))?;
    Ok(output.into_inner())
}

pub async fn upload_avatar(
    State(state): State<AppState>,
    headers: HeaderMap,
    bytes: Bytes,
) -> ApiResult<Json<User>> {
    let auth = state.auth(&headers)?;
    // Dispatch already runs on a bounded blocking worker; no image work on IO threads.
    let _permit = state
        .0
        .avatar_jobs
        .clone()
        .try_acquire_owned()
        .map_err(|_| {
            ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "Avatar service is busy. Try again shortly",
            )
        })?;
    let normalized = normalize_avatar(&bytes)?;
    let (user, changed) = {
        let mut db = lock(&state.0.db);
        let tx = db.transaction()?;
        tx.execute(
            "INSERT OR IGNORE INTO user_profiles(user_id) VALUES(?1)",
            [&auth.user.id],
        )?;
        let previous: Option<Vec<u8>> = tx.query_row(
            "SELECT avatar FROM user_profiles WHERE user_id=?1",
            [&auth.user.id],
            |r| r.get(0),
        )?;
        let changed = previous.as_deref() != Some(normalized.as_slice());
        if changed {
            let (total, owned) = storage_usage(&tx, &auth.user.id)?;
            let previous_size = previous.as_ref().map_or(0, |bytes| bytes.len() as u64);
            let size = normalized.len() as u64;
            if total.saturating_sub(previous_size).saturating_add(size)
                > state.0.config.storage_limit_bytes
                || owned.saturating_sub(previous_size).saturating_add(size)
                    > state.0.config.user_storage_limit_bytes
            {
                return Err(ApiError::new(
                    StatusCode::INSUFFICIENT_STORAGE,
                    "Avatar storage quota reached; remove files or ask the host to raise the limit",
                ));
            }
            next_version(get_user(&tx, &auth.user.id)?.profile.version)?;
            tx.execute(
                "UPDATE user_profiles SET avatar=?2,version=version+1 WHERE user_id=?1",
                params![auth.user.id, normalized],
            )?;
        }
        let user = get_user(&tx, &auth.user.id)?;
        tx.commit()?;
        (user, changed)
    };
    if changed {
        publish_profile(&state, &user);
    }
    Ok(Json(user))
}

pub async fn remove_avatar(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<User>> {
    let auth = state.auth(&headers)?;
    let (user, changed) = {
        let mut db = lock(&state.0.db);
        let tx = db.transaction()?;
        let previous = get_user(&tx, &auth.user.id)?;
        let changed = previous.profile.avatar_url.is_some();
        if changed {
            next_version(previous.profile.version)?;
            tx.execute(
                "UPDATE user_profiles SET avatar=NULL,version=version+1 WHERE user_id=?1",
                [&auth.user.id],
            )?;
        }
        let user = get_user(&tx, &auth.user.id)?;
        tx.commit()?;
        (user, changed)
    };
    if changed {
        publish_profile(&state, &user);
    }
    Ok(Json(user))
}

#[derive(Deserialize)]
pub struct AvatarQuery {
    v: Option<u64>,
}

pub async fn download_avatar(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(user_id): Path<String>,
    Query(query): Query<AvatarQuery>,
) -> ApiResult<Response> {
    let auth = state.auth(&headers)?;
    let db = lock(&state.0.db);
    if state.0.config.people
        && !people::visible(&db, &auth.user.id, &user_id)
        && !people::group_visible(&db, &auth.user.id, &user_id)
    {
        return Err(forbidden());
    }
    active_user(&db, &user_id)?;
    let (bytes, version): (Option<Vec<u8>>, u64) = db
        .query_row(
            "SELECT avatar,version FROM user_profiles WHERE user_id=?1",
            [&user_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
        .ok_or_else(missing)?;
    if query.v.is_some_and(|v| v != version) {
        return Err(missing());
    }
    let bytes = bytes.ok_or_else(missing)?;
    Ok((
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, "no-store"),
            (header::CONTENT_DISPOSITION, "inline"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        bytes,
    )
        .into_response())
}

#[cfg(test)]
mod tests;
