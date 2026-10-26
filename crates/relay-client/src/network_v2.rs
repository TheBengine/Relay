use super::*;
use std::collections::{HashSet, VecDeque};
use std::io::Read;

pub(super) type ChatEventInbox = Arc<std::sync::Mutex<VecDeque<Event>>>;

#[derive(Default)]
pub(super) struct ChatSession {
    pub(super) authority: Option<relay_protocol::handover::RoutePin>,
    pub(super) host_paused: bool,
    pub(super) authority_aliases: Vec<String>,
    pub(super) handover_checked_at: Option<Instant>,
    pub(super) handover_probe: Option<crate::host_route::Job>,
    pub(super) account_provider: Option<AccountProvider>,
    pub(super) events: ChatEventInbox,
    capabilities: HashSet<String>,
    protection: CommunityProtection,
    open_consent: Option<u64>,
    channels: HashMap<String, (u32, bool)>,
    send_account: Option<Cooldown>,
    send_targets: HashMap<Target, Cooldown>,
    edits: HashMap<String, Cooldown>,
    committed_ids: VecDeque<String>,
    sent_typing: Option<(Target, bool, Instant)>,
    typing_dirty: bool,
    typing_polled_at: Option<Instant>,
    typing_state: TypingState,
    typing_received_at: Option<Instant>,
}

#[derive(Clone)]
struct Cooldown {
    error: String,
    code: String,
    until: Instant,
    // A successful send may be retried idempotently even during slowmode.
    committed_client_id: Option<String>,
}

impl Cooldown {
    fn remaining_ms(&self, now: Instant) -> u64 {
        self.until
            .saturating_duration_since(now)
            .as_millis()
            .min(u64::MAX as u128) as u64
    }
}

impl ChatSession {
    pub(super) fn idle_deadline(&self, now: Instant) -> Option<Instant> {
        let mut next = None;
        if self.supports("typing-v1") {
            if self.typing_dirty {
                next = Some(
                    self.typing_polled_at
                        .map_or(now, |at| at + Duration::from_millis(500)),
                );
            } else if !self.typing_state.users.is_empty() {
                next = Some(
                    self.typing_polled_at
                        .map_or(now, |at| at + Duration::from_secs(2)),
                );
            }
            if let Some(received) = self.typing_received_at {
                for user in &self.typing_state.users {
                    let at = received
                        + Duration::from_millis(
                            user.expires_at_ms
                                .saturating_sub(self.typing_state.server_time_ms),
                        );
                    next = Some(next.map_or(at, |old| old.min(at)));
                }
            }
        }
        next
    }

    pub(super) fn new(bootstrap: &Bootstrap) -> Self {
        let mut state = Self::default();
        state.update_bootstrap(bootstrap);
        state
    }

    pub(super) fn update_bootstrap(&mut self, bootstrap: &Bootstrap) {
        self.host_paused = bootstrap.host_paused;
        self.account_provider = bootstrap.account_provider.clone();
        self.capabilities = bootstrap.capabilities.iter().cloned().collect();
        self.protection = bootstrap.protection.clone();
        let channels: HashMap<_, _> = bootstrap
            .channels
            .iter()
            .filter(|c| c.kind == "text")
            .map(|c| {
                (
                    c.id.clone(),
                    (
                        c.slowmode_seconds.min(MAX_SLOWMODE_SECONDS),
                        c.slowmode_bypass,
                    ),
                )
            })
            .collect();
        self.send_targets.retain(|target, _| match target {
            Target::Channel(id) => channels.get(id).is_some_and(|&(seconds, bypass)| {
                seconds > 0 && !bypass && self.channels.get(id) == Some(&(seconds, bypass))
            }),
            Target::Direct(_) => true,
        });
        self.channels = channels;
    }

    pub(super) fn supports(&self, capability: &str) -> bool {
        self.capabilities.contains(capability)
    }

    pub(super) fn set_open_consent(&mut self, revision: Option<u64>) {
        self.open_consent = revision;
    }

    /// Text goes out unprotected only when the community runs in the open mode
    /// AND this person accepted that exact revision. An announcement alone, from
    /// a server or an invitation, never changes what this client sends.
    pub(super) fn plain_allowed(&self) -> bool {
        crate::protection::plain_allowed(&self.protection, self.open_consent)
    }
    pub(super) fn is_open(&self) -> bool {
        self.protection.level == ProtectionLevel::Open
    }
    pub(super) fn open_revision(&self) -> Option<u64> {
        self.plain_allowed().then_some(self.protection.revision)
    }

    pub(super) fn plain_files_readable(&self) -> bool {
        crate::protection::plain_files_readable(&self.protection)
    }

    pub(super) fn clear_ephemeral(&mut self) {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.sent_typing = None;
        self.typing_dirty = false;
        self.typing_state = TypingState::default();
        self.typing_received_at = None;
    }

    fn send_cooldown(
        &mut self,
        target: &Target,
        client_id: &str,
        now: Instant,
    ) -> Option<Cooldown> {
        if self.send_account.as_ref().is_some_and(|c| c.until <= now) {
            self.send_account = None;
        }
        self.send_targets.retain(|_, c| c.until > now);
        self.edits.retain(|_, c| c.until > now);
        if self.committed_ids.iter().any(|id| id == client_id) {
            return None;
        }
        self.send_account
            .as_ref()
            .or_else(|| self.send_targets.get(target))
            .filter(|c| c.committed_client_id.as_deref() != Some(client_id))
            .cloned()
    }

    fn remember_committed(&mut self, client_id: &str) -> bool {
        if self.committed_ids.iter().any(|id| id == client_id) {
            return false;
        }
        if self.committed_ids.len() == 64 {
            self.committed_ids.pop_front();
        }
        self.committed_ids.push_back(client_id.to_owned());
        true
    }

    fn remember_send_cooldown(&mut self, target: Target, cooldown: Cooldown) {
        if cooldown.code == "slowmode" && matches!(&target, Target::Channel(_)) {
            self.send_targets.retain(|_, c| c.until > Instant::now());
            if self.send_targets.len() < 128 || self.send_targets.contains_key(&target) {
                self.send_targets.insert(target, cooldown);
            }
        } else {
            self.send_account = Some(cooldown);
        }
    }
}

enum WriteFailure {
    Error(String),
    RateLimited(Cooldown),
}

// Six hours matches the longest supported channel slowmode. Malformed, negative,
// zero, floating-point and overflowing headers never create an infinite retry.
const MAX_COOLDOWN_MS: u64 = 21_600_000;

fn retry_after_ms(value: &serde_json::Value, header: Option<&str>) -> u64 {
    let body = value
        .get("retry_after_ms")
        .and_then(serde_json::Value::as_u64)
        .filter(|ms| *ms > 0);
    let header = header
        .and_then(|h| h.trim().parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map(|seconds| seconds.saturating_mul(1_000));
    body.or(header).unwrap_or(1_000).clamp(1, MAX_COOLDOWN_MS)
}

fn checked_write(
    response: reqwest::blocking::Response,
) -> Result<reqwest::blocking::Response, WriteFailure> {
    if response.status() != reqwest::StatusCode::TOO_MANY_REQUESTS {
        return checked(response).map_err(WriteFailure::Error);
    }
    let header = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|h| h.to_str().ok())
        .map(str::to_owned);
    let value: serde_json::Value = read_json(response, 16 * 1024).unwrap_or_default();
    let wait = retry_after_ms(&value, header.as_deref());
    let error = value["error"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or("You are sending too quickly. Try again after the cooldown.")
        .to_owned();
    let code = value["code"]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 64)
        .unwrap_or("rate_limited")
        .to_owned();
    Err(WriteFailure::RateLimited(Cooldown {
        error,
        code,
        until: Instant::now() + Duration::from_millis(wait),
        committed_client_id: None,
    }))
}

pub(super) fn queue_chat_event(inbox: &ChatEventInbox, refresh: &AtomicBool, event: Event) {
    if event.kind == "typing" || event.kind == "pins" {
        let mut queue = inbox.lock().unwrap_or_else(|e| e.into_inner());
        // Coalesce hints for the same scope rather than accumulating every keypress.
        queue.retain(|old| {
            !(old.kind == event.kind
                && old.channel_id == event.channel_id
                && old.user_id == event.user_id
                && old.recipient_id == event.recipient_id)
        });
        if queue.len() >= 64 {
            queue.pop_front();
        }
        queue.push_back(event);
    } else if !event.kind.starts_with("media") {
        refresh.store(true, Ordering::Relaxed);
    }
}

fn typing_target(event: &Event, self_id: &str) -> Option<Target> {
    if event.kind != "typing" || event.typing.is_none() {
        return None;
    }
    if let Some(channel_id) = &event.channel_id {
        return (event.recipient_id.is_none()
            && event
                .user_id
                .as_deref()
                .is_some_and(|id| id != self_id && !id.is_empty()))
        .then(|| Target::Channel(channel_id.clone()));
    }
    let author = event.user_id.as_deref()?;
    let recipient = event.recipient_id.as_deref()?;
    if author.is_empty() || recipient != self_id || author == self_id {
        return None;
    }
    Some(Target::Direct(author.to_owned()))
}

fn filtered_typing(mut state: TypingState, self_id: &str) -> TypingState {
    let now = state.server_time_ms;
    let mut seen = HashSet::new();
    state.users.retain_mut(|entry| {
        if entry.user.id.is_empty()
            || entry.user.id == self_id
            || entry.expires_at_ms <= now
            || !seen.insert(entry.user.id.clone())
        {
            return false;
        }
        entry.expires_at_ms = entry.expires_at_ms.min(now.saturating_add(TYPING_TTL_MS));
        true
    });
    state.users.truncate(32);
    state
}

pub(super) fn path_id(id: &str) -> Result<(), String> {
    if id.is_empty()
        || id.len() > 100
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err("Invalid channel or message identifier.".into());
    }
    Ok(())
}

impl Worker {
    fn validate_send_binding(&self, binding: &SendBinding) -> Result<(), String> {
        let connection = self
            .connection
            .as_ref()
            .ok_or("Connect before sending this message.")?;
        if binding.protection.level != connection.chat.protection.level
            || binding.protection.revision != connection.chat.protection.revision
            || binding.plain != connection.chat.plain_allowed()
        {
            return Err("The protection settings changed since this message was prepared. Copy it to a new message before sending.".into());
        }
        // An accepted open-mode send has no cryptographic sender identity.
        // A background device refresh must not block its exact plaintext retry.
        if binding.plain {
            return Ok(());
        }
        match connection.crypto.as_ref() {
            Some(crypto)
                if binding.device_id == crypto.device.identity().device_id
                    && binding.fingerprint == crypto.device.fingerprint()? =>
            {
                Ok(())
            }
            _ => Err("The encryption device changed since this message was prepared. Copy it to a new message before sending.".into()),
        }
    }

    pub(super) fn edit_message(
        &mut self,
        message_id: &str,
        content: String,
    ) -> Result<bool, String> {
        path_id(message_id)?;
        if content.len() > 10000 {
            return Err("Messages can contain up to 10,000 bytes.".into());
        }
        let now = Instant::now();
        let existing = self.connection.as_mut().and_then(|c| {
            c.chat.edits.retain(|_, cooldown| cooldown.until > now);
            c.chat.edits.get(message_id).cloned()
        });
        if let Some(cooldown) = existing {
            self.emit_edit_cooldown(message_id.to_owned(), cooldown);
            return Ok(false);
        }
        let target = self
            .connection
            .as_ref()
            .and_then(|connection| connection.current.clone())
            .ok_or("Select the conversation containing this message before editing it.")?;
        let plain_allowed = self
            .connection
            .as_ref()
            .is_some_and(|c| c.chat.plain_allowed());
        if !plain_allowed {
            // Only encrypted edits exist here: refuse before any request when
            // the server cannot carry them.
            self.ensure_crypto_conversation(&target)?;
        }
        let scope = self.crypto_scope(&target)?;
        let own_user = self.connection.as_ref().unwrap().session.user.id.clone();
        let mut request = self
            .request(reqwest::Method::GET, "/api/history")?
            .query(&[("around", message_id), ("limit", "100")]);
        request = match &target {
            Target::Channel(id) => request.query(&[("channel_id", id)]),
            Target::Direct(id) => request.query(&[("peer_id", id)]),
        };
        let page: HistoryPage = read_json(
            checked(request.send().map_err(network_error)?)?,
            8 * 1024 * 1024,
        )?;
        if page.messages.len() > 100 {
            return Err("The edit context exceeds its history limit.".into());
        }
        let original = page
            .messages
            .into_iter()
            .find(|message| message.id == message_id)
            .ok_or("The message is no longer available in this conversation.")?;
        let routing_matches = match &target {
            Target::Channel(id) => {
                original.channel_id.as_ref() == Some(id) && original.recipient_id.is_none()
            }
            Target::Direct(peer) => {
                original.channel_id.is_none() && original.recipient_id.as_ref() == Some(peer)
            }
        };
        if plain_allowed && !original.content.starts_with(crate::e2ee::MESSAGE_PREFIX) {
            if original.author.id != own_user
                || !routing_matches
                || (content.trim().is_empty() && original.attachments.is_empty())
            {
                return Err("Only your messages in this conversation can be edited.".into());
            }
            let acknowledgement: Message = parse(
                self.request(
                    reqwest::Method::PATCH,
                    &format!("/api/messages/{message_id}"),
                )?
                .json(&EditMessage {
                    content: content.clone(),
                })
                .send()
                .map_err(network_error)?,
            )?;
            if acknowledgement.id != original.id
                || acknowledgement.author.id != own_user
                || acknowledgement.channel_id != original.channel_id
                || acknowledgement.recipient_id != original.recipient_id
                || acknowledgement.content != content
            {
                return Err("The edit acknowledgement does not match your message.".into());
            }
            return Ok(true);
        }
        if plain_allowed {
            self.ensure_crypto_conversation(&target)?;
        }
        if original.author.id != own_user
            || !routing_matches
            || !original.content.starts_with(crate::e2ee::MESSAGE_PREFIX)
        {
            return Err("Only your authenticated encrypted messages in this conversation can be edited. Plaintext fallback is disabled.".into());
        }
        let (protected_content, client_id) = {
            let crypto = self
                .connection
                .as_mut()
                .unwrap()
                .crypto
                .as_mut()
                .ok_or("Encryption keys are not ready.")?;
            let cached = crypto.decrypt_content(&scope, &original.content)?;
            if cached.author.user_id != own_user
                || cached.envelope.action.is_some()
                || original.client_id.as_deref() != Some(cached.envelope.client_id.as_str())
                || original.attachments.len() != cached.envelope.attachments.len()
                || !cached
                    .envelope
                    .attachments
                    .iter()
                    .all(|file| original.attachments.iter().any(|outer| outer.id == file.id))
            {
                return Err("The original encrypted message metadata does not match its authenticated context.".into());
            }
            if content.trim().is_empty() && cached.envelope.attachments.is_empty() {
                return Err("Write a message or keep an attached file.".into());
            }
            crypto.bind_delivery(&original.content, &original.id, original.sequence)?;
            let mut envelope = cached.envelope;
            envelope.content = content;
            let client_id = envelope.client_id.clone();
            // The completed send outbox is clear. A pending edit with this same
            // original ID returns its saved wire bytes on an identical retry.
            let wire = crypto.encrypted_edit_content(envelope)?;
            (wire, client_id)
        };
        let response = self
            .request(
                reqwest::Method::PATCH,
                &format!("/api/messages/{message_id}"),
            )?
            .json(&EditMessage {
                content: protected_content.clone(),
            })
            .send()
            .map_err(network_error)?;
        match checked_write(response) {
            Ok(response) => {
                let acknowledgement: Message = read_json(response, 2 * 1024 * 1024)?;
                if acknowledgement.id != original.id
                    || acknowledgement.sequence != original.sequence
                    || acknowledgement.author.id != own_user
                    || acknowledgement.client_id.as_deref() != Some(client_id.as_str())
                    || acknowledgement.channel_id != original.channel_id
                    || acknowledgement.recipient_id != original.recipient_id
                    || acknowledgement.content != protected_content
                    || acknowledgement.attachments.len() != original.attachments.len()
                    || !original.attachments.iter().all(|attachment| {
                        acknowledgement
                            .attachments
                            .iter()
                            .any(|returned| returned.id == attachment.id)
                    })
                {
                    return Err(
                        "The encrypted edit acknowledgement does not match the submitted message."
                            .into(),
                    );
                }
                let crypto = self
                    .connection
                    .as_mut()
                    .unwrap()
                    .crypto
                    .as_mut()
                    .ok_or("The encryption session ended before this edit was acknowledged.")?;
                crypto.bind_delivery(
                    &protected_content,
                    &acknowledgement.id,
                    acknowledgement.sequence,
                )?;
                crypto.committed(&client_id)?;
                Ok(true)
            }
            Err(WriteFailure::Error(error)) => Err(error),
            Err(WriteFailure::RateLimited(cooldown)) => {
                if let Some(c) = &mut self.connection {
                    if c.chat.edits.len() < 128 {
                        c.chat.edits.insert(message_id.to_owned(), cooldown.clone());
                    }
                }
                self.emit_edit_cooldown(message_id.to_owned(), cooldown);
                Ok(false)
            }
        }
    }

