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
    // Friends, requests and blocks, plus the people in a group this account is
    // in: they are needed to show who is writing, and nothing more.
    let together = co_members(db, viewer);
    b.members
        .retain(|u| visible(db, viewer, &u.id) || together.contains(&u.id));
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
                | "hosting-v1"
                | "gateway-broker-v1"
                | "replica-pull-v1"
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
    b.capabilities
        .push(relay_protocol::PEOPLE_GROUPS_CAPABILITY.into());
    b.capabilities
        .push(relay_protocol::PEOPLE_GROUP_INVITES_CAPABILITY.into());
    b.capabilities
        .push(relay_protocol::PEOPLE_GROUP_PRESENCE_CAPABILITY.into());
    // Separate from `presence`, which stays friends and self only. A failed read
    // leaves the list empty: nobody is shown rather than someone wrongly.
    b.group_presence = group_presence::rows(db, viewer).unwrap_or_default();
    b.capabilities
        .push(relay_protocol::game_activity::CAPABILITY.into());
    // Call-scoped voice gateways for the friends of a live private call. The community's
    // own `gateway-broker-v1` stays stripped above: no role or host consent is involved.
    b.capabilities
        .push(gateway_admission::FRIENDS_CAPABILITY.into());
}

/// A group call room is a pair's own room; it keeps the friend-pair rule.
pub(super) fn pair_room(db: &Connection, channel: &str) -> bool {
    db.query_row(
        "SELECT EXISTS(SELECT 1 FROM conversations WHERE parent_id=?1 AND kind='group_voice')",
        [channel],
        |r| r.get(0),
    )
    .unwrap_or(true)
}

/// Who may use a conversation in the Friends service. An ordinary group is for
/// its members, who joined by accepting an invitation; they need not be friends
/// with each other, and being together gives them no other access to each
/// other. A pair's call room still needs the two to be friends.
pub(super) fn group_access(db: &Connection, group: &ConversationInfo, user: &str) -> bool {
    group.member_ids.len() <= relay_protocol::PEOPLE_GROUP_MAX_MEMBERS
        && group.member_ids.iter().any(|member| member == user)
        && (!pair_room(db, &group.channel_id)
            || group
                .member_ids
                .iter()
                .filter(|member| *member != user)
                .all(|member| friend(db, user, member)))
}

/// The other active members of ordinary groups this account is in.
pub(super) fn co_members(db: &Connection, viewer: &str) -> std::collections::HashSet<String> {
    let mut found = std::collections::HashSet::new();
    let Ok(mut query) = db.prepare(
        "SELECT DISTINCT b.user_id FROM conversation_members a JOIN conversation_members b ON a.channel_id=b.channel_id JOIN conversations c ON c.channel_id=a.channel_id JOIN membership m ON m.user_id=b.user_id WHERE a.user_id=?1 AND b.user_id<>?1 AND c.kind='group' AND m.status='active' AND NOT EXISTS(SELECT 1 FROM conversations v WHERE v.parent_id=a.channel_id AND v.kind='group_voice')",
    ) else {
        return found;
    };
    if let Ok(rows) = query.query_map([viewer], |r| r.get::<_, String>(0)) {
        found.extend(rows.flatten());
    }
    found
}

/// Both are active in one ordinary group: the only thing that lets a person
/// see another's profile picture and name without being their friend.
pub(super) fn group_visible(db: &Connection, viewer: &str, peer: &str) -> bool {
    db.query_row(
        "SELECT EXISTS(SELECT 1 FROM conversation_members a JOIN conversation_members b ON a.channel_id=b.channel_id JOIN conversations c ON c.channel_id=a.channel_id JOIN membership m ON m.user_id=b.user_id WHERE a.user_id=?1 AND b.user_id=?2 AND c.kind='group' AND m.status='active' AND NOT EXISTS(SELECT 1 FROM conversations v WHERE v.parent_id=a.channel_id AND v.kind='group_voice'))",
        params![viewer, peer],
        |r| r.get(0),
    )
    .unwrap_or(false)
}

/// Group messages/typing/key changes are scoped to current readers, not to a
/// friendship. A group never grants global presence, DM or unrelated events.
pub(super) fn event_visible(db: &Connection, viewer: &str, event: &Event) -> bool {
    let Some(peer) = event.user_id.as_deref() else {
        return true;
    };
    if visible(db, viewer, peer) {
        return true;
    }
    let Some(channel) = event.channel_id.as_deref() else {
        return false;
    };
    if !matches!(event.kind.as_str(), "messages" | "typing" | "pins" | "e2ee" | relay_protocol::GROUP_PRESENCE_EVENT) {
        return false;
    }
    conversations::info(db, channel)
        .ok()
        .flatten()
        .is_some_and(|group| {
            group.kind == "group"
                && group.member_ids.iter().any(|id| id == peer)
                && group_access(db, &group, viewer)
        })
}

/// The group routes the Friends service accepts, each with its own method:
/// create, edit/leave, invite and revoke, and accept/decline an invitation.
fn conversation_route(path: &str, method: &axum::http::Method) -> bool {
    let method = method.as_str();
    if path == "/api/conversations" {
        return method == "POST";
    }
    if let Some(rest) = path.strip_prefix("/api/conversations/") {
        let parts: Vec<&str> = rest.split('/').collect();
        return match parts.as_slice() {
            [id] => !id.is_empty() && (method == "PATCH" || method == "DELETE"),
            [id, "invitations"] => !id.is_empty() && method == "POST",
            [id, "invitations", user] => !id.is_empty() && !user.is_empty() && method == "DELETE",
            _ => false,
        };
    }
    if let Some(rest) = path.strip_prefix("/api/group-invitations/") {
        let parts: Vec<&str> = rest.split('/').collect();
        return method == "POST"
            && matches!(parts.as_slice(), [id, "accept" | "decline"] if !id.is_empty());
    }
    false
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
            | "/api/activity"
            | "/api/history"
            | "/api/changes"
            | "/api/read-state"
            | "/api/events"
            | "/api/messages"
            | "/api/typing"
            | "/api/uploads"
            | "/api/sessions"
            | "/api/private-calls"
            | "/api/reports"
            | "/api/categories"
            | "/api/profile"
            | "/api/rtc/token"
            | "/api/rtc/media"
            | "/api/rtc/authorize"
            | "/rtc"
            | "/rtc/v1"
            | "/rtc/validate"
            | "/rtc/v1/validate"
    ) || conversation_route(path, request.method())
        || host_locator_directory::is_route(path, request.method())
        || gateway_admission::friends_route(path, request.method())
        || [
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
        && (path == "/api/people/requests"
            || path == "/api/conversations"
            || path.starts_with("/api/friends/")
            || path.ends_with("/invitations")
            || path.starts_with("/api/group-invitations/"))
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
                "Too many invitations or friend requests. Try again shortly.",
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
