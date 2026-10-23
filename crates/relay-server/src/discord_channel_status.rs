//! One explicit, owned public status. No Discord message-list reads or cache.
use super::*;
use crate::discord_bridge::{public_content, same_binding, settings};
use relay_protocol::discord::{snowflake, DiscordBridgeSettings, DiscordBridgeView};
use serde::Serialize;
use serenity::all::{Channel, ChannelId, ChannelType, Http, MessageId};
use tokio_util::sync::CancellationToken;

#[cfg(test)]
mod tests;

const KEY: &str = "discord-channel-message-v1";
const INTERVAL: Duration = Duration::from_secs(15);
const DEBOUNCE: Duration = Duration::from_secs(1);

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u8,
    owner: String,
    application: u64,
    guild: u64,
    channel: u64,
    bot: u64,
    nonce: String,
    message: Option<u64>,
    uncertain: bool,
    missing: bool,
}

fn owner(state: &AppState) -> String {
    lock(&state.0.discord_credential).host_binding()
}
fn record(state: &AppState) -> ApiResult<Option<Record>> {
    let db = lock(&state.0.db);
    record_db(&db)
}
fn record_db(db: &Connection) -> ApiResult<Option<Record>> {
    let value: Option<String> = db
        .query_row("SELECT value FROM metadata WHERE key=?1", [KEY], |r| {
            r.get(0)
        })
        .optional()?;
    value
        .map(|v| {
            serde_json::from_str::<Record>(&v)
                .map_err(|_| bad("Discord status metadata is damaged"))
        })
        .transpose()
}
fn persist(state: &AppState, expected: Option<&Record>, value: Option<&Record>) -> ApiResult<()> {
    let db = lock(&state.0.db);
    persist_db(&db, expected, value)
}
fn persist_db(db: &Connection, expected: Option<&Record>, value: Option<&Record>) -> ApiResult<()> {
    let current = record_db(db)?;
    if current.as_ref() != expected {
        // Idempotent delete is safe, but never erase or replace a different
        // record committed by recovery/cleanup during an HTTP await.
        if current.is_none() && value.is_none() {
            return Ok(());
        }
        return Err(bad("The Discord status changed during this request"));
    }
    if let Some(value) = value {
        db.execute("INSERT INTO metadata(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![KEY,serde_json::to_string(value)?])?;
    } else {
        db.execute("DELETE FROM metadata WHERE key=?1", [KEY])?;
    }
    Ok(())
}
fn valid_record(state: &AppState, r: &Record) -> bool {
    r.version == 1
        && r.owner == owner(state)
        && r.application > 0
        && r.guild > 0
        && r.channel > 0
        && r.bot > 0
        && r.nonce.len() == 20
        && r.nonce.bytes().all(|b| b.is_ascii_hexdigit())
        && r.message.is_none_or(|id| id > 0)
        && !(r.uncertain && r.message.is_some())
}
pub(super) fn visible_status(state: &AppState, db: &Connection) -> String {
    match record_db(db) {
        Ok(Some(r)) if !valid_record(state, &r) => "binding_changed",
        Ok(Some(r)) if r.uncertain => "uncertain",
        Ok(Some(r)) if r.missing => "message_missing",
        Ok(Some(_)) if *lock(&state.0.discord_message_status) == "off" => "cleanup_needed",
        Err(_) => "metadata_failed",
        _ => *lock(&state.0.discord_message_status),
    }
    .into()
}

struct Snapshot {
    active: bool,
    desired: Option<(u64, u64, String)>,
    expires: Option<tokio::time::Instant>,
}
fn snapshot(state: &AppState, expected: &DiscordBridgeSettings) -> Snapshot {
    let db = lock(&state.0.db);
    let Ok(p) = settings(&db) else {
        return Snapshot {
            active: false,
            desired: None,
            expires: None,
        };
    };
    let content = (same_binding(&p, expected) && p.status_enabled)
        .then(|| public_content(state, &db, &p, true).ok().flatten())
        .flatten();
    let desired = content
        .zip(p.status_channel_id.as_deref().and_then(snowflake))
        .map(|(text, channel)| (channel, snowflake(&p.guild_id).unwrap_or_default(), text));
    let at = now();
    let expiry = desired.as_ref().and_then(|_| {
        lock(&state.0.voice).values().filter(|v| Some(v.channel_id.as_str()) == p.channel_id.as_deref() && v.alive_at(at))
            .filter_map(|v| {
                let session = db.query_row("SELECT expires FROM sessions WHERE token_hash=?1 AND user_id=?2 AND expires>?3", params![v.token_hash,v.user.id,at], |r| r.get::<_,u64>(0)).optional().ok().flatten()?;
                // Use the transport's actual liveness predicate, so a future
                // idle-timeout change cannot silently make this timer stale.
                let (mut low, mut high) = (at, v.expires);
                while low < high { let mid = low + (high-low)/2; if v.alive_at(mid) { low=mid+1; } else { high=mid; } }
                Some(low.min(session))
            }).min()
    });
    Snapshot {
        active: same_binding(&p, expected) && p.status_enabled,
        desired,
        expires: expiry.map(|t| {
            tokio::time::Instant::now() + Duration::from_secs(t.saturating_sub(at).max(1))
        }),
    }
}

fn status(state: &AppState, text: &'static str) {
    *lock(&state.0.discord_message_status) = text;
}
fn mentions() -> serde_json::Value {
    serde_json::json!({"parse":[],"replied_user":false})
}
fn body(content: &str) -> serde_json::Value {
    serde_json::json!({"content":content,"allowed_mentions":mentions(),"flags":4,"embeds":[],"components":[]})
}
fn http_status(error: &serenity::Error) -> Option<u16> {
    if let serenity::Error::Http(e) = error {
        e.status_code().map(|s| s.as_u16())
    } else {
        None
    }
}
fn failure(error: &serenity::Error) -> &'static str {
    match http_status(error) {
        Some(401 | 403) => "needs_permission",
        Some(404) => "message_missing",
        _ => "connection_failed",
    }
}
async fn channel(http: &Http, channel: u64, guild: u64) -> Result<(), &'static str> {
    match http
        .get_channel(ChannelId::new(channel))
        .await
        .map_err(|e| failure(&e))?
    {
        Channel::Guild(c)
            if c.id.get() == channel
                && c.guild_id.get() == guild
                && matches!(c.kind, ChannelType::Text | ChannelType::News) =>
        {
            Ok(())
        }
        _ => Err("wrong_channel"),
    }
}
fn owned(message: &serenity::all::Message, r: &Record, nonce: bool) -> bool {
    message.channel_id.get() == r.channel
        && message.author.id.get() == r.bot
        && message.author.bot
        && (!nonce
            || message.nonce.as_ref().is_some_and(|n| {
                serde_json::to_value(n).ok().as_ref() == Some(&serde_json::json!(r.nonce))
            }))
}
async fn remove_record(
    state: &AppState,
    http: &Http,
    application: u64,
    bot: u64,
) -> Result<(), &'static str> {
    remove_record_guarded(state, http, application, bot, None).await
}
async fn remove_record_guarded(
    state: &AppState,
    http: &Http,
    application: u64,
    bot: u64,
    actor: Option<(&Auth, u64)>,
) -> Result<(), &'static str> {
    let Some(r) = record(state).map_err(|_| "metadata_failed")? else {
        return Ok(());
    };
    if !valid_record(state, &r) || r.application != application || r.bot != bot {
        return Err("binding_changed");
    }
    if r.uncertain {
        return Err("uncertain");
    }
    if let Some(id) = r.message {
        channel(http, r.channel, r.guild).await?;
        match http
            .get_message(ChannelId::new(r.channel), MessageId::new(id))
            .await
        {
            Ok(m) if owned(&m, &r, false) => {
                if let Some((auth, revision)) = actor {
                    recheck(state, auth, revision).map_err(|_| "access_changed")?;
                }
                match http
                    .delete_message(ChannelId::new(r.channel), MessageId::new(id), None)
                    .await
                {
                    Ok(()) => {}
                    Err(e) if http_status(&e) == Some(404) => {}
                    Err(_) => return Err("cleanup_needed"),
                }
            }
            Ok(_) => return Err("binding_changed"),
            Err(e) if http_status(&e) == Some(404) => {}
            Err(_) => return Err("cleanup_needed"),
        }
    }
    if let Some((auth, revision)) = actor {
        let db = lock(&state.0.db);
        authorized(&db, auth).map_err(|_| "access_changed")?;
        let credential = lock(&state.0.discord_credential);
        if credential.revision() != revision {
            return Err("access_changed");
        }
        persist_db(&db, Some(&r), None).map_err(|_| "metadata_failed")?;
    } else {
        persist(state, Some(&r), None).map_err(|_| "metadata_failed")?;
    }
    Ok(())
}