    fn emit_edit_cooldown(&self, message_id: String, cooldown: Cooldown) {
        self.emit(Update::EditRateLimited {
            message_id,
            error: cooldown.error.clone(),
            code: cooldown.code.clone(),
            retry_after_ms: cooldown.remaining_ms(Instant::now()).max(1),
        });
    }

    fn emit_send_cooldown(&self, target: Target, cooldown: Cooldown) {
        self.emit(Update::SendRateLimited {
            target,
            origin: self.connection.as_ref().map(CommandOrigin::from),
            error: cooldown.error.clone(),
            code: cooldown.code.clone(),
            retry_after_ms: cooldown.remaining_ms(Instant::now()).max(1),
        });
    }

    fn fetch_pins(&mut self, channel_id: &str, request_id: u64) -> Result<(), String> {
        path_id(channel_id)?;
        let c = self
            .connection
            .as_ref()
            .ok_or("Connect to a server first.")?;
        if !c.chat.supports("pins-v1") {
            return Err("Update this server to use pinned messages.".into());
        }
        let url = c.url.clone();
        let mut pins: Vec<PinnedMessage> = read_json(
            checked(
                self.request(
                    reqwest::Method::GET,
                    &format!("/api/channels/{channel_id}/pins"),
                )?
                .send()
                .map_err(network_error)?,
            )?,
            8 * 1024 * 1024,
        )?;
        if pins.len() > MAX_CHANNEL_PINS {
            return Err("The server returned too many pinned messages.".into());
        }
        if pins.iter().any(|pin| {
            pin.message.channel_id.as_deref() != Some(channel_id)
                || pin.message.recipient_id.is_some()
        }) {
            return Err("The server returned pins for another conversation.".into());
        }
        let target = Target::Channel(channel_id.into());
        if self
            .connection
            .as_ref()
            .is_some_and(|c| c.chat.supports("e2ee_mls_v1") && !c.chat.plain_allowed())
        {
            let _ = self.ensure_crypto_conversation(&target);
        }
        let mut messages: Vec<_> = pins.iter().map(|pin| pin.message.clone()).collect();
        self.decrypt_messages(&target, &mut messages);
        pins.retain_mut(|pin| {
            if let Some(message) = messages.iter().find(|m| m.id == pin.message.id) {
                pin.message = message.clone();
                true
            } else {
                false
            }
        });
        self.emit(Update::PinsLoaded {
            url,
            channel_id: channel_id.to_owned(),
            pins,
            request_id,
        });
        Ok(())
    }

    fn send_typing(&mut self, target: Target, typing: bool) -> Result<(), String> {
        let Some(c) = &self.connection else {
            return Ok(());
        };
        if !c.chat.supports("typing-v1") {
            return Ok(());
        }
        if typing && c.current.as_ref() != Some(&target) {
            return Ok(());
        }
        let now = Instant::now();
        if let Some((old_target, old_typing, at)) = &c.chat.sent_typing {
            if old_target == &target
                && *old_typing == typing
                && (!typing || now.duration_since(*at) < Duration::from_secs(3))
            {
                return Ok(());
            }
            if !typing && (old_target != &target || !old_typing) {
                return Ok(());
            }
        } else if !typing {
            return Ok(());
        }
        let (channel_id, peer_id) = target_ids(&target);
        if let Some(c) = &mut self.connection {
            c.chat.sent_typing = Some((target, typing, now));
        }
        checked(
            self.request(reqwest::Method::POST, "/api/typing")?
                .timeout(Duration::from_millis(750))
                .json(&TypingRequest {
                    channel_id,
                    peer_id,
                    typing,
                })
                .send()
                .map_err(network_error)?,
        )?;
        Ok(())
    }

    pub(super) fn clear_typing_selection(&mut self) {
        let pending = self
            .connection
            .as_ref()
            .and_then(|c| c.chat.sent_typing.as_ref())
            .filter(|(_, typing, _)| *typing)
            .map(|(target, _, _)| target.clone());
        if let Some(target) = pending {
            let _ = self.send_typing(target, false);
        }
        if let Some(c) = &mut self.connection {
            c.chat.sent_typing = None;
            c.chat.typing_state = TypingState::default();
            c.chat.typing_received_at = None;
            c.chat.typing_dirty = false;
            c.chat.typing_polled_at = None;
        }
    }

    pub(super) fn poll_chat_events(&mut self) {
        let Some(c) = &mut self.connection else {
            return;
        };
        if self.socket_stop.load(Ordering::SeqCst)
            || self.status == Some(ConnectionStatus::SignInRequired)
        {
            return;
        }
        let events: Vec<_> = c
            .chat
            .events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .collect();
        let url = c.url.clone();
        let current = c.current.clone();
        let self_id = c.session.user.id.clone();
        let mut pin_channels = HashSet::new();
        for event in events {
            if event.kind == "typing" && c.chat.supports("typing-v1") {
                if let Some(target) = typing_target(&event, &self_id) {
                    if current.as_ref() == Some(&target)
                        && match &target {
                            Target::Channel(id) => c.chat.channels.contains_key(id),
                            Target::Direct(_) => true,
                        }
                    {
                        c.chat.typing_dirty = true;
                    }
                }
            } else if event.kind == "pins"
                && c.chat.supports("pins-v1")
                && event.recipient_id.is_none()
            {
                if let Some(channel_id) = event
                    .channel_id
                    .filter(|id| c.chat.channels.contains_key(id))
                {
                    pin_channels.insert(channel_id);
                }
            }
        }
        // Convert server-relative TTL to elapsed monotonic time. The client's
        // wall clock may differ substantially from the server's clock.
        let now = Instant::now();
        let mut expired_update = None;
        if let Some(received) = c.chat.typing_received_at {
            let elapsed = now
                .duration_since(received)
                .as_millis()
                .min(u64::MAX as u128) as u64;
            let was = c.chat.typing_state.users.len();
            let state_time = c.chat.typing_state.server_time_ms;
            c.chat
                .typing_state
                .users
                .retain(|user| user.expires_at_ms.saturating_sub(state_time) > elapsed);
            if c.chat.typing_state.users.len() != was {
                let mut state = c.chat.typing_state.clone();
                state.server_time_ms = state_time.saturating_add(elapsed);
                expired_update = current.clone().map(|target| Update::Typing {
                    url: url.clone(),
                    target,
                    state,
                });
            }
        }
        let due = c
            .chat
            .typing_polled_at
            .is_none_or(|at| now.duration_since(at) >= Duration::from_millis(500));
        let should_poll = c.chat.supports("typing-v1")
            && current.is_some()
            && due
            && (c.chat.typing_dirty
                || (!c.chat.typing_state.users.is_empty()
                    && c.chat
                        .typing_polled_at
                        .is_none_or(|at| now.duration_since(at) >= Duration::from_secs(2))));
        if should_poll {
            c.chat.typing_dirty = false;
            c.chat.typing_polled_at = Some(now);
        }
        for channel_id in pin_channels {
            self.emit(Update::PinsChanged {
                url: url.clone(),
                channel_id,
            });
        }
        if let Some(update) = expired_update {
            self.emit(update);
        }
        if should_poll {
            if let Some(target) = current {
                let result = (|| {
                    let request = self
                        .request(reqwest::Method::GET, "/api/typing")?
                        .timeout(Duration::from_millis(750));
                    let request = match &target {
                        Target::Channel(id) => request.query(&[("channel_id", id)]),
                        Target::Direct(id) => request.query(&[("peer_id", id)]),
                    };
                    parse::<TypingState>(request.send().map_err(network_error)?)
                })();
                let state = filtered_typing(result.unwrap_or_default(), &self_id);
                if let Some(c) = &mut self.connection {
                    c.chat.typing_state = state.clone();
                    c.chat.typing_received_at = Some(Instant::now());
                }
                self.emit(Update::Typing { url, target, state });
            }
        }
    }

    pub(super) fn cancel_transfers(&mut self) {
        let _commit = self
            .transfer_commit
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        self.transfer_epoch.fetch_add(1, Ordering::SeqCst);
        for cancel in self.transfers.values() {
            cancel.store(true, Ordering::SeqCst);
        }
        self.transfers.clear();
    }

    pub(super) fn publish_communities(&self) {
        let info = |c: &Connection, current| crate::community::CommunityInfo {
            url: c.url.clone(),
            server_id: c.server_id.clone(),
            name: c.name.clone(),
            user: c.session.user.clone(),
            current,
        };
        let mut communities: Vec<_> = self
            .saved_connections
            .values()
            .map(|s| info(&s.connection, false))
            .collect();
        if let Some(c) = &self.connection {
            communities.push(info(c, true));
        }
        communities.sort_by(|a, b| a.url.cmp(&b.url));
        self.emit(Update::Communities(communities));
    }

    pub(super) fn park_connection(&mut self, replacing: &str) {
        self.clear_typing_selection();
        self.cancel_transfers();
        if self.connection.is_some() {
            let _ = self.post_empty("/api/voice/leave", &serde_json::json!({}));
        }
        if let Some(mut c) = self.connection.take() {
            c.voice_channel = None;
            c.voice_attempt = None;
            c.voice_admission = None;
            c.voice_recovery = None;
            c.crypto_pending_join = None;
            c.crypto_call_epoch = None;
            c.sharing = false;
            c.camera = false;
            c.frame_ids.clear();
            if c.url == replacing {
                self.socket_stop.store(true, Ordering::SeqCst);
                let _ = self
                    .http
                    .post(crate::embedded_network::endpoint(&format!(
                        "{}/api/logout",
                        c.url
                    )))
                    .bearer_auth(c.session.token)
                    .json(&serde_json::json!({}))
                    .send();
            } else {
                self.saved_connections.insert(
                    c.url.clone(),
                    SavedConnection {
                        connection: c,
                        stop: self.socket_stop.clone(),
                        dirty: self.refresh_pending.clone(),
                    },
                );
            }
        }
        if let Some(old) = self.saved_connections.remove(replacing) {
            old.stop.store(true, Ordering::SeqCst);
            let _ = self
                .http
                .post(crate::embedded_network::endpoint(&format!(
                    "{}/api/logout",
                    old.connection.url
                )))
                .bearer_auth(old.connection.session.token)
                .json(&serde_json::json!({}))
                .send();
        }
        self.status = None;
    }

    pub(super) fn set_status(&mut self, status: ConnectionStatus) {
        if self.status.as_ref() == Some(&status) {
            return;
        }
        self.status = Some(status.clone());
        if let Some(c) = &self.connection {
            self.emit(Update::ConnectionState {
                url: c.url.clone(),
                status,
            });
        }
    }

