//! Account-wide friends use a separate database, never an existing community.
//! Reuse the established private-message, MLS and call-admission paths.
use super::*;

pub(super) const CAPABILITY: &str = "people-v1";

pub(super) fn enabled(db: &Connection) -> bool {
    db.query_row(
        "SELECT value='people-v1' FROM metadata WHERE key='service_kind'",
        [],
        |r| r.get(0),
    )
    .unwrap_or(false)
}

pub(super) fn initialize(
    db: &Connection,
    config: &ServerConfig,
    existing: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let marked = enabled(db);
    if marked != config.people && (marked || existing && config.people) {
        return Err(
            "Friends must use their own new data directory; service mode cannot be changed".into(),
        );
    }
    if config.people {
        if config.account_provider.is_none() {
            return Err("Friends require a configured Relay account provider".into());
        }
        db.execute(
            "INSERT OR IGNORE INTO metadata VALUES('service_kind','people-v1')",
            [],
        )?;
    }
    Ok(())
}

/// Only self and people with an explicit relationship may appear in a snapshot.
/// A block never reveals presence, keys or somebody else's relationships.
pub(super) fn visible(db: &Connection, viewer: &str, peer: &str) -> bool {
    viewer == peer || db.query_row(
        "SELECT EXISTS(SELECT 1 FROM friendships WHERE (sender=?1 AND recipient=?2) OR (sender=?2 AND recipient=?1)) OR EXISTS(SELECT 1 FROM user_blocks WHERE owner=?1 AND peer=?2)",
        params![viewer, peer], |r| r.get::<_, bool>(0),
    ).unwrap_or(false)
}

pub(super) fn friend(db: &Connection, a: &str, b: &str) -> bool {
    db.query_row("SELECT EXISTS(SELECT 1 FROM friendships WHERE accepted=1 AND ((sender=?1 AND recipient=?2) OR (sender=?2 AND recipient=?1)))", params![a,b], |r| r.get(0)).unwrap_or(false)
        && !social::blocked(db, a, b).unwrap_or(true)
}

pub(super) fn snapshot(db: &Connection, viewer: &str, b: &mut Bootstrap) {
    b.members.retain(|u| visible(db, viewer, &u.id));
    b.linked_identities
        .retain(|u| visible(db, viewer, &u.user_id));
    b.presence
        .retain(|u| u.user_id == viewer || friend(db, viewer, &u.user_id));
    b.member_roles.clear();
    b.roles.clear();
    b.permissions = 0;
    b.capabilities.retain(|c| {
        !matches!(
            c.as_str(),
            "backups"
                | "server-settings-v1"
                | "discord-command-v1"
                | "discord-host-credentials-v1"
                | "discord-channel-status-v1"
                | "welcome-v1"
                | "custom-roles-v1"
                | "authenticator-v1"
                | "account-link-v1"
                | "assets-v1"
                | "protection-modes-v1"
                | "protection-calls-v1"
        )
    });
    b.capabilities.push(CAPABILITY.into());
}

/// Explicit allowlist: no community management, invitations, password login,
/// group creation, protection downgrade or public member directory in this mode.
pub(super) async fn guard(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    if !state.0.config.people {
        return next.run(request).await;
    }
    let path = request.uri().path();
    if path == "/api/local/shutdown" {
        // Managed Friends processes need the same owner stop path as ordinary
        // servers. Never admit it through a peer/proxy: the handler then checks
        // the private process token and exact current server/instance itself.
        if request.extensions().get::<NetworkIngress>().is_some()
            || !request
                .extensions()
                .get::<ConnectInfo<SocketAddr>>()
                .is_some_and(|peer| peer.0.ip().is_loopback())
            || [
                "forwarded",
                "x-forwarded-for",
                "x-real-ip",
                "x-forwarded-host",
                "x-forwarded-proto",
            ]
            .iter()
            .any(|name| request.headers().contains_key(*name))
        {
            return forbidden().into_response();
        }
        return next.run(request).await;
    }
    let allowed = matches!(
        path,
        "/health"
            | "/api/health"
            | "/api/account-login"
            | "/api/account-login/challenge"
            | "/api/logout"
            | "/api/bootstrap"
            | "/api/social"
            | "/api/people/requests"
            | "/api/presence"
            | "/api/history"
            | "/api/changes"
            | "/api/read-state"
            | "/api/events"
            | "/api/messages"
            | "/api/uploads"
            | "/api/sessions"
            | "/api/private-calls"
            | "/api/reports"
            | "/api/categories"
            | "/api/profile"
            | "/api/rtc/token"
            | "/api/rtc/authorize"
            | "/rtc"
            | "/rtc/v1"
            | "/rtc/validate"
            | "/rtc/v1/validate"
    ) || [
        "/api/friends/",
        "/api/blocks/",
        "/api/private-calls/",
        "/api/e2ee/",
        "/api/profiles/",
        "/api/messages/",
        "/api/uploads/",
        "/api/voice/",
        "/api/media/",
        "/api/sessions/",
    ]
    .iter()
    .any(|prefix| path.starts_with(prefix));
    if !allowed {
        return forbidden().into_response();
    }
    // Bound exact-name probes and UUID requests together, including failed probes.
    if request.method() == "POST"
        && (path == "/api/people/requests" || path.starts_with("/api/friends/"))
    {
        let auth = match state.auth(request.headers()) {
            Ok(a) => a,
            Err(e) => return e.into_response(),
        };
        let mut rates = lock(&state.0.mutation_rates);
        let timestamp = now_ms();
        rates.retain(|_, (start, _)| timestamp.saturating_sub(*start) < 60_000);
        let key = format!("people-request:{}", auth.user.id);
        if rates.len() >= 10000 && !rates.contains_key(&key) {
            return forbidden().into_response();
        }
        let count = rates.entry(key).or_insert((timestamp, 0));
        if count.1 >= 20 {
            return ApiError::cooldown(
                "friend_request_limit",
                "Too many friend requests. Try again shortly.",
                count
                    .0
                    .saturating_add(60_000)
                    .saturating_sub(timestamp)
                    .max(1),
                None,
            )
            .into_response();
        }
        count.1 += 1;
    }
    next.run(request).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Request {
    handle: String,
}

pub(super) async fn request(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<Request>,
) -> ApiResult<Json<SocialState>> {
    if !state.0.config.people {
        return Err(missing());
    }
    let auth = state.auth(&headers)?;
    let handle = input
        .handle
        .trim()
        .trim_start_matches('@')
        .to_ascii_lowercase();
    if !(3..=24).contains(&handle.len())
        || !handle
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        return Err(bad(
            "Enter your friend's exact Relay name, for example @alex.",
        ));
    }
    let peer: Option<String> = lock(&state.0.db).query_row(
        "SELECT l.user_id FROM linked_identities l JOIN membership m ON m.user_id=l.user_id WHERE l.issuer=?1 AND l.handle=?2 AND m.status='active'",
        params![state.0.config.account_provider.as_ref().ok_or_else(forbidden)?.public().issuer, handle], |r| r.get(0),
    ).optional()?;
    let peer = peer.filter(|p| p != &auth.user.id).ok_or_else(|| bad("Could not send this request. Check the name and ask your friend to open Friends in Relay first."))?;
    social::friend_request(State(state), headers, Path(peer)).await
}

#[cfg(test)]
mod tests;
