//! Background I/O carries only public routing/cursors. Private actors stay on
//! their owning command worker; no checkpoint reopening or active-call mutation.
use super::*;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use std::collections::VecDeque;

#[derive(Clone)]
pub(super) struct Plan {
    target: Target,
    scope: String,
    group: String,
    device: String,
    fingerprint: String,
    control: u64,
    application: u64,
}

pub(super) struct CatchUp {
    plan: Plan,
    contexts: Vec<CryptoContext>,
    messages: Vec<(Message, Option<u64>)>,
}

pub(super) fn plans(c: &Connection) -> Vec<Plan> {
    let Some(crypto) = c.crypto.as_ref() else {
        return Vec::new();
    };
    if crypto.device.identity().server_id != c.server_id
        || crypto.device.identity().user_id != c.session.user.id
    {
        return Vec::new();
    }
    crypto
        .device
        .scopes()
        .into_iter()
        .filter_map(|scope| {
            let group = crypto.device.group_state(&scope)?;
            let channel = format!("{}/channel/", c.server_id);
            let dm = format!("{}/dm/", c.server_id);
            let target = if let Some(id) = scope.strip_prefix(&channel) {
                Target::Channel(id.into())
            } else {
                let users = scope.strip_prefix(&dm)?;
                let (left, right) = users.split_once('/')?;
                Target::Direct(
                    if left == c.session.user.id {
                        right
                    } else if right == c.session.user.id {
                        left
                    } else {
                        return None;
                    }
                    .into(),
                )
            };
            Some(Plan {
                target,
                scope: scope.clone(),
                group: group.group_id.clone(),
                device: crypto.device.identity().device_id.clone(),
                fingerprint: crypto.advertised().fingerprint,
                control: crypto.cursor(&scope),
                application: crypto.application_cursor(&scope),
            })
        })
        .collect()
}

fn routed(mut request: RequestBuilder, target: &Target) -> RequestBuilder {
    request = match target {
        Target::Channel(id) => request.query(&[("channel_id", id)]),
        Target::Direct(id) => request.query(&[("peer_id", id)]),
    };
    request
}

fn change_target(change: &MessageChange, user: &str) -> Option<Target> {
    if let Some(id) = &change.channel_id {
        return Some(Target::Channel(id.clone()));
    }
    if change.user_id == user {
        change.recipient_id.clone().map(Target::Direct)
    } else if change.recipient_id.as_deref() == Some(user) {
        Some(Target::Direct(change.user_id.clone()))
    } else {
        None
    }
}

fn available(bootstrap: &Bootstrap, target: &Target) -> bool {
    match target {
        Target::Channel(id) => bootstrap.channels.iter().any(|channel| channel.id == *id),
        Target::Direct(id) => {
            id != &bootstrap.user.id
                && bootstrap.members.iter().any(|member| member.id == *id)
                && !bootstrap.social.blocked.contains(id)
        }
    }
}

/// A 403 can mean either changed conversation access or a revoked own device.
/// Verify the same signed-in community and current public device registration
/// before omitting a conversation. No private actor reaches this I/O thread.
fn confirm_device_after_denial(
    http: &Client,
    url: &str,
    token: &str,
    plan: &Plan,
    bootstrap: &mut Bootstrap,
    remaining: &impl Fn() -> Result<Duration, String>,
) -> Result<(), String> {
    let fresh: Bootstrap = read_json(
        checked(
            http.get(crate::embedded_network::endpoint(&format!(
                "{url}/api/bootstrap"
            )))
            .bearer_auth(token)
            .timeout(remaining()?)
            .send()
            .map_err(network_error)?,
        )?,
        8 * 1024 * 1024,
    )?;
    validate_bootstrap_identity(&bootstrap.server_id, &bootstrap.user.id, &fresh)?;
    *bootstrap = fresh;
    let devices: Vec<CryptoDevice> = read_json(
        checked(
            http.get(crate::embedded_network::endpoint(&format!(
                "{url}/api/e2ee/directory"
            )))
            .bearer_auth(token)
            .timeout(remaining()?)
            .send()
            .map_err(network_error)?,
        )?,
        8 * 1024 * 1024,
    )?;
    if devices.len() > 256
        || !devices.iter().any(|device| {
            device.user_id == bootstrap.user.id
                && device.device_id == plan.device
                && device.identity == plan.fingerprint
        })
    {
        return Err("The background encryption device is no longer registered. Reset this local encryption device to recover.".into());
    }
    Ok(())
}