    pub(super) fn handle_extended(&mut self, command: Command) -> Result<(), String> {
        match command {
            Command::AccountLoginBegin {
                authority,
                url,
                expected_server_id,
                invite,
                register,
                initial_protection,
                request_id,
            } => self.account_login_begin(
                authority,
                url,
                expected_server_id,
                invite,
                register,
                initial_protection,
                request_id,
            )?,
            Command::AccountLoginFinish {
                authority,
                url,
                expected_server_id,
                challenge_id,
                proof,
                cancel,
                request_id,
            } => self.account_login_finish(
                authority,
                url,
                expected_server_id,
                challenge_id,
                proof,
                cancel,
                request_id,
            )?,
            Command::FetchUserProfile {
                user_id,
                request_id,
            } => {
                path_id(&user_id)?;
                let user: User = self.get(&format!("/api/profiles/{user_id}"))?;
                if user.id != user_id {
                    return Err("The profile response did not match this person.".into());
                }
                let url = self
                    .connection
                    .as_ref()
                    .ok_or("Not connected.")?
                    .url
                    .clone();
                self.emit(Update::ProfileLoaded {
                    url,
                    user,
                    request_id,
                });
            }
            Command::SaveProfile {
                origin,
                display_name,
                bio,
                appearance,
                avatar,
                request_id,
            } => {
                self.start_profile_save(&origin, display_name, bio, appearance, avatar, request_id)?
            }
            Command::SelectConversation(target) => {
                if self.connection.as_ref().and_then(|c| c.current.as_ref()) != Some(&target) {
                    self.clear_typing_selection();
                }
                if let Some(c) = &mut self.connection {
                    c.current = Some(target);
                    c.chat.typing_dirty = c.chat.supports("typing-v1");
                }
            }
            Command::Typing { target, typing } => self.send_typing(target, typing)?,
            Command::FetchPins {
                channel_id,
                request_id,
            } => self.fetch_pins(&channel_id, request_id)?,
            Command::SetPin {
                channel_id,
                message_id,
                pinned,
                request_id,
            } => {
                path_id(&channel_id)?;
                path_id(&message_id)?;
                let c = self
                    .connection
                    .as_ref()
                    .ok_or("Connect to a server first.")?;
                if !c.chat.supports("pins-v1") {
                    return Err("Update this server to use pinned messages.".into());
                }
                checked(
                    self.request(
                        if pinned {
                            reqwest::Method::PUT
                        } else {
                            reqwest::Method::DELETE
                        },
                        &format!("/api/channels/{channel_id}/pins/{message_id}"),
                    )?
                    .send()
                    .map_err(network_error)?,
                )?;
                self.fetch_pins(&channel_id, request_id).map_err(|error| {
                    format!("Pin updated, but the list could not refresh: {error}")
                })?;
                if self.connection.as_ref().and_then(|c| c.current.as_ref())
                    == Some(&Target::Channel(channel_id))
                {
                    if let Err(error) = self.load_messages() {
                        self.emit(Update::Error(error));
                    }
                }
            }
            Command::RememberSession { enabled } => {
                if let Some(c) = &mut self.connection {
                    if enabled {
                        crate::credential_vault::save_bound(
                            &c.url,
                            &c.server_id,
                            &c.session,
                            c.chat
                                .authority
                                .as_ref()
                                .map(|pin| crate::host_route::RememberedAuthority {
                                    pin: pin.clone(),
                                    account_provider: c.chat.account_provider.clone(),
                                    previous_urls: c.chat.authority_aliases.clone(),
                                })
                                .as_ref(),
                        )?;
                    } else {
                        crate::credential_vault::forget(&c.url)?;
                    }
                    c.remembered = enabled;
                }
                self.remember_session = enabled;
                if self.connection.is_some() {
                    self.emit(Update::Notice(
                        if enabled {
                            "Relay will remember this account on this PC."
                        } else {
                            "Relay will ask you to sign in next time."
                        }
                        .into(),
                    ));
                }
            }
            Command::RememberNextLogins { enabled } => {
                self.remember_session = enabled;
            }
            Command::RememberAccountSessions { enabled, urls } => {
                let urls = urls.iter()
                    .map(|url| crate::community::normalize_url(url))
                    .collect::<Result<HashSet<_>, _>>()?;
                self.remember_session = enabled;
                let mut failure = None;
                for url in urls {
                    let connection = self.connection.as_mut().filter(|c| c.url == url)
                        .or_else(|| self.saved_connections.get_mut(&url).map(|s| &mut s.connection));
                    let result = if let Some(connection) = connection {
                        // A session which could not be kept, or was opted out,
                        // is revoked on normal close even if storage failed.
                        connection.remembered = false;
                        if enabled {
                            let authority = connection.chat.authority.as_ref().map(|pin|
                                crate::host_route::RememberedAuthority {
                                    pin: pin.clone(),
                                    account_provider: connection.chat.account_provider.clone(),
                                    previous_urls: connection.chat.authority_aliases.clone(),
                                });
                            crate::credential_vault::save_bound(&url, &connection.server_id,
                                &connection.session, authority.as_ref())
                                .map(|()| connection.remembered = true)
                        } else {
                            crate::credential_vault::forget(&url)
                        }
                    } else if !enabled {
                        crate::credential_vault::forget(&url)
                    } else {
                        Ok(())
                    };
                    if let Err(error) = result { failure.get_or_insert(error); }
                }
                if let Some(error) = failure {
                    return Err(format!("The saved-login choice could not be applied to every server: {error}"));
                }
            }
            Command::ForgetAccountSessions { urls, request_id } => {
                let result = (|| {
                    let urls = urls.iter().map(|url| crate::community::normalize_url(url))
                        .collect::<Result<HashSet<_>, _>>()?;
                    let mut failure = None;
                    for url in urls {
                        // Acknowledgement means the protected credential was
                        // deleted, including saved-only URLs with no live UI.
                        if let Err(error) = crate::credential_vault::forget(&url) {
                            failure.get_or_insert(error);
                        }
                        if let Err(error) = self.handle(Command::ForgetCommunity { url }) {
                            failure.get_or_insert(error);
                        }
                    }
                    failure.map_or(Ok(()), Err)
                })();
                self.emit(Update::AccountSessionsForgotten { request_id, result });
            }
            Command::KeepSignedInForMove { origin } => {
                self.command_connection(&origin)?;
                let c = self
                    .connection
                    .as_mut()
                    .ok_or("Connect to a community first.")?;
                if !c.remembered {
                    let authority =
                        c.chat
                            .authority
                            .as_ref()
                            .map(|pin| crate::host_route::RememberedAuthority {
                                pin: pin.clone(),
                                account_provider: c.chat.account_provider.clone(),
                                previous_urls: c.chat.authority_aliases.clone(),
                            });
                    crate::credential_vault::save_bound(
                        &c.url,
                        &c.server_id,
                        &c.session,
                        authority.as_ref(),
                    )?;
                    c.remembered = true;
                }
            }
            Command::Resume {
                url,
                expected_server_id,
            } => {
                let url = crate::community::normalize_url(&url)?;
                let remembered = crate::credential_vault::load(&url)?
                    .ok_or("No remembered login exists for this server. Sign in first.")?;
                remembered.confirmed_server_id(expected_server_id.as_deref())?;
                if remembered.authority.is_some() {
                    self.begin_host_resume(url, remembered)?;
                } else {
                    self.resume_same_host(url, remembered, expected_server_id)?;
                }
            }
            Command::History {
                target,
                before,
                request_id,
            } => {
                let page = self.history(&target, before, None)?;
                self.emit(Update::History {
                    target,
                    page,
                    request_id,
                });
            }
            Command::HistoryAfter {
                origin,
                target,
                after,
                request_id,
            } => {
                let connection = self.command_connection(&origin)?;
                if connection.protocol_version < API_PROTOCOL_VERSION {
                    return Err("Update this server to open the first unread message.".into());
                }
                let mut request = self
                    .request(reqwest::Method::GET, "/api/history")?
                    .query(&[("after", after.to_string()), ("limit", "100".to_string())]);
                request = match &target {
                    Target::Channel(id) => request.query(&[("channel_id", id)]),
                    Target::Direct(id) => request.query(&[("peer_id", id)]),
                };
                let mut page: HistoryPage = read_json(
                    checked(request.send().map_err(network_error)?)?,
                    8 * 1024 * 1024,
                )?;
                let next_after =
                    crate::unread_navigation::next_after(&page, &target, &origin.user_id, after)?;
                if !self
                    .connection
                    .as_ref()
                    .is_some_and(|c| c.chat.plain_allowed())
                    && page
                        .messages
                        .iter()
                        .any(|m| m.content.starts_with(crate::e2ee::MESSAGE_PREFIX))
                {
                    self.ensure_crypto_conversation(&target)?;
                }
                self.command_connection(&origin)?;
                let connection = self
                    .connection
                    .as_mut()
                    .ok_or("Connect to a community first.")?;
                let protections = super::encrypted::decrypt_in_connection(
                    connection,
                    &target,
                    &mut page.messages,
                );
                let unavailable = protections.iter().any(|(_, protection)| {
                    matches!(protection, crate::e2ee::MessageProtection::Unavailable(_))
                });
                self.emit(Update::MessageProtections {
                    origin: origin.clone(),
                    protections,
                });
                if unavailable {
                    return Err("Some unread messages could not be decrypted. Try again or open the latest messages.".into());
                }
                self.emit(Update::UnreadHistory {
                    origin,
                    target,
                    page,
                    next_after,
                    request_id,
                });
            }
            Command::AroundMessage {
                target,
                message_id,
                request_id,
            } => {
                let mut request = self
                    .request(reqwest::Method::GET, "/api/history")?
                    .query(&[("around", &message_id), ("limit", &"100".to_string())]);
                request = match &target {
                    Target::Channel(id) => request.query(&[("channel_id", id)]),
                    Target::Direct(id) => request.query(&[("peer_id", id)]),
                };
                let mut page: HistoryPage = read_json(
                    checked(request.send().map_err(network_error)?)?,
                    8 * 1024 * 1024,
                )?;
                if self
                    .connection
                    .as_ref()
                    .is_some_and(|c| c.chat.supports("e2ee_mls_v1") && !c.chat.plain_allowed())
                {
                    let _ = self.ensure_crypto_conversation(&target);
                }
                self.decrypt_messages(&target, &mut page.messages);
                self.emit(Update::History {
                    target,
                    page,
                    request_id,
                });
            }
            Command::Search {
                target,
                query,
                before,
                request_id,
            } => {
                let query = query.trim().to_string();
                if query.chars().count() > 200 {
                    return Err("Search can contain up to 200 characters.".into());
                }
                let page = self.history(&target, before, Some(&query))?;
                self.emit(Update::SearchResults {
                    target,
                    query,
                    page,
                    request_id,
                });
            }
            Command::MarkCommunityRead { origin } => {
                self.mark_community_read(&origin)?;
            }
            Command::MarkRead {
                origin,
                target,
                last_read_sequence,
            } => {
                let result = self.mark_read(&origin, &target, last_read_sequence);
                self.emit(Update::ReadMarked {
                    origin, target, requested_sequence: last_read_sequence, result,
                });
            }
            Command::SendRich {
                origin,
                binding,
                target,
                content,
                attachment_ids,
                client_id,
                reply_to_id,
                mentions,
            } => {
                self.command_connection(&origin)?;
                if let Some(binding) = &binding {
                    self.validate_send_binding(binding)?;
                }
                if client_id.is_empty() || client_id.len() > 100 {
                    return Err("This message needs a valid retry identifier.".into());
                }
                let cooldown = self
                    .connection
                    .as_mut()
                    .and_then(|c| c.chat.send_cooldown(&target, &client_id, Instant::now()));
                if let Some(cooldown) = cooldown {
                    self.emit_send_cooldown(target, cooldown);
                    return Ok(());
                }
                let (channel_id, recipient_id) = target_ids(&target);
                let plain = self
                    .connection
                    .as_ref()
                    .is_some_and(|c| c.chat.plain_allowed());
                if plain {
                    // A file prepared while encryption was on keeps its key only inside
                    // a protected message. Sent plainly, nobody could open it.
                    if let Some(crypto) = self.connection.as_ref().and_then(|c| c.crypto.as_ref()) {
                        for id in &attachment_ids {
                            if crypto.file(id)?.is_some() {
                                return Err("This file was prepared with encryption. Remove it and attach it again.".into());
                            }
                        }
                    }
                }
                let (protected_content, wire_mentions) = if plain {
                    (content.clone(), mentions)
                } else {
                    (
                        self.encrypt_chat(
                            &target,
                            content.clone(),
                            &attachment_ids,
                            &client_id,
                            reply_to_id.clone(),
                            mentions,
                        )?,
                        Vec::new(),
                    )
                };
                let body = SendMessage {
                    channel_id,
                    recipient_id,
                    content: protected_content,
                    attachment_ids: attachment_ids.clone(),
                    client_id: Some(client_id.clone()),
                    reply_to_id: reply_to_id.clone(),
                    mentions: wire_mentions,
                };
                // A lost HTTP response can safely be retried with the same identifier.
                // Do not retry a definite server rejection (permissions, validation, etc.).
                let first = self
                    .request(reqwest::Method::POST, "/api/messages")?
                    .json(&body)
                    .send();
                let response = match first {
                    Ok(response) => response,
                    Err(error)
                        if self
                            .connection
                            .as_ref()
                            .is_some_and(|c| c.protocol_version >= API_PROTOCOL_VERSION)
                            && (error.is_connect()
                                || error.is_timeout()
                                || error.is_body()
                                || error.is_request()) =>
                    {
                        self.request(reqwest::Method::POST, "/api/messages")?
                            .json(&body)
                            .send()
                            .map_err(network_error)?
                    }
                    Err(error) => return Err(network_error(error)),
                };
                let response = match checked_write(response) {
                    Ok(response) => response,
                    Err(WriteFailure::Error(error)) => return Err(error),
                    Err(WriteFailure::RateLimited(cooldown)) => {
                        if let Some(c) = &mut self.connection {
                            c.chat
                                .remember_send_cooldown(target.clone(), cooldown.clone());
                        }
                        self.emit_send_cooldown(target, cooldown);
                        return Ok(());
                    }
                };
                let message: Message = read_json(response, 2 * 1024 * 1024)?;
                if message.content != body.content
                    || message.client_id != body.client_id
                    || message.author.id != origin.user_id
                    || message.channel_id != body.channel_id
                    || message.recipient_id != body.recipient_id
                    || message.reply_to_id != body.reply_to_id
                    || (if plain {
                        message.mentions.iter().any(|m| !body.mentions.contains(m))
                    } else {
                        !message.mentions.is_empty()
                    })
                    || message.attachments.len() != body.attachment_ids.len()
                    || !message
                        .attachments
                        .iter()
                        .all(|a| body.attachment_ids.contains(&a.id))
                {
                    return Err("The encrypted message acknowledgement does not match its original request.".into());
                }
                if !plain {
                    let crypto = self.crypto()?;
                    crypto.bind_delivery(&body.content, &message.id, message.sequence)?;
                    crypto.committed(&client_id)?;
                }
                if let Some(c) = &mut self.connection {
                    let first_ack = c.chat.remember_committed(&client_id);
                    if let Target::Channel(id) = &target {
                        if let Some(&(seconds, false)) = c.chat.channels.get(id) {
                            if first_ack && seconds > 0 {
                                c.chat.remember_send_cooldown(
                                    target.clone(),
                                    Cooldown {
                                        error: "This channel has slowmode enabled.".into(),
                                        code: "slowmode".into(),
                                        until: Instant::now()
                                            + Duration::from_secs(u64::from(seconds)),
                                        committed_client_id: Some(client_id.clone()),
                                    },
                                );
                            }
                        }
                    }
                }
                self.emit(Update::SentRich {
                    origin,
                    target,
                    client_id,
                });
                if let Err(error) = self.load_messages() {
                    self.emit(Update::Error(error));
                }
            }
            Command::StartUpload {
                origin,
                target,
                path,
                operation_id,
            } => {
                if let Err(error) =
                    self.start_upload(&origin, target.clone(), path, operation_id.clone())
                {
                    self.emit(Update::UploadFailed {
                        target,
                        operation_id,
                        error,
                        cancelled: false,
                    });
                }
            }
            Command::CancelUpload { operation_id } => {
                let _commit = self
                    .transfer_commit
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if let Some(cancel) = self.transfers.get(&operation_id) {
                    cancel.store(true, Ordering::SeqCst);
                }
            }
            Command::SaveBackup { path, operation_id } => {
                if let Err(error) = self.start_backup(path, operation_id.clone()) {
                    self.emit(Update::BackupFailed {
                        operation_id,
                        error,
                        cancelled: false,
                    });
                }
            }
            Command::StartDownload {
                origin,
                attachment,
                path,
                operation_id,
            } => {
                if let Err(error) =
                    self.start_download(&origin, attachment, path, operation_id.clone())
                {
                    self.emit(Update::DownloadFailed {
                        operation_id,
                        error,
                        cancelled: false,
                    });
                }
            }
            Command::SwitchCommunity { url } => {
                let url = crate::community::normalize_url(&url)?;
                if self.connection.as_ref().is_some_and(|c| c.url == url) {
                    return Ok(());
                }
                let saved = self
                    .saved_connections
                    .get(&url)
                    .ok_or("Sign in to this community first.")?;
                let super::call_alerts::BootstrapReply { bootstrap, cursor } =
                    match parse::<super::call_alerts::BootstrapReply>(
                        self.http
                            .get(crate::embedded_network::endpoint(&format!(
                                "{url}/api/bootstrap"
                            )))
                            .bearer_auth(&saved.connection.session.token)
                            .send()
                            .map_err(network_error)?,
                    ) {
                        Ok(bootstrap) => bootstrap,
                        Err(error) => {
                            if error == "Sign in again" {
                                self.reject_saved_session(&url);
                            }
                            return Err(error);
                        }
                    };
                let call_received = Instant::now();
                // Validate the destination before touching the active community.
                if let Err(error) = validate_bootstrap_identity(
                    &saved.connection.server_id,
                    &saved.connection.session.user.id,
                    &bootstrap,
                ) {
                    self.reject_saved_identity(&url);
                    return Err(error);
                }
                self.record_alert_bootstrap(
                    &CommandOrigin::from(&saved.connection),
                    &bootstrap,
                    call_received,
                    cursor,
                );
                // Destination IO must not block the active call's media gate.
                // Serialize only the validated switch and its media invalidation.
                let gate = self.live.gate.clone();
                let _gate = gate
                    .lock()
                    .map_err(|_| "The media worker stopped.".to_string())?;
                self.live.epoch.fetch_add(1, Ordering::SeqCst);
                if let Ok(mut context) = self.live.context.lock() {
                    *context = None;
                }
                let mut saved = self
                    .saved_connections
                    .remove(&url)
                    .expect("saved community validated above");
                self.park_connection("");
                self.chat_media_epoch.fetch_add(1, Ordering::SeqCst);
                self.chat_media_workers = None;
                self.legacy_reactions.clear();
                saved.connection.name = bootstrap.server_name.clone();
                saved.connection.server_id = bootstrap.server_id.clone();
                saved.connection.protocol_version = bootstrap.protocol_version;
                saved.connection.chat.update_bootstrap(&bootstrap);
                self.socket_stop = saved.stop;
                self.refresh_pending = saved.dirty;
                let session = saved.connection.session.clone();
                self.connection = Some(saved.connection);
                self.sync_live_transport()?;
                self.emit(Update::Connected {
                    url,
                    session,
                    bootstrap: Box::new(bootstrap),
                });
                self.publish_communities();
            }
            Command::ForgetCommunity { url } => {
                let url = crate::community::normalize_url(&url)?;
                let _ = crate::credential_vault::forget(&url);
                if self.connection.as_ref().is_some_and(|c| c.url == url) {
                    self.cancel_transfers();
                    self.live.epoch.fetch_add(1, Ordering::SeqCst);
                    if let Ok(mut context) = self.live.context.lock() {
                        *context = None;
                    }
                    let gate = self.live.gate.clone();
                    let _guard = gate.lock().map_err(|_| "The media worker stopped.")?;
                    let _ = self.post_empty("/api/voice/leave", &serde_json::json!({}));
                    let _ = self.post_empty("/api/logout", &serde_json::json!({}));
                    self.connection = None;
                    self.chat_media_epoch.fetch_add(1, Ordering::SeqCst);
                    self.chat_media_workers = None;
                    self.legacy_reactions.clear();
                    self.socket_stop.store(true, Ordering::SeqCst);
                    self.emit(Update::Disconnected);
                }
                if let Some(saved) = self.saved_connections.remove(&url) {
                    saved.stop.store(true, Ordering::SeqCst);
                    let _ = self
                        .http
                        .post(crate::embedded_network::endpoint(&format!(
                            "{url}/api/logout"
                        )))
                        .bearer_auth(saved.connection.session.token)
                        .json(&serde_json::json!({}))
                        .send();
                }
                self.publish_communities();
            }
            Command::DiscordHost {
                origin,
                action,
                request_id,
            } => {
                self.discord_host(origin, action, request_id)?;
            }
            Command::Admin {
                origin,
                method,
                path,
                body,
                request_id,
            } => {
                self.command_connection(&origin)?;
                if !valid_admin_path(&path) {
                    return Err("Invalid administration route.".into());
                }
                let method = match method {
                    AdminMethod::Get => reqwest::Method::GET,
                    AdminMethod::Post => reqwest::Method::POST,
                    AdminMethod::Put => reqwest::Method::PUT,
                    AdminMethod::Patch => reqwest::Method::PATCH,
                    AdminMethod::Delete => reqwest::Method::DELETE,
                };
                let response = checked(
                    self.request(method, &path)?
                        .json(&body)
                        .send()
                        .map_err(network_error)?,
                )?;
                let data = if response.status() == reqwest::StatusCode::NO_CONTENT {
                    serde_json::Value::Null
                } else {
                    read_json(response, 2 * 1024 * 1024)?
                };
                self.emit(Update::AdminResult { request_id, data });
                self.refresh();
            }
            Command::ReplicaToken {
                origin,
                action,
                replica_id,
                request_id,
            } => self.replica_token(origin, action, replica_id, request_id)?,
            Command::MentionInbox {
                origin,
                remaining,
                request_id,
            } => {
                self.command_connection(&origin)?;
                let bootstrap: Bootstrap = self.get("/api/bootstrap")?;
                let mut queue: VecDeque<_> = remaining
                    .unwrap_or_else(|| {
                        bootstrap
                            .read_states
                            .iter()
                            .filter(|r| r.last_read_sequence > 0 || r.unread_count > 0)
                            .filter_map(|r| {
                                let target = match (&r.channel_id, &r.peer_id) {
                                    (Some(id), None)
                                        if bootstrap
                                            .channels
                                            .iter()
                                            .any(|c| c.id == *id && c.kind == "text") =>
                                    {
                                        Target::Channel(id.clone())
                                    }
                                    (None, Some(id)) if !bootstrap.social.blocked.contains(id) => {
                                        Target::Direct(id.clone())
                                    }
                                    _ => return None,
                                };
                                Some((target, None))
                            })
                            .collect()
                    })
                    .into();
                let mut messages = vec![];
                let mut errors = vec![];
                for _ in 0..10 {
                    let Some((target, before)) = queue.pop_front() else {
                        break;
                    };
                    match self.history(&target, before, None) {
                        Ok(page) => {
                            for message in page.messages {
                                if message.mentions.contains(&bootstrap.user.id)
                                    && !bootstrap.social.blocked.contains(&message.author.id)
                                {
                                    messages.push((target.clone(), message));
                                }
                            }
                            if let Some(next) = page.next_before {
                                if Some(next) != before {
                                    queue.push_back((target, Some(next)));
                                }
                            }
                        }
                        Err(error) => errors.push(format!("{target:?}: {error}")),
                    }
                }
                self.emit(Update::MentionInbox {
                    request_id,
                    messages,
                    remaining: queue.into(),
                    errors,
                });
            }
            Command::CommunitySearch {
                origin,
                query,
                filters,
                remaining,
                request_id,
            } => {
                self.command_connection(&origin)?;
                let query = query.trim().to_owned();
                if (query.is_empty() && !filters.active()) || query.chars().count() > 300 {
                    return Err("Enter a search of up to 300 characters or choose a filter.".into());
                }
                let bootstrap: Bootstrap = self.get("/api/bootstrap")?;
                let available: HashSet<_> = bootstrap
                    .read_states
                    .iter()
                    .filter_map(|r| match (&r.channel_id, &r.peer_id) {
                        (Some(id), None)
                            if bootstrap
                                .channels
                                .iter()
                                .any(|c| c.id == *id && c.kind == "text") =>
                        {
                            Some(Target::Channel(id.clone()))
                        }
                        (None, Some(id)) if !bootstrap.social.blocked.contains(id) => {
                            Some(Target::Direct(id.clone()))
                        }
                        _ => None,
                    })
                    .collect();
                let candidates = remaining.unwrap_or_else(|| {
                    bootstrap
                        .read_states
                        .iter()
                        .filter_map(|r| {
                            let target = match (&r.channel_id, &r.peer_id) {
                                (Some(id), None) => Target::Channel(id.clone()),
                                (None, Some(id)) => Target::Direct(id.clone()),
                                _ => return None,
                            };
                            available.contains(&target).then_some((target, None))
                        })
                        .collect()
                });
                if candidates.len() > 4096 {
                    return Err("Too many conversations to scan in one search.".into());
                }
                let mut seen = HashSet::new();
                let mut queue: VecDeque<_> = candidates
                    .into_iter()
                    .filter(|(target, _)| {
                        available.contains(target)
                            && filters.includes_target(target)
                            && seen.insert(target.clone())
                    })
                    .collect();
                let needle = query.to_lowercase();
                let mut messages = Vec::new();
                let mut errors = Vec::new();
                for _ in 0..10 {
                    let Some((target, before)) = queue.pop_front() else {
                        break;
                    };
                    match self.search_scan_page(&target, before) {
                        Ok((page, unavailable)) => {
                            if unavailable > 0 {
                                errors.push(format!("{target:?}: {unavailable} messages could not be decrypted. Open this conversation to check device verification, then search again."));
                            }
                            for message in page.messages {
                                if !bootstrap.social.blocked.contains(&message.author.id)
                                    && filters.matches(&target, &message, &needle)
                                {
                                    messages.push((target.clone(), message));
                                }
                            }
                            if let Some(next) =
                                page.next_before.filter(|next| Some(*next) != before)
                            {
                                queue.push_back((target, Some(next)));
                            }
                        }
                        Err(error) => errors.push(format!("{target:?}: {error}")),
                    }
                }
                self.emit(Update::CommunitySearch {
                    request_id,
                    query,
                    messages,
                    remaining: queue.into(),
                    errors,
                });
            }
            Command::RtcToken {
                channel_id,
                request_id,
            } => {
                let keys = self.protected_call_keys(&channel_id)?;
                let mut ticket: relay_protocol::RtcTicket = self.post(
                    "/api/rtc/token",
                    &VoiceJoin {
                        open_revision: self
                            .connection
                            .as_ref()
                            .and_then(|c| c.chat.open_revision()),
                        channel_id,
                        device_id: self
                            .connection
                            .as_ref()
                            .and_then(|c| c.crypto.as_ref())
                            .map(|crypto| crypto.device.identity().device_id.clone()),
                    },
                )?;
                if let Some(connection) = self.connection.as_ref() {
                    let local_iroh = crate::local_host::is_local_url(&connection.url)
                        && crate::local_host::embedded_address(&connection.server_id).as_deref()
                            == Some(
                                format!("http://{}:4580", crate::iroh_route::ONLY_ADDRESS).as_str(),
                            );
                    let private_bridge=ticket.media_bridge.is_some()&&crate::iroh_route::is_iroh(&ticket.url);
                    if crate::iroh_route::is_iroh(&connection.url) || local_iroh || private_bridge {
                        crate::embedded_network::prepare_iroh_rtc(
                            &mut ticket,
                            &connection.url,
                            &connection.server_id,
                            local_iroh || private_bridge&&crate::local_host::is_local_url(&connection.url),
                        )?;
                    } else {
                        let private_origin = if crate::embedded_network::routed(&connection.url) {
                            Some(connection.url.clone())
                        } else if crate::local_host::is_local_url(&connection.url) {
                            crate::local_host::embedded_address(&connection.server_id)
                        } else {
                            None
                        };
                        if let Some(origin) = private_origin {
                            crate::embedded_network::require_rtc_route(
                                &ticket.url,
                                &origin,
                                &connection.server_id,
                            )?;
                        }
                    }
                }
                self.emit(Update::RtcToken {
                    ticket,
                    request_id,
                    keys,
                });
            }
            _ => return Err("Unsupported network operation.".into()),
        }
        Ok(())
    }