pub(super) async fn cleanup(state: &AppState, http: &Http, application: u64, bot: Option<u64>) {
    if record(state).ok().flatten().is_none() {
        status(state, "off");
        return;
    }
    let Some(bot) = bot else {
        status(state, "cleanup_needed");
        return;
    };
    match tokio::time::timeout(
        Duration::from_secs(3),
        remove_record(state, http, application, bot),
    )
    .await
    {
        Ok(Ok(())) => status(state, "off"),
        Ok(Err(e)) => status(state, e),
        Err(_) => status(state, "cleanup_needed"),
    }
}

// The future is polled in the supervisor, rather than spawning an independent
// job that could retain a revoked token or outlive host shutdown.
pub(super) async fn run(
    state: &AppState,
    http: &Http,
    expected: &DiscordBridgeSettings,
    revision: u64,
    mut ready: tokio::sync::watch::Receiver<Option<u64>>,
    cancel: CancellationToken,
) {
    let mut changes = state.0.discord_changes.subscribe();
    let mut activity = None;
    let mut last_content: Option<(u64, String)> = None;
    let mut sent: Option<tokio::time::Instant> = None;
    let mut next: Option<tokio::time::Instant> = None;
    let mut blocked = false;
    let mut failures = 0u32;
    loop {
        if cancel.is_cancelled() || lock(&state.0.discord_credential).revision() != revision {
            return;
        }
        changes.borrow_and_update();
        let bot = *ready.borrow_and_update();
        let mut snap = snapshot(state, expected);
        if bot.is_none() || !snap.active {
            activity = None;
        } else if activity.is_none() {
            activity = Some(state.0.discord_activity.subscribe());
            // Catch a state change between the initial snapshot and subscribing.
            snap = snapshot(state, expected);
        }
        if let Some(bot) = bot {
            if let Some((target, guild, content)) = &snap.desired {
                // Retire the previous channel immediately when the administrator
                // switches target, without waiting for the next public edit.
                if !blocked
                    && record(state)
                        .ok()
                        .flatten()
                        .is_some_and(|r| r.channel != *target || r.guild != *guild)
                {
                    cleanup(
                        state,
                        http,
                        snowflake(&expected.application_id).unwrap_or_default(),
                        Some(bot),
                    )
                    .await;
                    if record(state).ok().flatten().is_some() {
                        blocked = true;
                        next = None;
                    }
                }
                let changed = last_content
                    .as_ref()
                    .is_none_or(|(c, v)| c != target || v != content);
                if changed && !blocked {
                    let due = *next.get_or_insert_with(|| {
                        (tokio::time::Instant::now() + DEBOUNCE).max(
                            sent.map(|s| s + INTERVAL)
                                .unwrap_or_else(tokio::time::Instant::now),
                        )
                    });
                    if due <= tokio::time::Instant::now() {
                        let result = tokio::select! {
                            _ = cancel.cancelled() => return,
                            _ = changes.changed() => { next=None; continue; },
                            r = tokio::time::timeout(Duration::from_secs(5), upsert(state,http,expected,bot,*target,*guild,content)) => r,
                        };
                        match result {
                            Ok(Ok(())) => {
                                last_content = Some((*target, content.clone()));
                                sent = Some(tokio::time::Instant::now());
                                next = None;
                                failures = 0;
                                status(state, "online");
                            }
                            Ok(Err(e)) => {
                                status(state, e);
                                blocked = !matches!(e, "connection_failed" | "policy_changed");
                                failures = failures.saturating_add(1);
                                next = if e == "policy_changed" {
                                    None
                                } else {
                                    Some(tokio::time::Instant::now() + retry_delay(failures))
                                };
                            }
                            Err(_) => {
                                let uncertain =
                                    record(state).ok().flatten().is_some_and(|r| r.uncertain);
                                status(
                                    state,
                                    if uncertain {
                                        "uncertain"
                                    } else {
                                        "connection_failed"
                                    },
                                );
                                blocked = uncertain;
                                failures = failures.saturating_add(1);
                                next = Some(tokio::time::Instant::now() + retry_delay(failures));
                            }
                        }
                    }
                } else if !changed {
                    next = None;
                }
            } else {
                cleanup(
                    state,
                    http,
                    snowflake(&expected.application_id).unwrap_or_default(),
                    Some(bot),
                )
                .await;
                last_content = None;
                next = None;
                blocked = false;
            }
        } else {
            status(
                state,
                if snap.desired.is_some() {
                    "waiting_for_bot"
                } else {
                    "off"
                },
            );
        }
        // Neither failed/private/disabled status nor an unchanged empty room
        // uses an interval. The only timer is an actual expiry or pending write.
        let deadline = snap
            .expires
            .into_iter()
            .chain(next.filter(|_| !blocked))
            .min();
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = changes.changed() => { blocked=false;next=None; },
            _ = ready.changed() => { blocked=false;next=None; },
            _ = async { if let Some(rx) = activity.as_mut() { let _=rx.changed().await; } else { std::future::pending::<()>().await; } } => {},
            _ = async { if let Some(t)=deadline { tokio::time::sleep_until(t).await; } else { std::future::pending::<()>().await; } } => {},
        }
    }
}