pub(super) fn fetch(
    http: &Client,
    url: &str,
    token: &str,
    plans: Vec<Plan>,
    bootstrap: &mut Bootstrap,
    changes: &mut Vec<MessageChange>,
) -> Result<Vec<CatchUp>, String> {
    let server = bootstrap.server_id.clone();
    let user = bootstrap.user.id.clone();
    let deadline = Instant::now() + Duration::from_secs(8);
    let remaining = || {
        deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .map(|d| d.min(Duration::from_secs(2)))
            .ok_or_else(|| "Background encryption is still catching up.".to_string())
    };
    let mut result = Vec::new();
    let mut total_bytes = 0usize;
    let mut denied = Vec::new();
    'plans: for plan in plans {
        if !available(bootstrap, &plan.target) {
            denied.push(plan.target);
            continue;
        }
        let relevant = changes
            .iter()
            .any(|change| change_target(change, &user).as_ref() == Some(&plan.target));
        if !relevant {
            continue;
        }
        let mut cursor = plan.control;
        let mut contexts = Vec::new();
        for _ in 0..16 {
            let response = routed(
                http.get(crate::embedded_network::endpoint(&format!(
                    "{url}/api/e2ee/context"
                )))
                .bearer_auth(token)
                .query(&[
                    ("device_id", plan.device.clone()),
                    ("after", cursor.to_string()),
                ])
                .timeout(remaining()?),
                &plan.target,
            )
            .send()
            .map_err(network_error)?;
            if response.status() == reqwest::StatusCode::FORBIDDEN {
                confirm_device_after_denial(http, url, token, &plan, bootstrap, &remaining)?;
                denied.push(plan.target);
                continue 'plans;
            }
            let context: CryptoContext = read_json(checked(response)?, 8 * 1024 * 1024)?;
            let target = match &plan.target {
                Target::Channel(id) => CryptoTarget {
                    channel_id: Some(id.clone()),
                    peer_id: None,
                },
                Target::Direct(id) => CryptoTarget {
                    channel_id: None,
                    peer_id: Some(id.clone()),
                },
            };
            if context.server_id != server
                || context.target != target
                || context.group_id.as_deref() != Some(&plan.group)
                || context.roster.len() > relay_crypto::MAX_MEMBERS
                || context.devices.len() > 256
                || context.events.len() > 128
                || context.sequence < cursor
            {
                return Err("Invalid background encryption context.".into());
            }
            total_bytes = total_bytes.saturating_add(
                serde_json::to_vec(&context)
                    .map_err(|e| e.to_string())?
                    .len(),
            );
            if total_bytes > 64 * 1024 * 1024 {
                return Err("Background encryption refresh exceeds its memory limit.".into());
            }
            let mut last = cursor;
            for event in &context.events {
                if event.sequence <= last
                    || event.sequence > context.sequence
                    || event.group_id != plan.group
                    || event.body.len() > 768 * 1024
                {
                    return Err("Invalid background group-key event order.".into());
                }
                last = event.sequence;
            }
            let done = context.events.is_empty() || last == context.sequence;
            cursor = if context.events.is_empty() {
                context.sequence
            } else {
                last
            };
            contexts.push(context);
            if done {
                break;
            }
        }
        if contexts
            .last()
            .is_none_or(|context| cursor < context.sequence)
        {
            return Err("Background group-key history is still catching up.".into());
        }
        // Only changing keys requires draining earlier applications first.
        let mut messages = Vec::new();
        if cursor > plan.control {
            let mut after = plan.application;
            let mut bytes = 0usize;
            let mut complete = false;
            for _ in 0..42 {
                let response = routed(
                    http.get(crate::embedded_network::endpoint(&format!(
                        "{url}/api/history"
                    )))
                    .bearer_auth(token)
                    .query(&[("after", after.to_string()), ("limit", "200".into())])
                    .timeout(remaining()?),
                    &plan.target,
                )
                .send()
                .map_err(network_error)?;
                if response.status() == reqwest::StatusCode::FORBIDDEN {
                    confirm_device_after_denial(http, url, token, &plan, bootstrap, &remaining)?;
                    denied.push(plan.target);
                    continue 'plans;
                }
                let page: HistoryPage = read_json(checked(response)?, 8 * 1024 * 1024)?;
                if page.messages.len() > 200 || page.latest_sequence < after {
                    return Err("Invalid background protected history cursor.".into());
                }
                total_bytes = total_bytes
                    .saturating_add(serde_json::to_vec(&page).map_err(|e| e.to_string())?.len());
                if total_bytes > 64 * 1024 * 1024 {
                    return Err("Background encryption refresh exceeds its memory limit.".into());
                }
                let empty = page.messages.is_empty();
                for message in page.messages {
                    if message.sequence <= after {
                        return Err("Background history arrived out of order.".into());
                    }
                    after = message.sequence;
                    bytes = bytes.saturating_add(message.content.len());
                    if messages.len() >= 8192 || bytes > 32 * 1024 * 1024 {
                        return Err("Background protected history exceeds its limits.".into());
                    }
                    // Decode the public epoch hint on the I/O thread. It never
                    // authenticates a message or receives any private MLS actor.
                    let epoch = message
                        .content
                        .strip_prefix(crate::e2ee::MESSAGE_PREFIX)
                        .and_then(|s| STANDARD.decode(s).ok())
                        .and_then(|b| relay_crypto::application_epoch(&b).ok());
                    messages.push((message, epoch));
                }
                if empty || after >= page.latest_sequence {
                    complete = true;
                    break;
                }
            }
            if !complete {
                return Err("Background protected history is still catching up.".into());
            }
        }
        result.push(CatchUp {
            plan,
            contexts,
            messages,
        });
    }
    // A refreshed bootstrap may also have removed an earlier plan. Filter the
    // entire payload before the owning worker touches any application ratchet.
    result.retain(|update| {
        available(bootstrap, &update.plan.target) && !denied.contains(&update.plan.target)
    });
    changes.retain(|change| {
        change_target(change, &user)
            .is_some_and(|target| available(bootstrap, &target) && !denied.contains(&target))
    });
    Ok(result)
}