    fn search_scan_page(
        &mut self,
        target: &Target,
        before: Option<u64>,
    ) -> Result<(HistoryPage, usize), String> {
        let mut request = self
            .request(reqwest::Method::GET, "/api/history")?
            .query(&[("limit", "100")]);
        request = match target {
            Target::Channel(id) => request.query(&[("channel_id", id)]),
            Target::Direct(id) => request.query(&[("peer_id", id)]),
        };
        if let Some(before) = before {
            request = request.query(&[("before", before)]);
        }
        // Search terms never enter a URL/body. Empty rooms must not create MLS groups.
        let mut page: HistoryPage = read_json(
            checked(request.send().map_err(network_error)?)?,
            8 * 1024 * 1024,
        )?;
        if page.messages.len() > 100 {
            return Err("The history page exceeds its limit.".into());
        }
        if !self
            .connection
            .as_ref()
            .is_some_and(|c| c.chat.plain_allowed())
            && page
                .messages
                .iter()
                .any(|m| m.content.starts_with(crate::e2ee::MESSAGE_PREFIX))
        {
            self.ensure_crypto_conversation(target)?;
        }
        let mut unavailable = 0;
        if let Some(connection) = self.connection.as_mut() {
            let origin = CommandOrigin::from(&*connection);
            let protections =
                super::encrypted::decrypt_in_connection(connection, target, &mut page.messages);
            unavailable = protections
                .iter()
                .filter(|(_, p)| matches!(p, crate::e2ee::MessageProtection::Unavailable(_)))
                .count();
            page.messages.retain(|m| {
                protections.iter().any(|(id, protection)| {
                    id == &m.id
                        && !matches!(protection, crate::e2ee::MessageProtection::Unavailable(_))
                })
            });
            self.emit(Update::MessageProtections {
                origin,
                protections,
            });
        }
        Ok((page, unavailable))
    }

