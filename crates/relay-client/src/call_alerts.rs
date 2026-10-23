//! Call and Friends-message alerts, driven by authenticated subscribers, never by paint.
use crate::{
    audio_settings::{AudioCue, AudioSettings},
    media::CallAlertAudio,
    network::{CommandOrigin, Target},
    notifications::{GenerationGuard, Notification, NotificationPolicy, Notifications},
};
use relay_protocol::Bootstrap;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[path = "message_alerts.rs"]
mod messages;

const CADENCE: Duration = Duration::from_secs(3);
const MAX_ACCOUNTS: usize = 32;
const MAX_CALLS: usize = 64;

#[derive(Clone, Default)]
pub(crate) struct Policy {
    pub notifications: bool,
    pub previews: bool,
    pub deafened: bool,
    pub disabled: bool,
    pub audio: AudioSettings,
    pub active: Option<CommandOrigin>,
    pub message_policies: HashMap<String, NotificationPolicy>,
    pub mentions_only: bool,
}

/// Cursor presence is authenticated at parsing; reset authority additionally
/// requires the subscriber's request to start after the last consumed page.
#[derive(Clone, Copy)]
pub(crate) struct MessageCursor {
    value: Option<u64>,
    requested: Option<Instant>,
}
impl MessageCursor {
    pub fn baseline(value: Option<u64>) -> Self {
        Self {
            value,
            requested: None,
        }
    }
    pub fn polled(value: Option<u64>, requested: Instant) -> Self {
        Self {
            value,
            requested: Some(requested),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
struct Key {
    origin: CommandOrigin,
    id: String,
}
struct Ring {
    key: Key,
    peer: String,
    name: String,
    incoming: bool,
    deadline: Instant,
    notified: bool,
    toast_retry: Option<Instant>,
    retired: bool,
    suppressed: bool,
    observed: bool,
    notification_guard: Option<GenerationGuard>,
}
impl Ring {
    fn revoke_notification(&mut self) -> bool {
        self.notification_guard.take().is_some_and(|guard| {
            let was_valid = guard.valid();
            guard.revoke();
            was_valid
        })
    }
}
/// An estimate from an authenticated response, not a synchronization guarantee.
/// Network delay and whole-second resolution are intentionally not called exact.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ClockReading {
    pub unix_seconds: u64,
    /// None is the explicitly uncalibrated legacy/local-clock fallback.
    pub server_sample_age: Option<Duration>,
}
impl ClockReading {
    pub fn deadline(self, expires_at: u64, now: Instant) -> Option<Instant> {
        now.checked_add(Duration::from_secs(
            expires_at.saturating_sub(self.unix_seconds),
        ))
    }
}
#[derive(Clone, Copy)]
struct ServerClock {
    unix_at_receipt: Duration,
    received: Instant,
}
impl ServerClock {
    fn estimate(self, now: Instant) -> Duration {
        self.unix_at_receipt
            .saturating_add(now.saturating_duration_since(self.received))
    }
    fn reading(self, now: Instant) -> ClockReading {
        let age = now.saturating_duration_since(self.received);
        ClockReading {
            unix_seconds: self.estimate(now).as_secs(),
            server_sample_age: Some(age),
        }
    }
}
struct Account {
    origin: CommandOrigin,
    generation: u64,
    quiet: bool,
    rings: Vec<Ring>,
    clock: Option<ServerClock>,
    received: Option<Instant>,
    message_guard: GenerationGuard,
    messages: Option<messages::Messages>,
}
impl Account {
    fn revoke_call_notifications(&mut self) {
        for ring in &mut self.rings {
            ring.revoke_notification();
        }
    }
    fn reading(&self, now: Instant, local_unix: u64) -> ClockReading {
        self.clock
            .map(|clock| clock.reading(now))
            .unwrap_or(ClockReading {
                unix_seconds: local_unix,
                server_sample_age: None, // Explicit legacy fallback, never server time.
            })
    }
    fn ringing(&self, id: &str, now: Instant) -> bool {
        self.rings
            .iter()
            .any(|r| r.key.id == id && r.observed && !r.retired && now < r.deadline)
    }
}
#[derive(Default)]
struct State {
    policy: Policy,
    accounts: Vec<Account>,
    generation: u64,
    selected: Option<Key>,
    next_sound: Option<Instant>,
    stopped: bool,
    revoked_messages: Vec<CommandOrigin>,
}
#[derive(Default)]
struct Effects {
    notifications: Vec<Notification>,
    sound: Option<AudioCue>,
    changed: bool,
    guards_changed: bool,
}

impl State {
    fn retire_alert_account(&mut self, origin: &CommandOrigin) {
        self.retire_message_account(origin);
        if let Some(account) = self.accounts.iter_mut().find(|a| &a.origin == origin) {
            account.revoke_call_notifications();
        }
    }
    fn bind(&mut self, origin: CommandOrigin) -> Option<u64> {
        if self.stopped
            || origin.url.is_empty()
            || origin.server_id.is_empty()
            || origin.user_id.is_empty()
        {
            return None;
        }
        let replaced: Vec<_> = self
            .accounts
            .iter()
            .filter(|a| a.origin.url == origin.url && a.origin != origin)
            .map(|a| a.origin.clone())
            .collect();
        for old in replaced {
            self.retire_alert_account(&old);
        }
        self.accounts
            .retain(|a| a.origin.url != origin.url || a.origin == origin);
        self.generation = self.generation.wrapping_add(1);
        if let Some(account) = self.accounts.iter_mut().find(|a| a.origin == origin) {
            account.revoke_call_notifications();
            self.revoked_messages.retain(|old| old != &origin);
            account.message_guard.revoke();
            account.message_guard = GenerationGuard::new();
            if let Some(messages) = &mut account.messages {
                messages.rebind();
            }
            account.generation = self.generation;
            account.clock = None;
            account.received = None;
            return Some(self.generation);
        }
        if self.accounts.len() >= MAX_ACCOUNTS {
            return None;
        }
        self.revoked_messages.retain(|old| old != &origin);
        self.accounts.push(Account {
            origin,
            generation: self.generation,
            quiet: false,
            rings: Vec::new(),
            clock: None,
            received: None,
            message_guard: GenerationGuard::new(),
            messages: None,
        });
        Some(self.generation)
    }
    #[cfg(test)]
    fn observe(
        &mut self,
        origin: &CommandOrigin,
        generation: u64,
        b: &Bootstrap,
        now: Instant,
        local_unix: u64,
    ) {
        self.observe_with_cursor(origin, generation, b, now, local_unix, None);
    }
    fn observe_with_cursor(
        &mut self,
        origin: &CommandOrigin,
        generation: u64,
        b: &Bootstrap,
        now: Instant,
        local_unix: u64,
        cursor: Option<MessageCursor>,
    ) {
        let Some(account) = self
            .accounts
            .iter_mut()
            .find(|a| &a.origin == origin && a.generation == generation)
        else {
            return;
        };
        if b.server_id != origin.server_id
            || b.user.id != origin.user_id
            || account.received.is_some_and(|last| now < last)
        {
            return;
        }
        account.received = Some(now);
        if b.social.server_time != 0 {
            // A backwards/older sample cannot extend a call or claim that the
            // retained estimate was freshly calibrated. Keep its original age.
            let sample = Duration::from_secs(b.social.server_time);
            if account
                .clock
                .is_none_or(|clock| sample >= clock.estimate(now))
            {
                account.clock = Some(ServerClock {
                    unix_at_receipt: sample,
                    received: now,
                });
            }
        }
        let estimated = account
            .clock
            .map(|clock| clock.estimate(now))
            .unwrap_or(Duration::from_secs(local_unix));
        let unix = estimated.as_secs();
        account.quiet = b
            .presence
            .iter()
            .any(|p| p.user_id == b.user.id && p.status == "dnd");
        account.observe_message_bootstrap(b, now, cursor);
        account.rings.retain_mut(|r| {
            if r.deadline <= now {
                r.revoke_notification();
                false
            } else {
                true
            }
        });
        // A terminal/missing call is latched until its original deadline. An old
        // snapshot cannot restart a ringtone or show a second toast afterwards.
        for ring in &mut account.rings {
            if !ring.observed {
                continue;
            }
            if let Some(call) = b.social.calls.iter().find(|c| c.id == ring.key.id) {
                // A forward sample may shorten expiry; it can never renew it.
                let remaining = Duration::from_secs(call.expires_at)
                    .saturating_sub(estimated)
                    .min(Duration::from_secs(45));
                ring.deadline = ring.deadline.min(now + remaining);
            }
            ring.retired |= !b.social.calls.iter().any(|c| {
                c.id == ring.key.id
                    && c.state == "ringing"
                    && c.expires_at > unix
                    && b.social.friends.contains(&ring.peer)
                    && !b.social.blocked.contains(&ring.peer)
                    && b.members.iter().any(|u| u.id == ring.peer)
                    && ((c.caller_id == origin.user_id && c.recipient_id == ring.peer)
                        || (c.recipient_id == origin.user_id && c.caller_id == ring.peer))
            });
        }
        for call in b.social.calls.iter().take(MAX_CALLS) {
            if call.state != "ringing"
                || call.expires_at <= unix
                || uuid::Uuid::parse_str(&call.id).is_err()
            {
                continue;
            }
            let (incoming, peer) = if call.recipient_id == b.user.id {
                (true, &call.caller_id)
            } else if call.caller_id == b.user.id {
                (false, &call.recipient_id)
            } else {
                continue;
            };
            if peer == &b.user.id
                || !b.social.friends.contains(peer)
                || b.social.blocked.contains(peer)
            {
                continue;
            }
            let Some(user) = b.members.iter().find(|u| &u.id == peer) else {
                continue;
            };
            let existing = account.rings.iter().position(|r| r.key.id == call.id);
            if existing.is_some_and(|index| account.rings[index].observed)
                || (existing.is_none() && account.rings.len() >= MAX_CALLS)
            {
                continue;
            }
            let suppressed = existing.is_some_and(|index| account.rings[index].suppressed);
            let mut ring = Ring {
                key: Key {
                    origin: origin.clone(),
                    id: call.id.clone(),
                },
                peer: peer.clone(),
                name: if user.profile.display_name.trim().is_empty() {
                    user.username.clone()
                } else {
                    user.profile.display_name.clone()
                },
                incoming,
                deadline: now
                    + Duration::from_secs(call.expires_at)
                        .saturating_sub(estimated)
                        .min(Duration::from_secs(45)),
                notified: false,
                toast_retry: None,
                retired: false,
                suppressed,
                observed: true,
                notification_guard: None,
            };
            if let Some(index) = existing {
                ring.deadline = ring.deadline.min(account.rings[index].deadline);
                account.rings[index] = ring;
            } else {
                account.rings.push(ring);
            }
        }
        // A silence placeholder grants no live UI. Only a validated live call
        // above can populate it; an authenticated missing/terminal snapshot
        // latches it as retired so a later stale snapshot cannot revive it.
        for ring in account.rings.iter_mut().filter(|r| !r.observed) {
            ring.observed = true;
        }
    }
    fn step(&mut self, now: Instant) -> Effects {
        let mut effects = Effects::default();
        let mut sound = None;
        // Windows can retain a notice/click independently of paint or account
        // navigation. Revoke the exact ring authority before any later delivery.
        for account in &mut self.accounts {
            for ring in &mut account.rings {
                if self.stopped
                    || self.policy.disabled
                    || !self.policy.notifications
                    || account.quiet
                    || ring.retired
                    || ring.suppressed
                    || now >= ring.deadline
                {
                    effects.guards_changed |= ring.revoke_notification();
                }
            }
        }
        if !self.stopped && !self.policy.disabled {
            for account in &mut self.accounts {
                for ring in account
                    .rings
                    .iter_mut()
                    .filter(|r| r.observed && !r.retired && !r.suppressed && now < r.deadline)
                {
                    if account.quiet {
                        continue;
                    }
                    if ring.incoming
                        && !ring.notified
                        && self.policy.notifications
                        && ring.toast_retry.is_none_or(|at| now >= at)
                    {
                        ring.notified = true;
                        let guard = GenerationGuard::new();
                        ring.notification_guard = Some(guard.clone());
                        effects.notifications.push(Notification {
                            title: "Incoming Relay call".into(),
                            body: if self.policy.previews {
                                format!("{} is calling. Open Relay to answer.", ring.name)
                            } else {
                                "Open Relay to answer your incoming call.".into()
                            },
                            origin: ring.key.origin.clone(),
                            target: Target::Direct(ring.peer.clone()),
                            guard: Some(guard),
                            background_only: false,
                        });
                    }
                    if !self.policy.deafened
                        && self.policy.audio.sounds_enabled
                        && self.policy.audio.sound_volume > 0.0
                        && (ring.incoming || self.policy.active.as_ref() == Some(&ring.key.origin))
                        && (sound
                            .as_ref()
                            .is_none_or(|(_, incoming): &(Key, bool)| ring.incoming && !*incoming))
                    {
                        sound = Some((ring.key.clone(), ring.incoming));
                    }
                }
            }
        }
        let key = sound.as_ref().map(|(key, _)| key.clone());
        effects.changed = self.selected != key;
        if effects.changed {
            self.selected = key;
            self.next_sound = None;
        }
        if let Some((_, incoming)) = sound {
            if self.next_sound.is_none_or(|at| now >= at) {
                effects.sound = Some(if incoming {
                    AudioCue::Ring
                } else {
                    AudioCue::Ringback
                });
                self.next_sound = Some(now + CADENCE);
            }
        }
        effects
    }
}

#[derive(Clone)]
pub(crate) struct Alerts(Arc<Mutex<Inner>>);
struct Inner {
    state: State,
    notifications: Notifications,
    audio: Option<CallAlertAudio>,
    message_sound_guard: Option<GenerationGuard>,
}

impl Alerts {
    pub fn new(notifications: Notifications, audio: Option<CallAlertAudio>) -> Self {
        Self(Arc::new(Mutex::new(Inner {
            state: State::default(),
            notifications,
            audio,
            message_sound_guard: None,
        })))
    }
    fn apply(inner: &mut Inner, now: Instant) {
        if inner.message_sound_guard.as_ref().is_some_and(|guard| {
            !guard.valid() || messages::message_cue(&inner.state.policy, true, false).is_none()
        }) {
            if let Some(audio) = &inner.audio {
                audio.stop();
            }
            inner.message_sound_guard = None;
        }
        let effects = inner.state.step(now);
        if effects.guards_changed {
            inner.notifications.refresh_guards();
        }
        if effects.changed {
            if let Some(audio) = &inner.audio {
                audio.stop();
            }
        }
        for notification in effects.notifications {
            let origin = notification.origin.clone();
            let target = notification.target.clone();
            let guard = notification.guard.clone();
            if inner.notifications.show(notification).is_err() {
                for account in inner
                    .state
                    .accounts
                    .iter_mut()
                    .filter(|a| a.origin == origin)
                {
                    for ring in account.rings.iter_mut().filter(|r| {
                        !r.retired
                            && !r.suppressed
                            && now < r.deadline
                            && Target::Direct(r.peer.clone()) == target
                            && r.notification_guard == guard
                    }) {
                        ring.revoke_notification();
                        ring.notified = false;
                        ring.toast_retry = Some(now + Duration::from_secs(1));
                    }
                }
            }
        }
        let mut message_delivered = false;
        let mut delivered_guard = None;
        let focused = inner.notifications.app_focused();
        let deliveries = inner.state.message_effects(now, unix_now(), focused);
        for delivery in deliveries {
            let guard = delivery.notification.guard.clone();
            if inner.notifications.show(delivery.notification).is_ok() {
                inner
                    .state
                    .delivered_message(&delivery.origin, &delivery.id);
                message_delivered = true;
                delivered_guard = guard;
            } else {
                inner
                    .state
                    .retry_message(&delivery.origin, &delivery.id, now);
            }
        }
        let sound = effects.sound.or_else(|| {
            messages::message_cue(
                &inner.state.policy,
                message_delivered,
                inner.state.selected.is_some(),
            )
        });
        if let (Some(cue), Some(audio)) = (sound, &inner.audio) {
            inner.message_sound_guard = if cue == AudioCue::Message {
                delivered_guard
            } else {
                None
            };
            audio.play(cue, inner.state.policy.audio.clone());
        }
    }
    pub fn configure(&self, policy: Policy) {
        let mut inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let audio_changed = inner.state.policy.audio != policy.audio;
        if inner.state.policy.previews != policy.previews {
            for account in &mut inner.state.accounts {
                account.revoke_call_notifications();
            }
            inner.notifications.refresh_guards();
        }
        if inner.state.policy.notifications != policy.notifications
            || inner.state.policy.previews != policy.previews
            || inner.state.policy.disabled != policy.disabled
            || inner.state.policy.message_policies != policy.message_policies
            || inner.state.policy.mentions_only != policy.mentions_only
        {
            Self::reset_message_guards(&mut inner);
            inner.notifications.refresh_guards();
        }
        inner.state.policy = policy;
        if audio_changed {
            if let Some(audio) = &inner.audio {
                audio.stop();
            }
        }
        Self::apply(&mut inner, Instant::now());
    }
    pub fn bind(&self, origin: CommandOrigin) -> Option<u64> {
        let mut inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let generation = inner.state.bind(origin.clone())?;
        inner.notifications.allow_account(origin);
        inner.notifications.refresh_guards();
        Some(generation)
    }
    #[cfg(test)]
    pub fn observe(&self, origin: &CommandOrigin, generation: u64, b: &Bootstrap) {
        self.observe_received(origin, generation, b, Instant::now());
    }
    #[cfg(test)]
    pub fn observe_received(
        &self,
        origin: &CommandOrigin,
        generation: u64,
        b: &Bootstrap,
        received: Instant,
    ) {
        self.observe_received_cursor(origin, generation, b, received, None);
    }
    pub fn observe_received_cursor(
        &self,
        origin: &CommandOrigin,
        generation: u64,
        b: &Bootstrap,
        received: Instant,
        cursor: Option<MessageCursor>,
    ) {
        let mut inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        inner
            .state
            .observe_with_cursor(origin, generation, b, received, unix_now(), cursor);
        inner.notifications.refresh_guards();
        Self::apply(&mut inner, Instant::now());
    }
    pub fn generation(&self, origin: &CommandOrigin) -> Option<u64> {
        let inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if inner.state.stopped {
            return None;
        }
        inner
            .state
            .accounts
            .iter()
            .find(|a| &a.origin == origin)
            .map(|a| a.generation)
    }
    pub fn clock(&self, origin: &CommandOrigin) -> Option<ClockReading> {
        let inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if inner.state.stopped {
            return None;
        }
        inner
            .state
            .accounts
            .iter()
            .find(|a| &a.origin == origin && a.received.is_some())
            .map(|account| account.reading(Instant::now(), unix_now()))
    }
    pub fn ringing(&self, origin: &CommandOrigin, id: &str) -> bool {
        let inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        !inner.state.stopped
            && inner
                .state
                .accounts
                .iter()
                .any(|a| &a.origin == origin && a.ringing(id, Instant::now()))
    }
    pub fn tick(&self) {
        Self::apply(
            &mut self.0.lock().unwrap_or_else(|e| e.into_inner()),
            Instant::now(),
        );
    }
    pub fn is_current(&self, origin: &CommandOrigin, generation: u64) -> bool {
        let inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        !inner.state.stopped
            && inner
                .state
                .accounts
                .iter()
                .any(|a| &a.origin == origin && a.generation == generation)
    }
    pub fn silence(&self, origin: &CommandOrigin, id: &str) {
        let mut inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        for account in inner
            .state
            .accounts
            .iter_mut()
            .filter(|a| &a.origin == origin)
        {
            if !account.rings.iter().any(|r| r.key.id == id) && account.rings.len() < MAX_CALLS {
                account.rings.push(Ring {
                    key: Key {
                        origin: origin.clone(),
                        id: id.into(),
                    },
                    peer: String::new(),
                    name: String::new(),
                    incoming: false,
                    deadline: Instant::now() + Duration::from_secs(45),
                    notified: true,
                    toast_retry: None,
                    retired: true,
                    suppressed: true,
                    observed: false,
                    notification_guard: None,
                });
            }
            for ring in account.rings.iter_mut().filter(|r| r.key.id == id) {
                ring.suppressed = true;
            }
        }
        Self::apply(&mut inner, Instant::now());
    }
    pub fn remove(&self, origin: &CommandOrigin, generation: u64) {
        let mut inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let existed = inner
            .state
            .accounts
            .iter()
            .any(|a| &a.origin == origin && a.generation == generation);
        if existed {
            inner.state.retire_alert_account(origin);
        }
        inner
            .state
            .accounts
            .retain(|a| &a.origin != origin || a.generation != generation);
        if existed {
            inner.notifications.remove_account(origin);
        }
        Self::apply(&mut inner, Instant::now());
    }
    pub fn forget_url(&self, url: &str) {
        let mut inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let removed: Vec<_> = inner
            .state
            .accounts
            .iter()
            .filter(|a| a.origin.url == url)
            .map(|a| a.origin.clone())
            .collect();
        for origin in removed {
            inner.state.retire_alert_account(&origin);
            inner.notifications.remove_account(&origin);
        }
        inner.state.accounts.retain(|a| a.origin.url != url);
        Self::apply(&mut inner, Instant::now());
    }
    pub fn poll_interval(&self, origin: &CommandOrigin, generation: u64) -> Duration {
        let inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if inner.state.accounts.iter().any(|a| {
            &a.origin == origin
                && a.generation == generation
                && a.rings
                    .iter()
                    .any(|r| !r.retired && r.deadline > Instant::now())
        }) {
            Duration::from_secs(1)
        } else {
            Duration::from_secs(5)
        }
    }
    pub fn stop(&self) {
        let mut inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        inner.state.stopped = true;
        let removed: Vec<_> = inner
            .state
            .accounts
            .iter()
            .map(|a| a.origin.clone())
            .collect();
        for origin in removed {
            inner.state.retire_alert_account(&origin);
        }
        for account in &inner.state.accounts {
            inner.notifications.remove_account(&account.origin);
        }
        inner.state.accounts.clear();
        inner.notifications.set_accounts(Vec::new());
        if let Some(audio) = &inner.audio {
            audio.stop();
        }
    }
    // Network fixtures count authenticated live calls, including silenced ones;
    // this is deliberately not a count of sounds/toasts selected for delivery.
    #[cfg(test)]
    pub(crate) fn active_rings(&self) -> usize {
        let inner = self.0.lock().unwrap();
        inner
            .state
            .accounts
            .iter()
            .map(|a| {
                a.rings
                    .iter()
                    .filter(|r| !r.retired && r.deadline > Instant::now())
                    .count()
            })
            .sum()
    }
    #[cfg(test)]
    pub(crate) fn owned_qa_call_notice_authorities(&self) -> usize {
        self.0
            .lock()
            .unwrap()
            .state
            .accounts
            .iter()
            .flat_map(|account| &account.rings)
            .filter(|ring| {
                ring.notification_guard
                    .as_ref()
                    .is_some_and(GenerationGuard::valid)
            })
            .count()
    }
}
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
#[path = "call_alerts_tests.rs"]
mod tests;