fn consume_batch(
    c: &mut Connection,
    target: &Target,
    scope: &str,
    messages: &mut [(Message, Option<u64>, bool)],
    binding: &mut Option<Vec<Message>>,
) -> Result<bool, String> {
    if let Some(mut ready) = binding.take() {
        // These applications are already authenticated and checkpointed. The
        // ordinary decoder now validates their public envelope and binds the
        // history IDs, without consuming their application ratchets again.
        super::encrypted::decrypt_in_connection(c, target, &mut ready);
        if c.crypto.as_ref().unwrap().device.is_blocked() {
            return Err("Encryption storage failed during background catch-up.".into());
        }
        return Ok(true);
    }
    let crypto = c
        .crypto
        .as_ref()
        .ok_or("Background encryption keys are unavailable.")?;
    let Some(epoch) = crypto.device.group_state(scope).map(|g| g.epoch) else {
        return Ok(false);
    };
    if !crypto.device.is_active(scope)? {
        return Ok(false);
    }
    let mut ready: Vec<_> = messages
        .iter_mut()
        .filter_map(|(message, sent, consumed)| {
            if !*consumed && sent.is_none_or(|s| s <= epoch) {
                *consumed = true;
                Some(message.clone())
            } else {
                None
            }
        })
        .take(32)
        .collect();
    if !ready.is_empty() {
        ready.sort_by_key(|message| message.sequence);
        let encrypted: Vec<_> = ready
            .iter()
            .enumerate()
            .filter(|(_, message)| message.content.starts_with(crate::e2ee::MESSAGE_PREFIX))
            .map(|(index, _)| index)
            .collect();
        let wires: Vec<_> = encrypted
            .iter()
            .map(|index| ready[*index].content.as_str())
            .collect();
        let decoded = c.crypto.as_mut().unwrap().decrypt_contents(scope, &wires);
        if c.crypto.as_ref().unwrap().device.is_blocked() {
            return Err("Encryption storage failed during background catch-up.".into());
        }
        // Keep ciphertext/public routing only; plaintext and file keys stay in
        // the private actor. Failed or legacy applications have no binding to
        // save. They retain the ordinary final-page protection handling.
        let authenticated: std::collections::HashSet<_> = encrypted
            .into_iter()
            .zip(decoded)
            .filter_map(|(index, result)| result.is_ok().then_some(index))
            .collect();
        let pending: Vec<_> = ready
            .into_iter()
            .enumerate()
            .filter_map(|(index, message)| authenticated.contains(&index).then_some(message))
            .collect();
        if !pending.is_empty() {
            *binding = Some(pending);
        }
        return Ok(true);
    }
    Ok(false)
}