    fn history(
        &mut self,
        target: &Target,
        before: Option<u64>,
        search: Option<&str>,
    ) -> Result<HistoryPage, String> {
        if self
            .connection
            .as_ref()
            .is_some_and(|c| c.chat.supports("e2ee_mls_v1") && !c.chat.plain_allowed())
        {
            let _ = self.ensure_crypto_conversation(target);
        }
        if self
            .connection
            .as_ref()
            .is_some_and(|c| c.protocol_version < API_PROTOCOL_VERSION)
        {
            if before.is_some() || search.is_some_and(|query| !query.is_empty()) {
                return Err("Update this server to use older history and full search.".into());
            }
            let request = self.request(reqwest::Method::GET, "/api/messages")?;
            let request = match target {
                Target::Channel(id) => request.query(&[("channel_id", id)]),
                Target::Direct(id) => request.query(&[("peer_id", id)]),
            };
            let mut messages: Vec<Message> = parse(request.send().map_err(network_error)?)?;
            self.decrypt_messages(target, &mut messages);
            return Ok(HistoryPage {
                messages,
                next_before: None,
                latest_sequence: 0,
            });
        }
        let mut cursor = before;
        let needle = search.map(str::to_lowercase);
        let mut result = HistoryPage::default();
        for _ in 0..20 {
            let mut request = self
                .request(reqwest::Method::GET, "/api/history")?
                .query(&[("limit", "100")]);
            request = match target {
                Target::Channel(id) => request.query(&[("channel_id", id)]),
                Target::Direct(id) => request.query(&[("peer_id", id)]),
            };
            if let Some(before) = cursor {
                request = request.query(&[("before", before)]);
            }
            // Queries stay on the client, including when reading legacy history.
            let mut page: HistoryPage = read_json(
                checked(request.send().map_err(network_error)?)?,
                8 * 1024 * 1024,
            )?;
            self.decrypt_messages(target, &mut page.messages);
            result.latest_sequence = result.latest_sequence.max(page.latest_sequence);
            result.next_before = page.next_before;
            result
                .messages
                .extend(page.messages.into_iter().filter(|message| {
                    needle.as_ref().is_none_or(|query| {
                        message.content.to_lowercase().contains(query)
                            || message
                                .attachments
                                .iter()
                                .any(|file| file.name.to_lowercase().contains(query))
                    })
                }));
            if needle.is_none() || result.messages.len() >= 100 || result.next_before.is_none() {
                break;
            }
            if cursor == result.next_before {
                return Err("The history cursor did not advance.".into());
            }
            cursor = result.next_before;
        }
        Ok(result)
    }

    pub(super) fn load_changes(&mut self) -> bool {
        let Some(c) = &self.connection else {
            return false;
        };
        if c.protocol_version < API_PROTOCOL_VERSION {
            return false;
        }
        let origin = CommandOrigin::from(c);
        let url = c.url.clone();
        let token = c.session.token.clone();
        let mut cursor = c.change_cursor;
        let mut caught_up = false;
        for _ in 0..5 {
            let result = self
                .http
                .get(crate::embedded_network::endpoint(&format!(
                    "{url}/api/changes"
                )))
                .bearer_auth(&token)
                .query(&[("after", cursor), ("limit", 200)])
                .send()
                .map_err(network_error)
                .and_then(parse::<ChangePage>);
            let Ok(mut page) = result else {
                break;
            };
            if page.next_cursor < cursor {
                break;
            }
            cursor = page.next_cursor;
            self.decrypt_changes(&mut page.changes);
            if !page.changes.is_empty() {
                self.emit(Update::Changes {
                    origin: origin.clone(),
                    changes: page.changes,
                });
            }
            if !page.has_more {
                caught_up = true;
                break;
            }
        }
        if let Some(c) = &mut self.connection {
            c.change_cursor = cursor;
        }
        caught_up
    }

    pub(super) fn refresh_background(&mut self) {
        let mut ready = Vec::new();
        while let Ok(result) = self.background_results.try_recv() {
            let saved = self.saved_connections.get(&result.url);
            let valid = saved.is_some_and(|saved| {
                saved.connection.session.token == result.token
                    && result.bootstrap.as_ref().is_ok_and(|bootstrap| {
                        validate_bootstrap_identity(
                            &saved.connection.server_id,
                            &saved.connection.session.user.id,
                            bootstrap,
                        )
                        .is_ok()
                    })
            });
            let has_work = result.keys.as_ref().is_ok_and(|keys| !keys.is_empty())
                || result
                    .changes
                    .as_ref()
                    .is_some_and(|page| page.as_ref().is_ok_and(|page| !page.changes.is_empty()));
            if valid && result.keys.is_ok() && has_work {
                self.background_pending
                    .push_back(super::background_crypto::Pending::new(
                        result,
                        &saved.unwrap().connection.session.user.id,
                    ));
            } else {
                ready.push(result);
            }
        }
        // Give one community one bounded step, then yield to the command loop.
        if let Some(mut pending) = self.background_pending.pop_front() {
            let valid = self
                .saved_connections
                .get(&pending.result.url)
                .is_some_and(|saved| saved.connection.session.token == pending.result.token);
            if valid {
                let saved = self.saved_connections.get_mut(&pending.result.url).unwrap();
                match pending.advance(&mut saved.connection) {
                    Ok(true) => ready.push(pending.result),
                    Ok(false) => self.background_pending.push_back(pending),
                    Err(error) => {
                        pending.result.keys = Err(error);
                        ready.push(pending.result);
                    }
                }
            } else if self.background_jobs.get(&pending.result.url) == Some(&pending.result.token) {
                self.background_jobs.remove(&pending.result.url);
            }
        }
        for result in ready {
            if self.background_jobs.get(&result.url) == Some(&result.token) {
                self.background_jobs.remove(&result.url);
            }
            let Some(saved) = self.saved_connections.get_mut(&result.url) else {
                continue;
            };
            if saved.connection.session.token != result.token {
                continue;
            }
            // An authenticated request may expire after bootstrap but before
            // changes/key retrieval. Stop only this verified saved token.
            if result
                .bootstrap
                .as_ref()
                .is_err_and(|error| error == "Sign in again")
                || result
                    .changes
                    .as_ref()
                    .is_some_and(|page| page.as_ref().is_err_and(|error| error == "Sign in again"))
                || result
                    .keys
                    .as_ref()
                    .is_err_and(|error| error == "Sign in again")
            {
                self.reject_saved_session(&result.url);
                continue;
            }
            if result
                .bootstrap
                .as_ref()
                .is_err_and(|error| error == IDENTITY_CHANGED || error == IDENTITY_UNCONFIRMED)
                || result
                    .keys
                    .as_ref()
                    .is_err_and(|error| error == IDENTITY_CHANGED || error == IDENTITY_UNCONFIRMED)
                || result.bootstrap.as_ref().is_ok_and(|bootstrap| {
                    validate_bootstrap_identity(
                        &saved.connection.server_id,
                        &saved.connection.session.user.id,
                        bootstrap,
                    )
                    .is_err()
                })
            {
                self.reject_saved_identity(&result.url);
                continue;
            }
            match result.bootstrap {
                Ok(mut bootstrap) => {
                    if bootstrap.host_paused {
                        saved.connection.chat.update_bootstrap(&bootstrap);
                        self.emit(Update::ConnectionState {
                            url: result.url.clone(),
                            status: ConnectionStatus::Reconnecting,
                        });
                        self.emit(Update::CommunityBootstrap {
                            url: result.url.clone(),
                            bootstrap,
                        });
                        continue;
                    }
                    if saved.connection.change_cursor > bootstrap.latest_change_sequence {
                        // A public reply may have been queued before this community
                        // was selected and advanced. Do not restore its older UI
                        // metadata or rewind an already accepted changes cursor.
                        saved.dirty.store(true, Ordering::Relaxed);
                        continue;
                    }
                    saved.connection.session.user = bootstrap.user.clone();
                    saved.connection.name = bootstrap.server_name.clone();
                    saved.connection.server_id = bootstrap.server_id.clone();
                    saved.connection.protocol_version = bootstrap.protocol_version;
                    saved.connection.chat.update_bootstrap(&bootstrap);
                    let encryption_failed = result.keys.is_err()
                        || saved
                            .connection
                            .crypto
                            .as_ref()
                            .is_some_and(|crypto| crypto.device.is_blocked());
                    let protections = result.protections;
                    let changes = match result.changes {
                        Some(Ok(mut page)) => {
                            if encryption_failed {
                                saved.dirty.store(true, Ordering::Relaxed);
                                None
                            } else {
                                // Active polling can accept an overlapping page while
                                // this background job is in flight. Only new sequences
                                // may reach the UI and its notification/sound gate.
                                page.changes.retain(|change| {
                                    change.sequence > saved.connection.change_cursor
                                });
                                if page.next_cursor >= saved.connection.change_cursor {
                                    saved.connection.change_cursor = page.next_cursor;
                                }
                                if page.has_more {
                                    saved.dirty.store(true, Ordering::Relaxed);
                                }
                                Some(page.changes)
                            }
                        }
                        Some(Err(_)) => {
                            saved.dirty.store(true, Ordering::Relaxed);
                            None
                        }
                        None => None,
                    };
                    super::encrypted::reconcile_read_states(
                        &saved.connection,
                        &mut bootstrap.read_states,
                    );
                    let origin = CommandOrigin::from(&saved.connection);
                    if self.background_retry.remove(&result.url).is_some() {
                        self.emit(Update::ConnectionState {
                            url: result.url.clone(),
                            status: ConnectionStatus::Connected,
                        });
                    }
                    if !encryption_failed && !protections.is_empty() {
                        self.emit(Update::MessageProtections {
                            origin: origin.clone(),
                            protections,
                        });
                    }
                    self.emit(Update::CommunityBootstrap {
                        url: result.url.clone(),
                        bootstrap,
                    });
                    if let Some(changes) = changes.filter(|changes| !changes.is_empty()) {
                        self.emit(Update::Changes { origin, changes });
                    }
                }
                Err(_) => {
                    // Authentication failures were handled above. Keep the
                    // same actor/cursor and bounded retry for a transport loss.
                    saved.dirty.store(true, Ordering::Relaxed);
                    let first_failure = !self.background_retry.contains_key(&result.url);
                    let failures = self
                        .background_retry
                        .get(&result.url)
                        .map_or(1, |(n, _)| n.saturating_add(1))
                        .min(5);
                    let delay = Duration::from_secs((1u64 << failures).min(30));
                    self.background_retry
                        .insert(result.url.clone(), (failures, Instant::now() + delay));
                    if first_failure {
                        self.emit(Update::ConnectionState {
                            url: result.url,
                            status: ConnectionStatus::Reconnecting,
                        });
                    }
                }
            }
        }
        if self.background_at.elapsed() < Duration::from_millis(750) {
            return;
        }
        self.background_at = Instant::now();
        let urls: Vec<_> = self
            .saved_connections
            .iter()
            .filter(|(url, s)| {
                s.dirty.load(Ordering::Relaxed)
                    && !self.background_jobs.contains_key(*url)
                    && self
                        .background_retry
                        .get(*url)
                        .is_none_or(|(_, next)| Instant::now() >= *next)
            })
            .map(|(url, _)| url.clone())
            .collect();
        for url in urls {
            // Bound completed/pending payloads as well as active HTTP threads.
            if self.background_jobs.len() >= 4 {
                break;
            }
            let Some(saved) = self.saved_connections.get(&url) else {
                continue;
            };
            let Ok(permit) = TransferPermit::acquire(self.background_active.clone()) else {
                break;
            };
            let token = saved.connection.session.token.clone();
            let server_id = saved.connection.server_id.clone();
            let user_id = saved.connection.session.user.id.clone();
            let cursor = saved.connection.change_cursor;
            let key_plans = super::background_crypto::plans(&saved.connection);
            let http = self.http.clone();
            let completed = self.background_completed.clone();
            saved.dirty.store(false, Ordering::Relaxed);
            self.background_jobs.insert(url.clone(), token.clone());
            let job_url = url.clone();
            let spawn = std::thread::Builder::new()
                .name("relay-community-refresh".into())
                .spawn(move || {
                    let _permit = permit;
                    let bootstrap = http
                        .get(crate::embedded_network::endpoint(&format!(
                            "{job_url}/api/bootstrap"
                        )))
                        .bearer_auth(&token)
                        .timeout(Duration::from_secs(2))
                        .send()
                        .map_err(network_error)
                        .and_then(parse::<Bootstrap>);
                    let mut bootstrap = bootstrap.and_then(|bootstrap| {
                        validate_bootstrap_identity(&server_id, &user_id, &bootstrap)?;
                        Ok(bootstrap)
                    });
                    let mut changes = bootstrap
                        .as_ref()
                        .ok()
                        .filter(|b| !b.host_paused && b.protocol_version >= API_PROTOCOL_VERSION)
                        .map(|b| {
                            http.get(crate::embedded_network::endpoint(&format!(
                                "{job_url}/api/changes"
                            )))
                            .bearer_auth(&token)
                            .query(&[
                                ("after", cursor.min(b.latest_change_sequence)),
                                ("limit", 32),
                            ])
                            .timeout(Duration::from_secs(2))
                            .send()
                            .map_err(network_error)
                            .and_then(|response| read_json(checked(response)?, 8 * 1024 * 1024))
                            .and_then(|page: ChangePage| {
                                let after = cursor.min(b.latest_change_sequence);
                                let mut sequence = after;
                                if page.changes.len() > 32 || page.next_cursor < after {
                                    return Err("Invalid background change page.".into());
                                }
                                for change in &page.changes {
                                    if change.sequence <= sequence
                                        || change.sequence > page.next_cursor
                                    {
                                        return Err(
                                            "Background changes arrived out of order.".into()
                                        );
                                    }
                                    sequence = change.sequence;
                                }
                                if page.has_more && page.next_cursor == after {
                                    return Err("Background change page made no progress.".into());
                                }
                                Ok(page)
                            })
                        });
                    let keys = match (bootstrap.as_mut(), changes.as_mut()) {
                        (Ok(bootstrap), Some(Ok(page))) => super::background_crypto::fetch(
                            &http,
                            &job_url,
                            &token,
                            key_plans,
                            bootstrap,
                            &mut page.changes,
                        ),
                        _ => Ok(Vec::new()),
                    };
                    let _ = completed.send(BackgroundResult {
                        url: job_url,
                        token,
                        bootstrap,
                        changes,
                        keys,
                        protections: Vec::new(),
                    });
                });
            if spawn.is_err() {
                self.background_jobs.remove(&url);
                if let Some(saved) = self.saved_connections.get(&url) {
                    saved.dirty.store(true, Ordering::Relaxed);
                }
            }
        }
    }

    fn reject_saved_identity(&mut self, url: &str) {
        self.reject_saved_session(url);
        self.emit(Update::Notice(
            "A saved community changed identity. Confirm its address with the host before signing in again.".into(),
        ));
    }

