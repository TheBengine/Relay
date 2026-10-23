//! Opt-in, read-only Discord command. No message/member/presence intents or cache.
use super::*;
use relay_protocol::discord::{snowflake, DiscordBridgeSettings, DiscordBridgeView};
use serenity::all::{
    ApplicationId, ClientBuilder, CommandId, ConnectionStage, Context, CreateAllowedMentions,
    CreateCommand, CreateInteractionResponse, CreateInteractionResponseMessage, EventHandler,
    GatewayIntents, GuildId, Http, Interaction, InteractionId, InteractionResponseFlags, Ready,
    ResumedEvent, ShardStageUpdateEvent,
};
use std::collections::{HashSet, VecDeque};
use tokio_util::sync::CancellationToken;

#[cfg(test)]
pub(super) mod tests;

const KEY: &str = "discord-bridge-v1";
const UNAVAILABLE: &str = "Relay call sharing is unavailable. Ask the host for an invitation.";

pub(super) fn settings(db: &Connection) -> ApiResult<DiscordBridgeSettings> {
    let value: Option<String> = db
        .query_row("SELECT value FROM metadata WHERE key=?1", [KEY], |r| {
            r.get(0)
        })
        .optional()?;
    match value {
        Some(v) => serde_json::from_str(&v).map_err(|_| bad("Discord settings are damaged")),
        None => Ok(DiscordBridgeSettings::default()),
    }
}

// Fail closed when a public-looking channel has any explicit ACL. Checking this
// again on every command also catches changes made after the administrator saved.
fn public_room(db: &Connection, channel: &str) -> ApiResult<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM channels c WHERE c.id=?1 AND c.kind='voice' AND c.visibility='public' AND c.voice_parent_id IS NULL AND NOT EXISTS(SELECT 1 FROM channel_permissions p WHERE p.channel_id=c.id) AND NOT EXISTS(SELECT 1 FROM conversations t WHERE t.channel_id=c.id))", [channel], |r| r.get(0))?)
}

fn literal(value: &str) -> String {
    let mut out = String::new();
    for c in value.chars() {
        if "\\`*_~|><[]()#".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

pub(super) fn public_content(
    state: &AppState,
    db: &Connection,
    policy: &DiscordBridgeSettings,
    markdown: bool,
) -> ApiResult<Option<String>> {
    if !policy.enabled || !policy.valid() {
        return Ok(None);
    }
    let Some(channel) = policy.channel_id.as_deref() else {
        return Ok(None);
    };
    if !public_room(db, channel)? {
        return Ok(None);
    }
    let count = lock(&state.0.voice).values()
        .filter(|v| v.channel_id == channel && v.alive_at(now()) && channel_access(db, channel, &v.user.id, JOIN).is_ok()
            && db.query_row("SELECT EXISTS(SELECT 1 FROM sessions WHERE token_hash=?1 AND user_id=?2 AND expires>?3)", params![v.token_hash, v.user.id, now()], |r| r.get::<_,bool>(0)).unwrap_or(false))
        .map(|v| &v.user.id).collect::<HashSet<_>>().len();
    Ok(Some(format!(
        "Relay · {}\n{} {} in the call\n\nHow to join\n{}",
        if markdown {
            literal(policy.public_label.trim())
        } else {
            policy.public_label.trim().into()
        },
        count,
        if count == 1 { "person" } else { "people" },
        if markdown {
            literal(policy.join_instructions.trim())
        } else {
            policy.join_instructions.trim().into()
        }
    )))
}

fn view(state: &AppState, db: &Connection) -> ApiResult<DiscordBridgeView> {
    let policy = settings(db)?;
    Ok(DiscordBridgeView {
        connection: if policy.enabled {
            *lock(&state.0.discord_status)
        } else {
            "off"
        }
        .into(),
        public_preview: public_content(state, db, &policy, false)?,
        channel_status: crate::discord_channel_status::visible_status(state, db),
        settings: policy,
    })
}

pub(super) async fn get_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<DiscordBridgeView>> {
    let auth = state.auth(&headers)?;
    let db = lock(&state.0.db);
    active_user(&db, &auth.user.id)?;
    roles::require(&db, &auth.user.id, PERMISSION_ADMIN)?;
    Ok(Json(view(&state, &db)?))
}

pub(super) async fn save_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut input): Json<DiscordBridgeSettings>,
) -> ApiResult<Json<DiscordBridgeView>> {
    let auth = state.auth(&headers)?;
    input.application_id = input.application_id.trim().into();
    input.guild_id = input.guild_id.trim().into();
    input.status_channel_id = input
        .status_channel_id
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty());
    input.public_label = input.public_label.trim().into();
    input.join_instructions = input.join_instructions.trim().into();
    if !input.valid() {
        return Err(bad("Choose a room, Discord application/server IDs, a public label (80 characters) and join instructions (700 characters)"));
    }
    let changed;
    {
        let mut db = lock(&state.0.db);
        let tx = db.transaction()?;
        crate::discord_channel_status::authorized(&tx, &auth)?;
        if input.enabled && !public_room(&tx, input.channel_id.as_deref().unwrap_or_default())? {
            return Err(bad(
                "Choose a public community voice room without channel permission overrides",
            ));
        }
        changed = settings(&tx)? != input;
        if changed {
            tx.execute("INSERT INTO metadata(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![KEY, serde_json::to_string(&input)?])?;
            roles::audit(&tx, &auth.user.id, "discord.policy.updated", KEY)?;
        }
        tx.commit()?;
    }
    if changed {
        state
            .0
            .discord_changes
            .send_modify(|v| *v = v.wrapping_add(1));
    }
    let db = lock(&state.0.db);
    Ok(Json(view(&state, &db)?))
}

