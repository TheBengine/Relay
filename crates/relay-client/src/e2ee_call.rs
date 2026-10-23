//! Durable encrypted call requests; live challenges are never restored.
use super::*;
use relay_crypto::CallFreshnessProof;
use relay_protocol::{CallKeyDelivery, CallKeyRequest, CryptoDevice, CryptoTarget};

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CallState {
    outbox: HashMap<String, CallKeyRequest>,
    pub(super) proofs: HashMap<String, Vec<CallFreshnessProof>>,
    receipts: HashMap<String, Vec<Receipt>>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    operation: String,
    body_hash: String,
    request_hash: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Challenge {
    version: u32,
    scope: String,
    operation_id: String,
    nonce: [u8; 32],
}

impl CallState {
    pub(super) fn is_empty(&self) -> bool {
        self.outbox.is_empty()
            && self.proofs.values().all(Vec::is_empty)
            && self.receipts.values().all(Vec::is_empty)
    }
    pub(super) fn valid(&self) -> bool {
        self.outbox.len() <= 128
            && self.proofs.len() <= 128
            && self.receipts.len() <= 128
            && self
                .outbox
                .values()
                .all(|r| r.body.len() <= 12 * 1024 && r.operation_id.len() <= 128)
            && self
                .proofs
                .values()
                .all(|p| p.len() <= relay_crypto::MAX_MEMBERS)
            && self.receipts.values().all(|r| r.len() <= 128)
    }
}

fn scope_for(server: &str, user: &str, target: &CryptoTarget) -> Result<String, String> {
    match (&target.channel_id, &target.peer_id) {
        (Some(id), None) if !id.is_empty() && id.len() <= 128 => {
            Ok(format!("{server}/channel/{id}"))
        }
        (None, Some(peer)) if peer != user && !peer.is_empty() && peer.len() <= 128 => {
            let mut users = [user, peer.as_str()];
            users.sort_unstable();
            Ok(format!("{server}/dm/{}/{}", users[0], users[1]))
        }
        _ => Err("The call-key request has an invalid conversation target.".into()),
    }
}

impl CryptoSession {
    /// A transport replacement must prove freshness again, even when this
    /// actor stays alive. Keep exact durable operations and old replay caches:
    /// finishing an old write is not proof for the new transport.
    pub fn invalidate_call_freshness(&mut self, scope: &str) {
        self.fresh_call_scopes.remove(scope);
        self.call_refresh_operations.remove(scope);
        self.call_challenges.remove(scope);
        self.call_challenge_attempts.remove(scope);
    }
    pub fn call_waiting_message(&self, scope: &str) -> String {
        let Some(group) = self.device.group_state(scope) else {
            return "Verify the other device before joining this encrypted call.".into();
        };
        if group.controller == *self.device.identity() {
            return "Preparing fresh call keys on this device. Relay retries automatically.".into();
        }
        if let Some(peer) = self.status.peers.iter().find(|p| {
            p.device_id == group.controller.device_id
                && p.user_id == group.controller.user_id
                && p.verified
        }) {
            return format!("Waiting for {}'s Relay to prepare fresh call keys. Keep that verified device online; retry is automatic.", peer.name);
        }
        "Waiting for the other verified device to prepare fresh call keys. Keep its Relay open; retry is automatic.".into()
    }
    pub fn call_inbox_targets(
        &mut self,
        hints: &[relay_protocol::CallKeyTarget],
    ) -> Result<Vec<CryptoTarget>, String> {
        if hints.len() > 128 {
            return Err("The private call-key inbox exceeds its limit.".into());
        }
        let own = self.device.identity();
        let mut targets = Vec::new();
        for hint in hints {
            if hint.server_id != own.server_id || hint.target.channel_id.is_some() {
                return Err(
                    "The private call-key inbox belongs to another community or target.".into(),
                );
            }
            let scope = scope_for(&own.server_id, &own.user_id, &hint.target)?;
            // Hints may only service a known, verified group that we manage.
            // They never create groups or approve devices from server metadata.
            if self
                .device
                .group_state(&scope)
                .is_some_and(|g| g.controller == *own && g.group_id == hint.group_id)
                && !targets.contains(&hint.target)
            {
                targets.push(hint.target.clone());
            }
        }
        if targets.is_empty() {
            return Ok(targets);
        }
        let start = self.call_inbox_cursor % targets.len();
        targets.rotate_left(start);
        let count = targets.len().min(4);
        self.call_inbox_cursor = start + count;
        targets.truncate(count);
        Ok(targets)
    }
    pub fn needs_call_refresh(&self, scope: &str) -> bool {
        !self.fresh_call_scopes.contains(scope)
    }
    pub fn has_call_challenges(&self, scope: &str) -> bool {
        self.state
            .call
            .proofs
            .get(scope)
            .is_some_and(|p| !p.is_empty())
    }
    pub fn pending_call_request(&self, scope: &str) -> Option<CallKeyRequest> {
        self.state.call.outbox.get(scope).cloned()
    }
    pub fn should_send_call_challenge(&self, scope: &str) -> bool {
        self.pending_call_request(scope).is_some()
            || !self.call_challenges.contains_key(scope)
            || self
                .call_challenge_attempts
                .get(scope)
                .is_none_or(|last| last.elapsed() >= std::time::Duration::from_secs(5))
    }
    pub fn stage_call_challenge(
        &mut self,
        scope: &str,
        target: CryptoTarget,
    ) -> Result<CallKeyRequest, String> {
        let own = self.device.identity();
        if scope_for(&own.server_id, &own.user_id, &target)? != scope {
            return Err("The call-key request belongs to another conversation.".into());
        }
        let group = self
            .device
            .group_state(scope)
            .ok_or("Prepare the encrypted conversation first.")?;
        if group.controller == *own {
            return Err("The key manager prepares its own call keys directly.".into());
        }
        if let Some(request) = self.pending_call_request(scope) {
            if request.target != target {
                return Err("The saved call-key request has a different target.".into());
            }
            // A restored outbox cannot adopt another actor's challenge.
            return Ok(request);
        }
        self.guard_control_ack(scope)?;
        let nonce = if let Some(nonce) = self.call_challenges.get(scope) {
            *nonce
        } else {
            let mut nonce = [0; 32];
            getrandom::getrandom(&mut nonce)
                .map_err(|_| "Fresh call keys could not be prepared.")?;
            nonce
        };
        let operation_id = uuid::Uuid::new_v4().to_string();
        let request = self.mutate(|device, state| {
            if state.call.outbox.len() >= 128 {
                return Err("Call-key request storage is full.".into());
            }
            let group = device
                .group_state(scope)
                .ok_or("The encrypted conversation is missing.")?;
            let group_id = group.group_id.clone();
            let epoch = group.epoch;
            let bytes = serde_json::to_vec(&Challenge {
                version: 1,
                scope: scope.into(),
                operation_id: operation_id.clone(),
                nonce,
            })
            .map_err(|_| "The call-key challenge could not be prepared.")?;
            let body = STANDARD.encode(device.encrypt(scope, &bytes)?);
            let request = CallKeyRequest {
                target,
                device_id: device.identity().device_id.clone(),
                group_id,
                epoch,
                operation_id,
                body,
            };
            state.call.outbox.insert(scope.into(), request.clone());
            Ok(request)
        })?;
        self.call_challenges.insert(scope.into(), nonce);
        self.call_challenge_attempts
            .insert(scope.into(), std::time::Instant::now());
        Ok(request)
    }
    pub fn committed_call_request(
        &mut self,
        scope: &str,
        delivery: &CallKeyDelivery,
    ) -> Result<(), String> {
        let request = self
            .pending_call_request(scope)
            .ok_or("The saved call-key request is missing.")?;
        let own = self.device.identity();
        if delivery.server_id != own.server_id
            || delivery.author_user_id != own.user_id
            || delivery.target != request.target
            || delivery.request != request
        {
            return Err(
                "The call-key acknowledgement changed. The saved request was preserved.".into(),
            );
        }
        self.mutate(|_, state| {
            state.call.outbox.remove(scope);
            Ok(())
        })
    }
    pub fn receive_call_challenge(
        &mut self,
        scope: &str,
        delivery: &CallKeyDelivery,
        allowed: &[CryptoDevice],
    ) -> Result<(), String> {
        let own = self.device.identity();
        let request = &delivery.request;
        let group = self
            .device
            .group_state(scope)
            .ok_or("The encrypted conversation is missing.")?;
        if group.controller != *own
            || delivery.server_id != own.server_id
            || scope_for(&own.server_id, &own.user_id, &delivery.target)? != scope
            || scope_for(&own.server_id, &delivery.author_user_id, &request.target)? != scope
            || request.group_id != group.group_id
            || request.operation_id.is_empty()
            || request.operation_id.len() > 128
            || request.body.len() > 12 * 1024
            || !allowed
                .iter()
                .any(|d| d.user_id == delivery.author_user_id && d.device_id == request.device_id)
        {
            return Err(
                "The call-key request does not match this verified conversation and device.".into(),
            );
        }
        let request_hash = cipher_hash(
            &serde_json::to_string(delivery).map_err(|_| "Invalid call-key delivery.")?,
        );
        let body_hash = cipher_hash(&request.body);
        if let Some(receipts) = self.state.call.receipts.get(scope) {
            if let Some(old) = receipts
                .iter()
                .find(|r| r.operation == request.operation_id || r.body_hash == body_hash)
            {
                if old.operation == request.operation_id && old.request_hash == request_hash {
                    return Ok(());
                }
                return Err(
                    "The authenticated call-key request was replayed with changed delivery data."
                        .into(),
                );
            }
        }
        let bytes = STANDARD
            .decode(&request.body)
            .map_err(|_| "Malformed call-key request.")?;
        if STANDARD.encode(&bytes) != request.body
            || bytes.len() > 8 * 1024
            || relay_crypto::application_epoch(&bytes)? != request.epoch
        {
            return Err("The call-key request has invalid ciphertext or epoch metadata.".into());
        }
        self.mutate(|device, state| {
            let relay_crypto::Received::Application { author, bytes } =
                device.receive(scope, &bytes)?
            else {
                return Err(
                    "The call-key request is not an authenticated application message.".into(),
                );
            };
            if author.server_id != delivery.server_id
                || author.user_id != delivery.author_user_id
                || author.device_id != request.device_id
                || bytes.len() > 2048
            {
                return Err("The call-key request has a different authenticated author.".into());
            }
            let challenge: Challenge = serde_json::from_slice(&bytes)
                .map_err(|_| "The call-key challenge is malformed.")?;
            if challenge.version != 1
                || challenge.scope != scope
                || challenge.operation_id != request.operation_id
            {
                return Err(
                    "The authenticated call-key challenge has different delivery data.".into(),
                );
            }
            let proofs = state.call.proofs.entry(scope.into()).or_default();
            if !proofs.iter().any(|p| p.identity == author)
                && proofs.len() >= relay_crypto::MAX_MEMBERS
            {
                return Err("Call-key challenge storage is full.".into());
            }
            proofs.retain(|p| p.identity != author);
            proofs.push(CallFreshnessProof {
                identity: author,
                nonce: challenge.nonce,
            });
            proofs.sort_by(|a, b| a.identity.cmp(&b.identity));
            let receipts = state.call.receipts.entry(scope.into()).or_default();
            receipts.push(Receipt {
                operation: request.operation_id.clone(),
                body_hash,
                request_hash,
            });
            if receipts.len() > 128 {
                receipts.remove(0);
            }
            Ok(())
        })
    }
    pub fn fresh_call_keys(
        &self,
        scope: &str,
        expected_users: &[String],
    ) -> Result<Arc<CallKeys>, String> {
        if self.needs_call_refresh(scope) {
            return Err(self.call_waiting_message(scope));
        }
        self.call_keys(scope, expected_users)
    }
}