    fn reject_saved_session(&mut self, url: &str) {
        if let Some(mut saved) = self.saved_connections.remove(url) {
            saved.connection.chat.clear_ephemeral();
            saved.stop.store(true, Ordering::SeqCst);
            saved.dirty.store(false, Ordering::SeqCst);
            if saved.connection.remembered {
                let _ = crate::credential_vault::forget(url);
            }
        }
        self.background_retry.remove(url);
        self.background_jobs.remove(url);
        self.background_pending
            .retain(|pending| pending.result.url != url);
        self.emit(Update::ConnectionState {
            url: url.into(),
            status: ConnectionStatus::SignInRequired,
        });
        self.publish_communities();
    }

    fn transfer_client(&mut self) -> Result<Client, String> {
        if self.transfer_http.is_none() {
            self.transfer_http = Some(
                Client::builder()
                    .no_proxy()
                    .connect_timeout(Duration::from_secs(4))
                    .timeout(Duration::from_secs(120))
                    .redirect(reqwest::redirect::Policy::none())
                    .build()
                    .map_err(network_error)?,
            );
        }
        Ok(self
            .transfer_http
            .as_ref()
            .expect("transfer client initialized")
            .clone())
    }

    fn start_upload(
        &mut self,
        origin: &CommandOrigin,
        target: Target,
        path: PathBuf,
        operation_id: String,
    ) -> Result<(), String> {
        self.command_connection(origin)?;
        if self.transfers.len() >= 2 {
            return Err("Two transfers are already running. Wait or cancel one.".into());
        }
        if operation_id.is_empty()
            || operation_id.len() > 100
            || self.transfers.contains_key(&operation_id)
        {
            return Err("This upload identifier is invalid or already running.".into());
        }
        // Only a person who accepted the open mode uploads files as they are.
        let plain_upload = self
            .connection
            .as_ref()
            .is_some_and(|c| c.chat.plain_allowed());
        let size = std::fs::metadata(&path)
            .map_err(|_| "The selected file cannot be read.")?
            .len();
        if plain_upload {
            if size == 0 || size > MAX_UPLOAD_BYTES as u64 {
                return Err("Files must be nonempty and fit within 10 MB.".into());
            }
        } else if size == 0 || size > (MAX_UPLOAD_BYTES - relay_crypto::files::FILE_OVERHEAD) as u64
        {
            return Err("Encrypted files must be nonempty and fit within 10 MB including encryption overhead.".into());
        }
        let file = std::fs::File::open(&path).map_err(|_| "The selected file cannot be read.")?;
        let binding = if plain_upload {
            None
        } else {
            let context = self.ensure_crypto_conversation(&target)?;
            let scope = match &target {
                Target::Channel(id) => format!("channel:{id}"),
                Target::Direct(peer) => {
                    let own = self
                        .connection
                        .as_ref()
                        .ok_or("Connect to a community first.")?
                        .session
                        .user
                        .id
                        .as_str();
                    let mut pair = [own, peer.as_str()];
                    pair.sort();
                    format!("direct:{}:{}", pair[0], pair[1])
                }
            };
            Some(format!(
                "Relay/file/v1/{}/{}/{}/{}",
                context.server_id,
                scope,
                context
                    .group_id
                    .as_deref()
                    .ok_or("The conversation encryption group is not ready.")?,
                uuid::Uuid::new_v4()
            ))
        };
        let c = self
            .connection
            .as_ref()
            .ok_or("Connect to a community first.")?;
        let url = c.url.clone();
        let token = c.session.token.clone();
        let http = self.transfer_client()?;
        let epoch = self.transfer_epoch.load(Ordering::SeqCst);
        let current_epoch = self.transfer_epoch.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let completed = self.transfer_completed.clone();
        let template = TransferMeta {
            epoch,
            operation_id: operation_id.clone(),
            target: Some(target),
            path: path.clone(),
        };
        let permit = TransferPermit::acquire(self.transfer_active.clone())?;
        self.transfers.insert(operation_id.clone(), cancel);
        let result = std::thread::Builder::new()
            .name("relay-upload".into())
            .spawn(move || {
                let _permit = permit;
                let mut reader = ProgressReader {
                    reader: Box::new(file),
                    size,
                    read: 0,
                    last: Instant::now() - Duration::from_secs(1),
                    cancel: worker_cancel.clone(),
                    current_epoch: current_epoch.clone(),
                    completed: completed.clone(),
                    template: template.clone(),
                };
                let name = path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("attachment")
                    .to_owned();
                let result =
                    (|| -> Result<(Attachment, Option<crate::e2ee::EncryptedAttachment>), String> {
                        let mut plain = zeroize::Zeroizing::new(Vec::with_capacity(size as usize));
                        reader
                            .by_ref()
                            .take(size + 1)
                            .read_to_end(&mut plain)
                            .map_err(|_| {
                                "The selected file changed or upload was cancelled.".to_string()
                            })?;
                        if plain.len() as u64 != size
                            || worker_cancel.load(Ordering::SeqCst)
                            || current_epoch.load(Ordering::SeqCst) != epoch
                        {
                            return Err("The selected file changed or upload was cancelled.".into());
                        }
                        let mime = match image::guess_format(&plain) {
                            Ok(image::ImageFormat::Png) => "image/png",
                            Ok(image::ImageFormat::Jpeg) => "image/jpeg",
                            Ok(image::ImageFormat::Gif) => "image/gif",
                            Ok(image::ImageFormat::WebP) => "image/webp",
                            _ => "application/octet-stream",
                        }
                        .to_owned();
                        let (wire, key, upload_name, upload_mime) = match &binding {
                            Some(binding) => {
                                let (wire, key) = relay_crypto::files::encrypt(&plain, binding)?;
                                (
                                    wire,
                                    Some(key),
                                    format!("{}.bin", uuid::Uuid::new_v4()),
                                    "application/octet-stream".to_owned(),
                                )
                            }
                            None => (plain.to_vec(), None, name.clone(), mime.clone()),
                        };
                        drop(plain);
                        let wire_size = wire.len() as u64;
                        if wire_size > MAX_UPLOAD_BYTES as u64 {
                            return Err("The encrypted file exceeds the upload limit.".into());
                        }
                        let reader = ProgressReader {
                            reader: Box::new(std::io::Cursor::new(wire)),
                            size: wire_size,
                            read: 0,
                            last: Instant::now() - Duration::from_secs(1),
                            cancel: worker_cancel.clone(),
                            current_epoch: current_epoch.clone(),
                            completed: completed.clone(),
                            template: template.clone(),
                        };
                        let mut attachment = http
                            .post(crate::embedded_network::endpoint(&format!(
                                "{url}/api/uploads"
                            )))
                            .bearer_auth(token)
                            .query(&[
                                ("name", upload_name.as_str()),
                                ("mime", upload_mime.as_str()),
                            ])
                            .header("Content-Type", "application/octet-stream")
                            .body(reqwest::blocking::Body::sized(reader, wire_size))
                            .send()
                            .map_err(network_error)
                            .and_then(parse::<Attachment>)?;
                        super::validate_chat_attachment(&attachment)?;
                        if attachment.size != wire_size {
                            return Err(
                                "The server returned mismatched encrypted attachment metadata."
                                    .into(),
                            );
                        }
                        let metadata = key.map(|key| crate::e2ee::EncryptedAttachment {
                            id: attachment.id.clone(),
                            name: name.clone(),
                            mime: mime.clone(),
                            file: key,
                        });
                        attachment.name = name;
                        attachment.mime = mime;
                        attachment.size = size;
                        Ok((attachment, metadata))
                    })();
                let result = if worker_cancel.load(Ordering::SeqCst)
                    || current_epoch.load(Ordering::SeqCst) != epoch
                {
                    Err("Upload cancelled.".into())
                } else {
                    result
                };
                let _ = completed.send(template.result(TransferOutcome::Uploaded(result)));
            });
        if let Err(error) = result {
            self.transfers.remove(&operation_id);
            return Err(format!("Cannot start the upload: {error}"));
        }
        Ok(())
    }

    pub(super) fn mark_read(
        &mut self,
        origin: &CommandOrigin,
        target: &Target,
        last_read_sequence: u64,
    ) -> Result<ReadState, String> {
        self.command_connection(origin)?;
        let (channel_id, peer_id) = target_ids(target);
        let mut state: ReadState = self.patch(
            "/api/read-state",
            &MarkRead {
                channel_id: channel_id.clone(),
                peer_id: peer_id.clone(),
                last_read_sequence,
            },
        )?;
        if state.channel_id != channel_id || state.peer_id != peer_id {
            return Err("The server returned a read marker for another conversation.".into());
        }
        if let Some(connection) = &self.connection {
            encrypted::reconcile_read_states(connection, std::slice::from_mut(&mut state));
        }
        Ok(state)
    }

    fn mark_community_read(&mut self, origin: &CommandOrigin) -> Result<(), String> {
        // A background server's read markers must not switch the active server,
        // leave voice, or borrow the active account's session token.
        let connection = self
            .connection
            .as_ref()
            .filter(|c| CommandOrigin::from(*c) == *origin)
            .or_else(|| {
                self.saved_connections
                    .get(&origin.url)
                    .map(|saved| &saved.connection)
                    .filter(|c| CommandOrigin::from(*c) == *origin)
            })
            .ok_or("This request belongs to another account or community.")?;
        let token = zeroize::Zeroizing::new(connection.session.token.clone());
        let endpoint =
            |path: &str| crate::embedded_network::endpoint(&format!("{}{path}", origin.url));
        let bootstrap: Bootstrap = parse(
            self.http
                .get(endpoint("/api/bootstrap"))
                .bearer_auth(token.as_str())
                .send()
                .map_err(network_error)?,
        )?;
        validate_bootstrap_identity(&origin.server_id, &origin.user_id, &bootstrap)?;
        let mut channels: Vec<_> = bootstrap
            .read_states
            .iter()
            .filter(|read| read.unread_count > 0)
            .filter_map(|read| read.channel_id.as_ref())
            .filter(|id| bootstrap.channels.iter().any(|channel| &channel.id == *id))
            .cloned()
            .collect();
        channels.sort();
        channels.dedup();
        for channel_id in channels {
            // The server bounds this explicit mark-all request to the channel's
            // current latest sequence. Direct messages are deliberately excluded.
            let _: ReadState = parse(
                self.http
                    .patch(endpoint("/api/read-state"))
                    .bearer_auth(token.as_str())
                    .json(&MarkRead {
                        channel_id: Some(channel_id),
                        peer_id: None,
                        last_read_sequence: u64::MAX,
                    })
                    .send()
                    .map_err(network_error)?,
            )?;
        }
        let refreshed: Bootstrap = parse(
            self.http
                .get(endpoint("/api/bootstrap"))
                .bearer_auth(token.as_str())
                .send()
                .map_err(network_error)?,
        )?;
        validate_bootstrap_identity(&origin.server_id, &origin.user_id, &refreshed)?;
        if self
            .connection
            .as_ref()
            .is_some_and(|c| CommandOrigin::from(c) == *origin)
        {
            self.emit(Update::Bootstrap(refreshed));
        } else {
            self.emit(Update::CommunityBootstrap {
                url: origin.url.clone(),
                bootstrap: refreshed,
            });
        }
        Ok(())
    }

    fn start_profile_save(
        &mut self,
        origin: &CommandOrigin,
        display_name: String,
        bio: String,
        appearance: Option<ProfileAppearance>,
        avatar: ProfileAvatarChange,
        request_id: u64,
    ) -> Result<(), String> {
        // Bind queued text and local avatar paths before validating content or
        // spawning a transfer. A connection switch must never redirect a save.
        let c = self.command_connection(origin)?;
        if display_name.trim().chars().count() > 32 || bio.chars().count() > 190 {
            return Err(
                "Use up to 32 characters for your display name and 190 for About Me.".into(),
            );
        }
        if !c.chat.supports("profiles-v1") {
            return Err("Update this server to edit profiles.".into());
        }
        let url = c.url.clone();
        let token = c.session.token.clone();
        let expected_user_id = c.session.user.id.clone();
        let operation_id = format!("profile-{request_id}");
        if self.transfers.contains_key(&operation_id) {
            return Err("This profile save is already running.".into());
        }
        let http = self.transfer_client()?;
        let epoch = self.transfer_epoch.load(Ordering::SeqCst);
        let current_epoch = self.transfer_epoch.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let completed = self.transfer_completed.clone();
        let template = TransferMeta {
            epoch,
            operation_id: operation_id.clone(),
            target: None,
            path: PathBuf::new(),
        };
        let permit = TransferPermit::acquire(self.transfer_active.clone())?;
        self.transfers.insert(operation_id.clone(), cancel);
        let spawned = std::thread::Builder::new()
            .name("relay-profile-save".into())
            .spawn(move || {
                let _permit = permit;
                let mut latest = None;
                let result = (|| -> Result<(), String> {
                    if worker_cancel.load(Ordering::SeqCst)
                        || current_epoch.load(Ordering::SeqCst) != epoch
                    {
                        return Err("Profile save cancelled.".into());
                    }
                    let body = UpdateProfile {
                        display_name,
                        bio,
                        appearance,
                    };
                    let user: User = parse(
                        http.put(crate::embedded_network::endpoint(&format!(
                            "{url}/api/profile"
                        )))
                        .bearer_auth(&token)
                        .json(&body)
                        .send()
                        .map_err(network_error)?,
                    )?;
                    if user.id != expected_user_id {
                        return Err("The profile response did not match your account.".into());
                    }
                    latest = Some(user);
                    if worker_cancel.load(Ordering::SeqCst)
                        || current_epoch.load(Ordering::SeqCst) != epoch
                    {
                        return Err("Profile save cancelled.".into());
                    }
                    let response = match avatar {
                        ProfileAvatarChange::Prepared(bytes) => {
                            if bytes.is_empty()
                                || bytes.len() > 2 * 1024 * 1024
                                || !matches!(
                                    image::guess_format(&bytes),
                                    Ok(image::ImageFormat::Png)
                                )
                            {
                                return Err("The cropped avatar is invalid.".into());
                            }
                            Some(
                                http.put(crate::embedded_network::endpoint(&format!(
                                    "{url}/api/profile/avatar"
                                )))
                                .bearer_auth(&token)
                                .header("Content-Type", "application/octet-stream")
                                .body(bytes)
                                .send()
                                .map_err(network_error)?,
                            )
                        }
                        ProfileAvatarChange::Keep => None,
                        ProfileAvatarChange::Remove => Some(
                            http.delete(crate::embedded_network::endpoint(&format!(
                                "{url}/api/profile/avatar"
                            )))
                            .bearer_auth(&token)
                            .send()
                            .map_err(network_error)?,
                        ),
                        ProfileAvatarChange::Upload(path) => {
                            let file = std::fs::File::open(path)
                                .map_err(|_| "The selected avatar cannot be read.")?;
                            let mut bytes = Vec::new();
                            file.take(2 * 1024 * 1024 + 1)
                                .read_to_end(&mut bytes)
                                .map_err(|_| "The selected avatar cannot be read.")?;
                            if bytes.len() > 2 * 1024 * 1024 {
                                return Err("Avatars can be up to 2 MiB.".into());
                            }
                            if !matches!(
                                image::guess_format(&bytes),
                                Ok(image::ImageFormat::Png
                                    | image::ImageFormat::Jpeg
                                    | image::ImageFormat::WebP)
                            ) {
                                return Err("Choose a PNG, JPEG or WebP avatar.".into());
                            }
                            if worker_cancel.load(Ordering::SeqCst)
                                || current_epoch.load(Ordering::SeqCst) != epoch
                            {
                                return Err("Profile save cancelled.".into());
                            }
                            Some(
                                http.put(crate::embedded_network::endpoint(&format!(
                                    "{url}/api/profile/avatar"
                                )))
                                .bearer_auth(&token)
                                .header("Content-Type", "application/octet-stream")
                                .body(bytes)
                                .send()
                                .map_err(network_error)?,
                            )
                        }
                    };
                    if let Some(response) = response {
                        let user: User = parse(response)?;
                        if user.id != expected_user_id {
                            return Err("The avatar response did not match your account.".into());
                        }
                        latest = Some(user);
                    }
                    Ok(())
                })();
                let _ = completed.send(template.result(TransferOutcome::Profile {
                    url,
                    request_id,
                    user: latest,
                    error: result.err(),
                }));
            });
        if let Err(error) = spawned {
            self.transfers.remove(&operation_id);
            return Err(format!("Cannot start profile save: {error}"));
        }
        Ok(())
    }