pub(super) fn same_binding(
    current: &DiscordBridgeSettings,
    expected: &DiscordBridgeSettings,
) -> bool {
    current.enabled
        && current.valid()
        && current.application_id == expected.application_id
        && current.guild_id == expected.guild_id
}

fn command_content(
    state: &AppState,
    expected: &DiscordBridgeSettings,
    application: u64,
    guild: Option<u64>,
) -> String {
    if snowflake(&expected.application_id) != Some(application)
        || snowflake(&expected.guild_id) != guild
    {
        return UNAVAILABLE.into();
    }
    let db = lock(&state.0.db);
    let Ok(current) = settings(&db) else {
        return UNAVAILABLE.into();
    };
    if !same_binding(&current, expected) {
        return UNAVAILABLE.into();
    }
    public_content(state, &db, &current, true)
        .ok()
        .flatten()
        .unwrap_or_else(|| UNAVAILABLE.into())
}

fn response(content: String) -> CreateInteractionResponse {
    CreateInteractionResponse::Message(
        CreateInteractionResponseMessage::new()
            .content(content)
            .flags(InteractionResponseFlags::EPHEMERAL | InteractionResponseFlags::SUPPRESS_EMBEDS)
            .allowed_mentions(
                CreateAllowedMentions::new()
                    .all_users(false)
                    .all_roles(false)
                    .everyone(false),
            ),
    )
}

#[derive(Default)]
struct Seen {
    ids: HashSet<InteractionId>,
    order: VecDeque<InteractionId>,
}
impl Seen {
    fn insert(&mut self, id: InteractionId) -> bool {
        if !self.ids.insert(id) {
            return false;
        }
        self.order.push_back(id);
        if self.order.len() > 512 {
            if let Some(old) = self.order.pop_front() {
                self.ids.remove(&old);
            }
        }
        true
    }
}

struct Handler {
    state: AppState,
    policy: DiscordBridgeSettings,
    cancel: CancellationToken,
    command: Arc<tokio::sync::Mutex<Option<CommandId>>>,
    seen: Mutex<Seen>,
    credential_revision: u64,
    ready: tokio::sync::watch::Sender<Option<u64>>,
}
impl Handler {
    fn active(&self) -> bool {
        !self.cancel.is_cancelled()
            && lock(&self.state.0.discord_credential).revision() == self.credential_revision
    }
}

#[serenity::async_trait]
impl EventHandler for Handler {
    async fn shard_stage_update(&self, _: Context, event: ShardStageUpdateEvent) {
        if self.active() && event.new != ConnectionStage::Connected {
            *lock(&self.state.0.discord_status) = "connecting";
        }
    }