fn save_application_cursor(
    c: &mut Connection,
    scope: &str,
    messages: &[(Message, Option<u64>, bool)],
) -> Result<(), String> {
    if let Some(last) = messages
        .iter()
        .take_while(|(_, _, consumed)| *consumed)
        .last()
        .map(|(m, _, _)| m.sequence)
    {
        c.crypto
            .as_mut()
            .unwrap()
            .application_committed(scope, last)?;
    }
    Ok(())
}

enum Step {
    Directory(Vec<CryptoDevice>, Vec<User>),
    Drain,
    ApplicationCursor,
    Control(CryptoEvent),
    ControlCursor(u64),
}

struct Progress {
    plan: Plan,
    messages: Vec<(Message, Option<u64>, bool)>,
    binding: Option<Vec<Message>>,
    steps: VecDeque<Step>,
}

pub(super) struct Incremental {
    groups: VecDeque<Progress>,
}

impl Incremental {
    pub(super) fn new(updates: Vec<CatchUp>) -> Self {
        let groups = updates
            .into_iter()
            .map(|update| {
                let mut steps = VecDeque::new();
                for context in update.contexts {
                    steps.push_back(Step::Directory(context.devices, context.roster));
                    if context.events.is_empty() {
                        steps.push_back(Step::ControlCursor(context.sequence));
                    }
                    for event in context.events {
                        steps.push_back(Step::Drain);
                        steps.push_back(Step::ApplicationCursor);
                        steps.push_back(Step::Control(event));
                    }
                }
                steps.push_back(Step::Drain);
                steps.push_back(Step::ApplicationCursor);
                let messages = update
                    .messages
                    .into_iter()
                    .map(|(m, epoch)| (m, epoch, false))
                    .collect();
                Progress {
                    plan: update.plan,
                    messages,
                    binding: None,
                    steps,
                }
            })
            .collect();
        Self { groups }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    /// One bounded batch or durable key/cursor operation per command-loop turn.
    pub(super) fn advance(&mut self, c: &mut Connection) -> Result<(), String> {
        let Some(progress) = self.groups.front_mut() else {
            return Ok(());
        };
        let plan = &mut progress.plan;
        let crypto = c
            .crypto
            .as_ref()
            .ok_or("Background encryption keys are unavailable.")?;
        if crypto.device.identity().server_id != c.server_id
            || crypto.device.identity().user_id != c.session.user.id
            || crypto.device.identity().device_id != plan.device
            || crypto.cursor(&plan.scope) != plan.control
            || crypto
                .device
                .group_state(&plan.scope)
                .is_none_or(|g| g.group_id != plan.group)
        {
            return Err("The background encryption actor changed while refreshing.".into());
        }
        let Some(step) = progress.steps.front() else {
            return Ok(());
        };
        match step {
            Step::Directory(devices, roster) => {
                let crypto = c.crypto.as_mut().unwrap();
                super::encrypted::update_directory_cache(crypto, devices, roster, false)?;
                // The same trust on first use as in the open community, so a
                // parked conversation can follow group updates that add new
                // people. Notices wait in the protected state until shown.
                crypto.trust_directory(false)?;
            }
            Step::Drain => {
                if consume_batch(
                    c,
                    &plan.target,
                    &plan.scope,
                    &mut progress.messages,
                    &mut progress.binding,
                )? {
                    return Ok(());
                }
            }
            Step::ApplicationCursor => save_application_cursor(c, &plan.scope, &progress.messages)?,
            Step::Control(event) => {
                // No private actor or old key is moved to an I/O worker.
                c.crypto
                    .as_mut()
                    .unwrap()
                    .apply_control(&plan.scope, event, &c.server_id, None)?;
                plan.control = event.sequence;
            }
            Step::ControlCursor(sequence) => {
                if *sequence != plan.control {
                    c.crypto
                        .as_mut()
                        .unwrap()
                        .set_cursor(&plan.scope, *sequence)?;
                    plan.control = *sequence;
                }
            }
        }
        progress.steps.pop_front();
        if progress.steps.is_empty() {
            self.groups.pop_front();
        }
        Ok(())
    }
}

pub(super) struct Pending {
    pub(super) result: BackgroundResult,
    keys: Incremental,
    deliveries: VecDeque<Vec<MessageChange>>,
    decoded: Vec<MessageChange>,
}

impl Pending {
    #[cfg(test)]
    pub(super) fn diagnostic_phase(&self) -> &'static str {
        if let Some(progress) = self.keys.groups.front() {
            if progress.binding.is_some() {
                return "bind_applications";
            }
            return match progress.steps.front() {
                Some(Step::Directory(_, _)) => "directory",
                Some(Step::Drain) => "drain_applications",
                Some(Step::ApplicationCursor) => "application_cursor",
                Some(Step::Control(event)) => match event.kind.as_str() {
                    "create" => "control_create",
                    "welcome" => "control_welcome",
                    "commit" => "control_commit",
                    _ => "control_other",
                },
                Some(Step::ControlCursor(_)) => "control_cursor",
                None => "finish_group",
            };
        }
        if self.deliveries.is_empty() {
            "complete_page"
        } else {
            "decode_delivery"
        }
    }
    pub(super) fn new(mut result: BackgroundResult, user: &str) -> Self {
        let keys = Incremental::new(std::mem::replace(&mut result.keys, Ok(Vec::new())).unwrap());
        let changes = result
            .changes
            .as_mut()
            .and_then(|r| r.as_mut().ok())
            .map(|page| std::mem::take(&mut page.changes))
            .unwrap_or_default();
        let mut grouped: Vec<(Option<Target>, Vec<MessageChange>)> = Vec::new();
        for change in changes {
            let target = change.message.as_ref().map(|m| match &m.channel_id {
                Some(id) => Target::Channel(id.clone()),
                None if m.author.id == user => {
                    Target::Direct(m.recipient_id.clone().unwrap_or_default())
                }
                None => Target::Direct(m.author.id.clone()),
            });
            if let Some((_, batch)) = grouped.iter_mut().find(|(existing, _)| existing == &target) {
                batch.push(change);
            } else {
                grouped.push((target, vec![change]));
            }
        }
        Self {
            result,
            keys,
            deliveries: grouped.into_iter().map(|(_, batch)| batch).collect(),
            decoded: Vec::new(),
        }
    }

    pub(super) fn advance(&mut self, c: &mut Connection) -> Result<bool, String> {
        if !self.keys.is_empty() {
            self.keys.advance(c)?;
            return Ok(false);
        }
        if let Some(mut changes) = self.deliveries.pop_front() {
            self.result
                .protections
                .extend(super::encrypted::decrypt_changes_in_connection(
                    c,
                    &mut changes,
                ));
            if c.crypto
                .as_ref()
                .is_some_and(|crypto| crypto.device.is_blocked())
            {
                return Err(
                    "Encryption storage failed while receiving background messages.".into(),
                );
            }
            self.decoded.extend(changes);
            return Ok(false);
        }
        self.decoded.sort_by_key(|change| change.sequence);
        if let Some(Ok(page)) = self.result.changes.as_mut() {
            page.changes = std::mem::take(&mut self.decoded);
        }
        Ok(true)
    }
}