    fn start_download(
        &mut self,
        origin: &CommandOrigin,
        attachment: Attachment,
        path: PathBuf,
        operation_id: String,
    ) -> Result<(), String> {
        if operation_id.is_empty()
            || operation_id.len() > 100
            || self.transfers.contains_key(&operation_id)
        {
            return Err("This download identifier is invalid or already running.".into());
        }
        if attachment.size > MAX_UPLOAD_BYTES as u64
            || attachment.id.is_empty()
            || attachment.id.len() > 128
            || !attachment
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            || attachment.url != format!("/api/uploads/{}", attachment.id)
        {
            return Err("The attachment address or size is invalid.".into());
        }
        let c = self.command_connection(origin)?;
        let key = match c.crypto.as_ref() {
            Some(crypto) => crypto.file(&attachment.id)?,
            None => None,
        };
        let (encrypted, wire_size) = match key {
            Some(encrypted) => {
                if encrypted.id != attachment.id || encrypted.file.size != attachment.size {
                    return Err(
                        "The attachment metadata does not match its verified envelope.".into(),
                    );
                }
                let wire_size = encrypted
                    .file
                    .size
                    .checked_add(relay_crypto::files::FILE_OVERHEAD as u64)
                    .filter(|size| *size <= MAX_UPLOAD_BYTES as u64)
                    .ok_or("The encrypted attachment exceeds its limit.")?;
                (Some(encrypted), wire_size)
            }
            // Only a community that runs without the extra layer serves files as they are.
            None if c.chat.plain_files_readable() && attachment.size > 0 => (None, attachment.size),
            None if c.crypto.is_none() => return Err("Encryption keys are not ready.".into()),
            None => return Err(
                "This attachment has no verified encryption key. Plaintext fallback is disabled."
                    .into(),
            ),
        };
        let url = c.url.clone();
        let token = c.session.token.clone();
        let http = self.transfer_client()?;
        let epoch = self.transfer_epoch.load(Ordering::SeqCst);
        let current_epoch = self.transfer_epoch.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let completed = self.transfer_completed.clone();
        let template = TransferMeta {
            epoch,
            operation_id: operation_id.clone(),
            target: None,
            path: path.clone(),
        };
        let commit = self.transfer_commit.clone();
        let permit = TransferPermit::acquire(self.transfer_active.clone())?;
        self.transfers.insert(operation_id.clone(), cancel);
        let spawned = std::thread::Builder::new()
            .name("relay-download".into())
            .spawn(move || {
                let _permit = permit;
                let result = (|| -> Result<(), String> {
                    if worker_cancel.load(Ordering::SeqCst)
                        || current_epoch.load(Ordering::SeqCst) != epoch
                    {
                        return Err("Download cancelled.".into());
                    }
                    let mut response = checked(
                        http.get(crate::embedded_network::endpoint(&format!(
                            "{url}{}",
                            attachment.url
                        )))
                        .bearer_auth(token)
                        .send()
                        .map_err(network_error)?,
                    )?;
                    if response
                        .content_length()
                        .is_some_and(|size| size != wire_size)
                    {
                        return Err("The attachment size changed. Refresh and try again.".into());
                    }
                    let wire = (|| -> Result<Vec<u8>, String> {
                        let mut wire = Vec::with_capacity(wire_size as usize);
                        let mut buffer = [0u8; 64 * 1024];
                        let mut transferred = 0;
                        let mut progress = Instant::now() - Duration::from_secs(1);
                        loop {
                            if worker_cancel.load(Ordering::SeqCst)
                                || current_epoch.load(Ordering::SeqCst) != epoch
                            {
                                return Err("Download cancelled.".into());
                            }
                            let count = response
                                .read(&mut buffer)
                                .map_err(|_| "The download failed.")?;
                            if count == 0 {
                                break;
                            }
                            transferred += count as u64;
                            if transferred > wire_size || transferred > MAX_UPLOAD_BYTES as u64 {
                                return Err(
                                    "The download exceeds the attachment size limit.".into()
                                );
                            }
                            wire.extend_from_slice(&buffer[..count]);
                            if progress.elapsed() >= Duration::from_millis(100)
                                || transferred == wire_size
                            {
                                let _ = completed.try_send(template.result(
                                    TransferOutcome::DownloadProgress {
                                        transferred,
                                        total: wire_size,
                                    },
                                ));
                                progress = Instant::now();
                            }
                        }
                        if transferred != wire_size {
                            return Err("The server returned an incomplete attachment.".into());
                        }
                        if worker_cancel.load(Ordering::SeqCst)
                            || current_epoch.load(Ordering::SeqCst) != epoch
                        {
                            return Err("Download cancelled.".into());
                        }
                        Ok(wire)
                    })()?;
                    // No plaintext temporary file is created until the entire
                    // ciphertext, binding and AEAD tag have authenticated.
                    let decrypted;
                    let plain: &[u8] = match &encrypted {
                        Some(encrypted) => {
                            decrypted = relay_crypto::files::decrypt(
                                &wire,
                                &encrypted.file,
                                &encrypted.file.binding,
                            )?;
                            &decrypted[..]
                        }
                        None => {
                            if relay_crypto::files::looks_encrypted(&wire) {
                                return Err(
                                    "This file is encrypted, but its key is not on this device."
                                        .into(),
                                );
                            }
                            &wire[..]
                        }
                    };
                    if worker_cancel.load(Ordering::SeqCst)
                        || current_epoch.load(Ordering::SeqCst) != epoch
                    {
                        return Err("Download cancelled.".into());
                    }
                    let temporary =
                        path.with_extension(format!("{}.partial", uuid::Uuid::new_v4()));
                    let mut file = std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&temporary)
                        .map_err(|_| "Cannot create the download file.")?;
                    let copy = (|| -> Result<(), String> {
                        for chunk in plain.chunks(64 * 1024) {
                            if worker_cancel.load(Ordering::SeqCst)
                                || current_epoch.load(Ordering::SeqCst) != epoch
                            {
                                return Err("Download cancelled.".into());
                            }
                            std::io::Write::write_all(&mut file, chunk)
                                .map_err(|_| "Cannot write the download.")?;
                        }
                        file.sync_all()
                            .map_err(|_| "Cannot finish the download.".to_string())?;
                        Ok(())
                    })();
                    drop(file);
                    if let Err(error) = copy {
                        let _ = std::fs::remove_file(&temporary);
                        return Err(error);
                    }
                    let _commit = commit.lock().unwrap_or_else(|e| e.into_inner());
                    if worker_cancel.load(Ordering::SeqCst)
                        || current_epoch.load(Ordering::SeqCst) != epoch
                    {
                        let _ = std::fs::remove_file(&temporary);
                        return Err("Download cancelled.".into());
                    }
                    if std::fs::rename(&temporary, &path).is_err() {
                        let _ = std::fs::remove_file(&temporary);
                        return Err("Cannot put the download in the chosen location.".into());
                    }
                    Ok(())
                })();
                let _ = completed.send(template.result(TransferOutcome::Downloaded(result)));
            });
        if let Err(error) = spawned {
            self.transfers.remove(&operation_id);
            return Err(format!("Cannot start download: {error}"));
        }
        Ok(())
    }

    fn start_backup(&mut self, path: PathBuf, operation_id: String) -> Result<(), String> {
        if operation_id.is_empty()
            || operation_id.len() > 100
            || self.transfers.contains_key(&operation_id)
        {
            return Err("This backup identifier is invalid or already running.".into());
        }
        if self.transfers.len() >= 2 {
            return Err("Wait for a transfer to finish before saving a backup.".into());
        }
        let c = self
            .connection
            .as_ref()
            .ok_or("Connect to a community first.")?;
        let url = c.url.clone();
        let token = c.session.token.clone();
        let http = self.transfer_client()?;
        let epoch = self.transfer_epoch.load(Ordering::SeqCst);
        let current_epoch = self.transfer_epoch.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let completed = self.transfer_completed.clone();
        let template = TransferMeta {
            epoch,
            operation_id: operation_id.clone(),
            target: None,
            path: path.clone(),
        };
        let commit = self.transfer_commit.clone();
        let permit = TransferPermit::acquire(self.transfer_active.clone())?;
        self.transfers.insert(operation_id.clone(), cancel);
        let result = std::thread::Builder::new()
            .name("relay-backup".into())
            .spawn(move || {
                let _permit = permit;
                let result = (|| -> Result<(), String> {
                    let mut response = checked(
                        http.get(crate::embedded_network::endpoint(&format!(
                            "{url}/api/admin/backup"
                        )))
                        .bearer_auth(token)
                        .send()
                        .map_err(network_error)?,
                    )?;
                    let total = response.content_length().unwrap_or(0);
                    // Save into a unique side file. Cancellation never replaces an existing backup.
                    let temporary =
                        path.with_extension(format!("{}.partial", uuid::Uuid::new_v4()));
                    let mut file = std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&temporary)
                        .map_err(|_| "Cannot create the backup file.")?;
                    let copy = (|| {
                        let mut header = [0u8; 16];
                        response
                            .read_exact(&mut header)
                            .map_err(|_| "The server returned an incomplete backup.".to_string())?;
                        if &header != b"SQLite format 3\0" {
                            return Err("The server did not return a SQLite backup.".to_string());
                        }
                        std::io::Write::write_all(&mut file, &header)
                            .map_err(|_| "Cannot write the backup.")?;
                        let mut buffer = [0u8; 64 * 1024];
                        let mut size = 16u64;
                        let mut progress = Instant::now() - Duration::from_secs(1);
                        loop {
                            if worker_cancel.load(Ordering::SeqCst)
                                || current_epoch.load(Ordering::SeqCst) != epoch
                            {
                                return Err("Backup cancelled.".to_string());
                            }
                            let n = response
                                .read(&mut buffer)
                                .map_err(|_| "The backup download failed.")?;
                            if n == 0 {
                                break;
                            }
                            size += n as u64;
                            if size > 4 * 1024 * 1024 * 1024 {
                                return Err("The backup exceeds the 4 GiB download limit.".into());
                            }
                            std::io::Write::write_all(&mut file, &buffer[..n])
                                .map_err(|_| "Cannot write the backup.")?;
                            if progress.elapsed() >= Duration::from_millis(100)
                                || (total > 0 && size == total)
                            {
                                let _ = completed.try_send(template.result(
                                    TransferOutcome::DownloadProgress {
                                        transferred: size,
                                        total,
                                    },
                                ));
                                progress = Instant::now();
                            }
                        }
                        file.sync_all().map_err(|_| "Cannot finish the backup.")?;
                        if worker_cancel.load(Ordering::SeqCst)
                            || current_epoch.load(Ordering::SeqCst) != epoch
                        {
                            return Err("Backup cancelled.".to_string());
                        }
                        Ok(())
                    })();
                    drop(file);
                    if let Err(error) = copy {
                        let _ = std::fs::remove_file(&temporary);
                        return Err(error);
                    }
                    let _commit = commit.lock().unwrap_or_else(|e| e.into_inner());
                    if worker_cancel.load(Ordering::SeqCst)
                        || current_epoch.load(Ordering::SeqCst) != epoch
                    {
                        let _ = std::fs::remove_file(&temporary);
                        return Err("Backup cancelled.".into());
                    }
                    if std::fs::rename(&temporary, &path).is_err() {
                        let _ = std::fs::remove_file(&temporary);
                        return Err("Cannot put the backup in the chosen location.".into());
                    }
                    Ok(())
                })();
                let _ = completed.send(template.result(TransferOutcome::Backup(result)));
            });
        if let Err(error) = result {
            self.transfers.remove(&operation_id);
            return Err(format!("Cannot start backup download: {error}"));
        }
        Ok(())
    }

    pub(super) fn poll_transfers(&mut self) {
        while let Ok(result) = self.transfer_results.try_recv() {
            if result.epoch != self.transfer_epoch.load(Ordering::SeqCst) {
                continue;
            }
            let cancelled = self
                .transfers
                .get(&result.operation_id)
                .is_some_and(|c| c.load(Ordering::SeqCst));
            match result.result {
                TransferOutcome::Profile {
                    url,
                    request_id,
                    user,
                    error,
                } => {
                    self.transfers.remove(&result.operation_id);
                    if let Some(user) = &user {
                        if let Some(c) = &mut self.connection {
                            if c.url == url && c.session.user.id == user.id {
                                c.session.user = user.clone();
                            }
                        }
                        self.refresh_pending.store(true, Ordering::Relaxed);
                    }
                    if let Some(error) = error {
                        self.emit(Update::ProfileSaveFailed {
                            url,
                            user,
                            request_id,
                            error,
                        });
                    } else if let Some(user) = user {
                        self.emit(Update::ProfileSaved {
                            url,
                            user,
                            request_id,
                        });
                    }
                }
                TransferOutcome::Progress { transferred, total } => {
                    if let Some(target) = result.target {
                        self.emit(Update::UploadProgress {
                            target,
                            operation_id: result.operation_id,
                            path: result.path,
                            transferred,
                            total,
                        });
                    }
                }
                TransferOutcome::DownloadProgress { transferred, total } => {
                    self.emit(Update::DownloadProgress {
                        operation_id: result.operation_id,
                        path: result.path,
                        transferred,
                        total,
                    })
                }
                TransferOutcome::Downloaded(download) => {
                    self.transfers.remove(&result.operation_id);
                    if cancelled {
                        self.emit(Update::DownloadFailed {
                            operation_id: result.operation_id,
                            error: "Download cancelled.".into(),
                            cancelled: true,
                        });
                    } else {
                        self.emit(match download {
                            Ok(()) => Update::DownloadFinished {
                                operation_id: result.operation_id,
                                path: result.path,
                            },
                            Err(error) => Update::DownloadFailed {
                                operation_id: result.operation_id,
                                error,
                                cancelled: false,
                            },
                        });
                    }
                }
                TransferOutcome::Uploaded(upload) => {
                    self.transfers.remove(&result.operation_id);
                    if let Some(target) = result.target {
                        if cancelled {
                            self.emit(Update::UploadFailed {
                                target,
                                operation_id: result.operation_id,
                                error: "Upload cancelled.".into(),
                                cancelled: true,
                            });
                        } else {
                            let update = match upload {
                                Ok((attachment, metadata)) => {
                                    let persisted = match metadata {
                                        None => Ok(()),
                                        Some(metadata) => self.connection.as_mut().and_then(|c|c.crypto.as_mut())
                                            .ok_or_else(||"The encryption session ended before attachment keys were saved.".to_string())
                                            .and_then(|crypto|crypto.add_file(metadata)),
                                    };
                                    match persisted {
                                        Ok(()) => Update::UploadFinished {
                                            target,
                                            operation_id: result.operation_id,
                                            attachment,
                                        },
                                        Err(error) => Update::UploadFailed {
                                            target,
                                            operation_id: result.operation_id,
                                            error,
                                            cancelled: false,
                                        },
                                    }
                                }
                                Err(error) => Update::UploadFailed {
                                    target,
                                    operation_id: result.operation_id,
                                    error,
                                    cancelled: false,
                                },
                            };
                            self.emit(update);
                        }
                    }
                }
                TransferOutcome::Backup(backup) => {
                    self.transfers.remove(&result.operation_id);
                    if cancelled {
                        self.emit(Update::BackupFailed {
                            operation_id: result.operation_id,
                            error: "Backup cancelled.".into(),
                            cancelled: true,
                        });
                    } else {
                        match backup {
                            Ok(()) => self.emit(Update::BackupSaved {
                                operation_id: result.operation_id,
                                path: result.path,
                            }),
                            Err(error) => self.emit(Update::BackupFailed {
                                operation_id: result.operation_id,
                                error,
                                cancelled: false,
                            }),
                        }
                    }
                }
            }
        }
    }
}