    async fn resume(&self, _: Context, _: ResumedEvent) {
        if self.active() && self.command.lock().await.is_some() {
            *lock(&self.state.0.discord_status) = "online";
        }
    }

    async fn ready(&self, ctx: Context, ready: Ready) {
        if !self.active() {
            return;
        }
        if snowflake(&self.policy.application_id) != Some(ready.application.id.get()) {
            *lock(&self.state.0.discord_status) = "wrong_application";
            self.cancel.cancel();
            return;
        }
        let mut owned = self.command.lock().await;
        if !self.active() {
            return;
        }
        if owned.is_some() {
            self.ready.send_replace(Some(ready.user.id.get()));
            *lock(&self.state.0.discord_status) = "online";
            return;
        }
        let guild =
            GuildId::new(snowflake(&self.policy.guild_id).expect("validated Discord guild"));
        // Upsert just /relay, never bulk-overwrite the application's other commands.
        let result = tokio::select! {
            _ = self.cancel.cancelled() => return,
            result = tokio::time::timeout(Duration::from_secs(15), guild.create_command(&ctx.http, CreateCommand::new("relay").description("Show the shared Relay call and how to join"))) => result,
        };
        match result {
            Ok(Ok(command)) => {
                *owned = Some(command.id);
                if self.active() {
                    self.ready.send_replace(Some(ready.user.id.get()));
                    *lock(&self.state.0.discord_status) = "online";
                } else {
                    self.cancel.cancel();
                }
            }
            _ => {
                *lock(&self.state.0.discord_status) = "setup_failed";
                self.cancel.cancel();
            }
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        let Interaction::Command(command) = interaction else {
            return;
        };
        if !self.active() || command.data.name != "relay" || !lock(&self.seen).insert(command.id) {
            return;
        }
        if *self.command.lock().await != Some(command.data.id) {
            return;
        }
        let content = command_content(
            &self.state,
            &self.policy,
            command.application_id.get(),
            command.guild_id.map(|g| g.get()),
        );
        if !self.active() {
            return;
        }
        // No reads of Discord usernames, member lists, messages or Relay content.
        // An in-flight response cannot be recalled after opt-out; cancellation
        // prevents a queued handler from starting further HTTP work.
        tokio::select! {
            _ = self.cancel.cancelled() => {},
            _ = tokio::time::timeout(Duration::from_millis(2500), command.create_response(&ctx.http, response(content))) => {},
        }
    }
}

pub(super) async fn run(state: AppState, token: Option<String>) {
    lock(&state.0.discord_credential).environment(token);
    let mut changes = state.0.discord_changes.subscribe();
    let mut last_attempt: Option<std::time::Instant> = None;
    loop {
        changes.borrow_and_update();
        let policy = settings(&lock(&state.0.db))
            .ok()
            .filter(|p| p.enabled && p.valid());
        if let Some(policy) = policy {
            let (token, credential_revision) = lock(&state.0.discord_credential).current();
            if let Some(token) = token.as_deref() {
                if let Some(previous) = last_attempt {
                    if let Some(wait) = Duration::from_secs(5).checked_sub(previous.elapsed()) {
                        tokio::select! {
                            _ = state.shutdown_requested() => break,
                            _ = changes.changed() => {},
                            _ = tokio::time::sleep(wait) => {},
                        }
                        continue;
                    }
                }
                last_attempt = Some(std::time::Instant::now());
                let cancel = CancellationToken::new();
                let (ready, ready_rx) = tokio::sync::watch::channel(None);
                let command = Arc::new(tokio::sync::Mutex::new(None));
                *lock(&state.0.discord_status) = "connecting";
                #[cfg(test)]
                state
                    .0
                    .discord_attempts
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let http = http_for(&state, token);
                let allowance = tokio::select! {
                    _ = state.shutdown_requested() => break,
                    _ = changes.changed() => continue,
                    result = tokio::time::timeout(Duration::from_secs(10), http.get_bot_gateway()) => result,
                };
                if !matches!(allowance, Ok(Ok(ref a)) if a.session_start_limit.remaining > 0 && a.session_start_limit.max_concurrency > 0)
                {
                    *lock(&state.0.discord_status) = if matches!(allowance, Ok(Ok(_))) {
                        "session_limit"
                    } else {
                        "connection_failed"
                    };
                    tokio::select! {
                        _ = state.shutdown_requested() => break,
                        _ = changes.changed() => continue,
                    }
                }
                let builder = ClientBuilder::new_with_http(http, GatewayIntents::empty())
                    .application_id(ApplicationId::new(
                        snowflake(&policy.application_id).expect("validated Discord application"),
                    ))
                    .event_handler(Handler {
                        state: state.clone(),
                        policy: policy.clone(),
                        cancel: cancel.clone(),
                        command: command.clone(),
                        seen: Mutex::new(Seen::default()),
                        credential_revision,
                        ready,
                    });
                let built = tokio::select! {
                    _ = state.shutdown_requested() => break,
                    _ = changes.changed() => continue,
                    result = builder => result,
                };
                match built {
                    Ok(mut client) => {
                        #[cfg(test)]
                        let test_proxy = lock(&state.0.test_discord_proxy).clone();
                        #[cfg(test)]
                        if let Some(proxy) = test_proxy {
                            *client.ws_url.lock().await =
                                proxy.replacen("http://", "ws://", 1) + "/socket";
                        }
                        let manager = client.shard_manager.clone();
                        let http = client.http.clone();
                        let mut shutdown = false;
                        let mut changed = false;
                        {
                            let start = client.start();
                            let status = crate::discord_channel_status::run(
                                &state,
                                &http,
                                &policy,
                                credential_revision,
                                ready_rx.clone(),
                                cancel.clone(),
                            );
                            tokio::pin!(start);
                            tokio::pin!(status);
                            loop {
                                tokio::select! {
                                    _ = state.shutdown_requested() => { shutdown = true; break; },
                                    _ = changes.changed() => {
                                        let keep = settings(&lock(&state.0.db)).is_ok_and(|p| same_binding(&p, &policy))
                                            && lock(&state.0.discord_credential).revision() == credential_revision;
                                        if !keep { changed = true; break; }
                                    },
                                    _ = cancel.cancelled() => break,
                                    _ = &mut status => break,
                                    _ = &mut start => { *lock(&state.0.discord_status) = "connection_failed"; break; },
                                }
                            }
                        }
                        cancel.cancel();
                        // All cleanup shares one bound; status cannot extend the
                        // host's existing seven-second shutdown budget.
                        let bot = *ready_rx.borrow();
                        let id = *command.lock().await;
                        let _ = tokio::time::timeout(Duration::from_secs(5), async {
                            tokio::join!(
                                manager.shutdown_all(),
                                async {
                                    if let Some(id) = id {
                                        let guild = GuildId::new(
                                            snowflake(&policy.guild_id)
                                                .expect("validated Discord guild"),
                                        );
                                        let _ = guild.delete_command(&http, id).await;
                                    }
                                },
                                crate::discord_channel_status::cleanup(
                                    &state,
                                    &http,
                                    snowflake(&policy.application_id).unwrap_or_default(),
                                    bot
                                )
                            );
                        })
                        .await;
                        if shutdown {
                            break;
                        }
                        if changed {
                            continue;
                        }
                    }
                    Err(_) => *lock(&state.0.discord_status) = "connection_failed",
                }
            } else {
                *lock(&state.0.discord_status) = "needs_token";
            }
        } else {
            *lock(&state.0.discord_status) = "off";
        }
        // No polling while disabled, missing credentials or a terminal setup error.
        tokio::select! {
            _ = state.shutdown_requested() => break,
            _ = changes.changed() => {},
        }
    }
    *lock(&state.0.discord_status) = "off";
}

pub(super) fn http_for(state: &AppState, token: &str) -> Http {
    #[cfg(not(test))]
    let _ = state;
    let http = Http::new(token);
    #[cfg(test)]
    let http = if let Some(proxy) = lock(&state.0.test_discord_proxy).clone() {
        serenity::http::HttpBuilder::new(token)
            .client(
                reqwest::Client::builder()
                    .no_proxy()
                    .build()
                    .expect("local fixture HTTP client"),
            )
            .proxy(proxy)
            .ratelimiter_disabled(true)
            .build()
    } else {
        http
    };
    http
}