async fn upsert(
    state: &AppState,
    http: &Http,
    expected: &DiscordBridgeSettings,
    bot: u64,
    target: u64,
    guild: u64,
    content: &str,
) -> Result<(), &'static str> {
    let application = snowflake(&expected.application_id).ok_or("wrong_application")?;
    let mut previous = record(state).map_err(|_| "metadata_failed")?;
    if previous
        .as_ref()
        .is_some_and(|r| r.channel != target || r.guild != guild)
    {
        remove_record(state, http, application, bot).await?;
        previous = None;
    }
    let saved = previous.clone();
    let mut r = previous.unwrap_or_else(|| Record {
        version: 1,
        owner: owner(state),
        application,
        guild,
        channel: target,
        bot,
        nonce: Uuid::new_v4().simple().to_string()[..20].into(),
        message: None,
        uncertain: false,
        missing: false,
    });
    if !valid_record(state, &r) || r.application != application || r.bot != bot {
        return Err("binding_changed");
    }
    if r.uncertain {
        return Err("uncertain");
    }
    if r.missing {
        return Err("message_missing");
    }
    channel(http, target, guild).await?;
    // Re-evaluate policy immediately before emitting public content. A queued
    // obsolete room/ACL/label snapshot cannot initiate a new write.
    if snapshot(state, expected).desired.as_ref() != Some(&(target, guild, content.into())) {
        return Err("policy_changed");
    }
    if let Some(id) = r.message {
        let message = match http
            .get_message(ChannelId::new(target), MessageId::new(id))
            .await
        {
            Ok(m) if owned(&m, &r, false) => m,
            Ok(_) => return Err("binding_changed"),
            Err(e) if http_status(&e) == Some(404) => {
                let old = r.clone();
                r.missing = true;
                persist(state, Some(&old), Some(&r)).map_err(|_| "metadata_failed")?;
                return Err("message_missing");
            }
            Err(e) => return Err(failure(&e)),
        };
        if snapshot(state, expected).desired.as_ref() != Some(&(target, guild, content.into())) {
            return Err("policy_changed");
        }
        if message.content != content {
            let mut payload = body(content);
            payload["flags"] = serde_json::json!(message.flags.unwrap_or_default().bits() | 4);
            let edited = http
                .edit_message(
                    ChannelId::new(target),
                    MessageId::new(id),
                    &payload,
                    Vec::new(),
                )
                .await
                .map_err(|e| failure(&e))?;
            if !owned(&edited, &r, false) {
                return Err("binding_changed");
            }
        }
    } else {
        // Commit uncertainty before POST. If the process stops after delivery
        // but before storing the returned ID, restart cannot create a duplicate.
        r.uncertain = true;
        persist(state, saved.as_ref(), Some(&r)).map_err(|_| "metadata_failed")?;
        let pending = r.clone();
        let mut payload = body(content);
        payload["nonce"] = serde_json::json!(r.nonce);
        payload["enforce_nonce"] = serde_json::json!(true);
        payload["flags"] = serde_json::json!(4 | 4096); // no embeds or initial push notification
        let result = http
            .send_message(ChannelId::new(target), Vec::new(), &payload)
            .await;
        match result {
            Ok(message) if owned(&message, &r, true) => {
                r.message = Some(message.id.get());
                r.uncertain = false;
                persist(state, Some(&pending), Some(&r)).map_err(|_| "metadata_failed")?;
            }
            Err(e) if http_status(&e).is_some_and(|s| matches!(s, 400 | 401 | 403 | 404)) => {
                r.uncertain = false;
                persist(state, Some(&pending), Some(&r)).map_err(|_| "metadata_failed")?;
                return Err(failure(&e));
            }
            _ => return Err("uncertain"),
        }
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Recover {
    message_id: String,
}
pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/discord/status/recover", post(recover))
        .route("/api/discord/status/cleanup", post(manual_cleanup))
        .layer(DefaultBodyLimit::max(2048))
}
fn admin(state: &AppState, headers: &HeaderMap) -> ApiResult<Auth> {
    let auth = state.auth(headers)?;
    let db = lock(&state.0.db);
    active_user(&db, &auth.user.id)?;
    roles::require(&db, &auth.user.id, PERMISSION_ADMIN)?;
    Ok(auth)
}
pub(super) fn authorized(db: &Connection, auth: &Auth) -> ApiResult<()> {
    active_user(db, &auth.user.id)?;
    roles::require(db, &auth.user.id, PERMISSION_ADMIN)?;
    let live: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sessions WHERE token_hash=?1 AND user_id=?2 AND expires>?3)",
        params![auth.token_hash, auth.user.id, now()],
        |r| r.get(0),
    )?;
    if !live {
        return Err(unauthorized());
    }
    Ok(())
}
fn recheck(state: &AppState, auth: &Auth, revision: u64) -> ApiResult<()> {
    let db = lock(&state.0.db);
    authorized(&db, auth)?;
    if lock(&state.0.discord_credential).revision() != revision {
        return Err(forbidden());
    }
    Ok(())
}
fn persist_authorized(
    state: &AppState,
    auth: &Auth,
    revision: u64,
    expected: &Record,
    value: &Record,
) -> ApiResult<()> {
    let db = lock(&state.0.db);
    authorized(&db, auth)?;
    let credential = lock(&state.0.discord_credential);
    if credential.revision() != revision {
        return Err(forbidden());
    }
    persist_db(&db, Some(expected), Some(value))
}
fn retry_delay(failures: u32) -> Duration {
    Duration::from_secs((30u64.saturating_mul(1u64 << failures.saturating_sub(1).min(4))).min(300))
}
async fn manual_http(state: &AppState, r: &Record) -> ApiResult<(Http, u64)> {
    let (token, revision) = lock(&state.0.discord_credential).current();
    let token = token.ok_or(bad("Add this status message's bot token on the host first"))?;
    let http = crate::discord_bridge::http_for(state, &token);
    let application =
        tokio::time::timeout(Duration::from_secs(5), http.get_current_application_info())
            .await
            .map_err(|_| bad("Discord did not answer"))?
            .map_err(|_| bad("Check the bot token and Discord access"))?;
    if application.id.get() != r.application {
        return Err(bad("Use the original status message's Discord app"));
    }
    let bot = tokio::time::timeout(Duration::from_secs(5), http.get_current_user())
        .await
        .map_err(|_| bad("Discord did not answer"))?
        .map_err(|_| bad("Check the bot token and Discord access"))?;
    if bot.id.get() != r.bot || !bot.bot {
        return Err(forbidden());
    }
    Ok((http, revision))
}
async fn recover(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<Recover>,
) -> ApiResult<Json<DiscordBridgeView>> {
    let auth = admin(&state, &headers)?;
    let mut r = record(&state)?.ok_or(bad("There is no uncertain status message"))?;
    if !valid_record(&state, &r) || !r.uncertain {
        return Err(bad("This status message does not need recovery"));
    }
    let id = snowflake(&input.message_id).ok_or(bad("Copy the Discord message ID"))?;
    let (http, revision) = manual_http(&state, &r).await?;
    let message = tokio::time::timeout(Duration::from_secs(5), async {
        channel(&http, r.channel, r.guild).await?;
        http.get_message(ChannelId::new(r.channel), MessageId::new(id))
            .await
            .map_err(|_| "Cannot read the selected status message")
    })
    .await
    .map_err(|_| bad("Discord did not answer"))?
    .map_err(|_| bad("Cannot verify this status message"))?;
    if !owned(&message, &r, true) {
        return Err(bad(
            "The message is not the status created by this Relay host",
        ));
    }
    let pending = r.clone();
    r.message = Some(id);
    r.uncertain = false;
    persist_authorized(&state, &auth, revision, &pending, &r)?;
    state
        .0
        .discord_changes
        .send_modify(|v| *v = v.wrapping_add(1));
    crate::discord_bridge::get_settings(State(state), headers).await
}
async fn manual_cleanup(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<DiscordBridgeView>> {
    let auth = admin(&state, &headers)?;
    {
        let db = lock(&state.0.db);
        authorized(&db, &auth)?;
        let mut p = settings(&db)?;
        p.status_enabled = false;
        db.execute("INSERT INTO metadata(key,value) VALUES('discord-bridge-v1',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[serde_json::to_string(&p)?])?;
    }
    state
        .0
        .discord_changes
        .send_modify(|v| *v = v.wrapping_add(1));
    if let Some(r) = record(&state)? {
        if !valid_record(&state, &r) || r.uncertain {
            return Err(bad("Recover this host's uncertain status message first"));
        }
        let (http, revision) = manual_http(&state, &r).await?;
        recheck(&state, &auth, revision)?;
        tokio::time::timeout(
            Duration::from_secs(5),
            remove_record_guarded(&state, &http, r.application, r.bot, Some((&auth, revision))),
        )
        .await
        .map_err(|_| bad("The old status could not be removed yet"))?
        .map_err(|_| bad("The old status could not be removed yet"))?;
    }
    status(&state, "off");
    crate::discord_bridge::get_settings(State(state), headers).await
}