fn target_ids(target: &Target) -> (Option<String>, Option<String>) {
    match target {
        Target::Channel(id) => (Some(id.clone()), None),
        Target::Direct(id) => (None, Some(id.clone())),
    }
}

fn valid_admin_path(path: &str) -> bool {
    path.starts_with("/api/")
        && !path.starts_with("/api/local/")
        && !path.contains(['?', '#', '\\', '%'])
        && !path.contains("..")
        && !path.contains("//")
        && path.len() <= 256
}

// Keep the total number of workers bounded even when a cancelled HTTP read
// takes time to return after switching communities.
struct TransferPermit(Arc<AtomicUsize>);
impl TransferPermit {
    fn acquire(active: Arc<AtomicUsize>) -> Result<Self, String> {
        active
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                (count < 2).then_some(count + 1)
            })
            .map_err(|_| "Two transfers are finishing. Wait briefly or cancel one.".to_string())?;
        Ok(Self(active))
    }
}
impl Drop for TransferPermit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Clone)]
struct TransferMeta {
    epoch: u64,
    operation_id: String,
    target: Option<Target>,
    path: PathBuf,
}
impl TransferMeta {
    fn result(&self, result: TransferOutcome) -> TransferResult {
        TransferResult {
            epoch: self.epoch,
            operation_id: self.operation_id.clone(),
            target: self.target.clone(),
            path: self.path.clone(),
            result,
        }
    }
}

struct ProgressReader {
    reader: Box<dyn Read + Send>,
    size: u64,
    read: u64,
    last: Instant,
    cancel: Arc<AtomicBool>,
    current_epoch: Arc<AtomicU64>,
    completed: idle::Sender<TransferResult>,
    template: TransferMeta,
}
impl Read for ProgressReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.cancel.load(Ordering::SeqCst)
            || self.current_epoch.load(Ordering::SeqCst) != self.template.epoch
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "Upload cancelled",
            ));
        }
        let n = self.reader.read(buffer)?;
        self.read += n as u64;
        if self.read > self.size || self.read > MAX_UPLOAD_BYTES as u64 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "File changed during upload",
            ));
        }
        if self.last.elapsed() >= Duration::from_millis(100) || self.read == self.size {
            let _ = self
                .completed
                .try_send(self.template.result(TransferOutcome::Progress {
                    transferred: self.read,
                    total: self.size,
                }));
            self.last = Instant::now();
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cooldown_parser_prefers_precise_body_validates_headers_and_bounds_every_fallback() {
        assert_eq!(
            retry_after_ms(&json!({"retry_after_ms": 125}), Some("9")),
            125
        );
        assert_eq!(
            retry_after_ms(&json!({"retry_after_ms": -1}), Some(" 3 ")),
            3_000
        );
        assert_eq!(
            retry_after_ms(&json!({"retry_after_ms": 0}), Some("2")),
            2_000
        );
        for header in [
            None,
            Some(""),
            Some("-1"),
            Some("NaN"),
            Some("1.5"),
            Some("0"),
            Some("18446744073709551616"),
        ] {
            assert_eq!(
                retry_after_ms(&json!({"retry_after_ms": "never"}), header),
                1_000
            );
        }
        assert_eq!(
            retry_after_ms(&json!({"retry_after_ms": u64::MAX}), None),
            MAX_COOLDOWN_MS
        );
        assert_eq!(
            retry_after_ms(&json!({}), Some("18446744073709551615")),
            MAX_COOLDOWN_MS
        );
        assert_eq!(retry_after_ms(&json!({"retry_after_ms": 1}), None), 1);
    }

    #[test]
    fn cooldown_scope_and_expiry_use_monotonic_time_and_do_not_cross_account_sessions() {
        let now = Instant::now();
        let room = Target::Channel("room".into());
        let direct = Target::Direct("peer".into());
        let mut state = ChatSession::default();
        state.remember_send_cooldown(
            room.clone(),
            Cooldown {
                error: "slow".into(),
                code: "slowmode".into(),
                until: now + Duration::from_secs(4),
                committed_client_id: None,
            },
        );
        assert!(state.send_cooldown(&room, "retry", now).is_some());
        assert!(state.send_cooldown(&direct, "other", now).is_none());
        assert!(state
            .send_cooldown(&room, "retry", now + Duration::from_secs(4))
            .is_none());
        state.remember_send_cooldown(
            room.clone(),
            Cooldown {
                error: "burst".into(),
                code: "message_rate_limit".into(),
                until: now + Duration::from_secs(5),
                committed_client_id: None,
            },
        );
        assert!(state.send_cooldown(&direct, "other", now).is_some());
        assert!(
            ChatSession::default()
                .send_cooldown(&room, "retry", now)
                .is_none(),
            "a replacement session must not inherit another account's local cooldown"
        );
        assert!(state
            .send_cooldown(&direct, "other", now + Duration::from_secs(5))
            .is_none());
    }

    #[test]
    fn changed_channel_interval_role_bypass_and_revocation_invalidate_old_slowmode() {
        let bootstrap = |seconds, bypass| Bootstrap {
            channels: vec![Channel {
                id: "room".into(),
                kind: "text".into(),
                slowmode_seconds: seconds,
                slowmode_bypass: bypass,
                ..Default::default()
            }],
            ..Default::default()
        };
        let target = Target::Channel("room".into());
        let now = Instant::now();
        for replacement in [
            bootstrap(0, false),
            bootstrap(5, false),
            bootstrap(10, true),
            Bootstrap::default(),
        ] {
            let mut state = ChatSession::new(&bootstrap(10, false));
            state.remember_send_cooldown(
                target.clone(),
                Cooldown {
                    code: "slowmode".into(),
                    error: "wait".into(),
                    until: now + Duration::from_secs(10),
                    committed_client_id: None,
                },
            );
            state.update_bootstrap(&bootstrap(10, false));
            assert!(state.send_cooldown(&target, "new", now).is_some());
            state.update_bootstrap(&replacement);
            assert!(
                state.send_cooldown(&target, "new", now).is_none(),
                "changed server permission/settings must not inherit the old interval"
            );
        }
    }

    #[test]
    fn duplicate_committed_ids_never_restart_slowmode_and_cache_stays_bounded() {
        let mut state = ChatSession::default();
        let target = Target::Channel("room".into());
        let start = Instant::now();
        assert!(state.remember_committed("committed-id"));
        state.remember_send_cooldown(
            target.clone(),
            Cooldown {
                code: "slowmode".into(),
                error: "wait".into(),
                until: start + Duration::from_secs(10),
                committed_client_id: Some("committed-id".into()),
            },
        );
        assert!(state
            .send_cooldown(&target, "committed-id", start + Duration::from_secs(8))
            .is_none());
        assert!(
            !state.remember_committed("committed-id"),
            "duplicate acknowledgement must not start a new interval"
        );
        assert!(state
            .send_cooldown(&target, "new-id", start + Duration::from_secs(10))
            .is_none());
        state.remember_send_cooldown(
            Target::Direct("peer".into()),
            Cooldown {
                code: "message_rate_limit".into(),
                error: "burst".into(),
                until: start + Duration::from_secs(30),
                committed_client_id: None,
            },
        );
        assert!(
            state
                .send_cooldown(&target, "committed-id", start)
                .is_none(),
            "known committed retry bypasses new-message account quota"
        );
        assert!(state.send_cooldown(&target, "new-id", start).is_some());
        for i in 0..100 {
            state.remember_committed(&format!("unique-{i}"));
        }
        assert_eq!(state.committed_ids.len(), 64);
        assert_eq!(
            state.committed_ids.back().map(String::as_str),
            Some("unique-99")
        );
        assert!(!state.committed_ids.iter().any(|id| id == "committed-id"));
        assert!(
            ChatSession::default().committed_ids.is_empty(),
            "replacement accounts do not inherit message IDs"
        );
    }

    #[test]
    fn typing_hints_require_an_unambiguous_current_account_recipient() {
        let event = |value| serde_json::from_value::<Event>(value).unwrap();
        assert_eq!(
            typing_target(
                &event(
                    json!({"kind":"typing","channel_id":"room","user_id":"peer","typing":false})
                ),
                "self"
            ),
            Some(Target::Channel("room".into()))
        );
        assert_eq!(
            typing_target(
                &event(
                    json!({"kind":"typing","user_id":"peer","recipient_id":"self","typing":true})
                ),
                "self"
            ),
            Some(Target::Direct("peer".into()))
        );
        for value in [
            json!({"kind":"typing","user_id":"peer","recipient_id":"outsider","typing":true}),
            json!({"kind":"typing","channel_id":"room","user_id":"peer","recipient_id":"self","typing":true}),
            json!({"kind":"typing","channel_id":"room","user_id":"self","typing":true}),
            json!({"kind":"typing","channel_id":"room","user_id":"peer"}),
            json!({"kind":"messages","channel_id":"room","user_id":"peer","typing":true}),
        ] {
            assert!(typing_target(&event(value), "self").is_none());
        }
    }

    #[test]
    fn typing_state_expiry_uses_server_time_deduplicates_self_and_caps_ttl_and_entries() {
        let user = |id: &str, expires_at_ms| TypingUser {
            user: User {
                id: id.into(),
                username: id.into(),
                role: "member".into(),
                ..Default::default()
            },
            expires_at_ms,
        };
        let state = filtered_typing(
            TypingState {
                server_time_ms: 1_000_000_000,
                users: vec![
                    user("expired", 999_999_999),
                    user("self", 1_000_008_000),
                    user("live", 1_000_000_001),
                    user("live", 1_000_001_000),
                    user("long", u64::MAX),
                    user("", 1_000_008_000),
                ],
            },
            "self",
        );
        assert_eq!(
            state
                .users
                .iter()
                .map(|entry| entry.user.id.as_str())
                .collect::<Vec<_>>(),
            ["live", "long"]
        );
        assert_eq!(state.users[1].expires_at_ms, 1_000_008_000);
        let crowded = filtered_typing(
            TypingState {
                server_time_ms: 100,
                users: (0..100).map(|i| user(&i.to_string(), 8_100)).collect(),
            },
            "self",
        );
        assert_eq!(crowded.users.len(), 32);
    }

    #[test]
    fn ephemeral_event_inbox_coalesces_and_bounds_without_triggering_bootstrap_per_keypress() {
        let inbox = ChatEventInbox::default();
        let dirty = AtomicBool::new(false);
        for i in 0..100 {
            let event = serde_json::from_value(json!({"kind":"typing","channel_id":"room","user_id":format!("user-{i}"),"typing":true})).unwrap();
            queue_chat_event(&inbox, &dirty, event);
        }
        assert_eq!(inbox.lock().unwrap().len(), 64);
        queue_chat_event(
            &inbox,
            &dirty,
            serde_json::from_value(
                json!({"kind":"typing","channel_id":"room","user_id":"user-99","typing":false}),
            )
            .unwrap(),
        );
        assert_eq!(inbox.lock().unwrap().len(), 64);
        assert_eq!(inbox.lock().unwrap().back().unwrap().typing, Some(false));
        assert!(!dirty.load(Ordering::Relaxed));
        queue_chat_event(
            &inbox,
            &dirty,
            serde_json::from_value(json!({"kind":"messages","channel_id":"room"})).unwrap(),
        );
        assert!(dirty.load(Ordering::Relaxed));
    }
    #[test]
    fn admin_routes_never_escape_authenticated_server_namespace() {
        assert!(valid_admin_path("/api/channels/abc/permissions"));
        for path in [
            "https://other/api/admin",
            "/api/../private",
            "/api/%2e%2e",
            "/api//admin",
            "/api/admin?url=external",
        ] {
            assert!(!valid_admin_path(path));
        }
    }
    #[test]
    fn read_and_history_target_keys_are_separate_for_dms_and_channels() {
        assert_eq!(
            target_ids(&Target::Channel("a".into())),
            (Some("a".into()), None)
        );
        assert_eq!(
            target_ids(&Target::Direct("a".into())),
            (None, Some("a".into()))
        );
    }
}
