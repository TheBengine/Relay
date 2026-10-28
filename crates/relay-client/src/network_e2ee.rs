//! MLS delivery integration. This worker is the only owner of private ratchets.
use super::*;
use crate::e2ee::{
    CryptoPeerStatus, CryptoProblemSource, CryptoSession, Envelope, MessageProtection,
};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use relay_crypto::{Identity, PublicDevice};
use sha2::Digest;

fn crypto_target(target: &Target) -> CryptoTarget {
    match target {
        Target::Channel(id) => CryptoTarget {
            channel_id: Some(id.clone()),
            peer_id: None,
        },
        Target::Direct(id) => CryptoTarget {
            channel_id: None,
            peer_id: Some(id.clone()),
        },
    }
}
fn scope_for(server: &str, own_user: &str, target: &Target) -> String {
    match target {
        Target::Channel(id) => format!("{server}/channel/{id}"),
        Target::Direct(peer) => {
            let mut users = [own_user, peer.as_str()];
            users.sort_unstable();
            format!("{server}/dm/{}/{}", users[0], users[1])
        }
    }
}

const MAX_DIRECTORY_DEVICES: usize = 256;
const MEMBERSHIP_BATCH_DEVICES: usize = 16;

#[cfg(test)]
thread_local! {
    static CONTROL_DELIVERY_SIZES: std::cell::RefCell<Vec<(usize, usize, usize)>> = const { std::cell::RefCell::new(Vec::new()) };
}
#[cfg(test)]
pub(super) fn take_control_delivery_sizes() -> Vec<(usize, usize, usize)> {
    CONTROL_DELIVERY_SIZES.with(|sizes| std::mem::take(&mut *sizes.borrow_mut()))
}

#[cfg(all(test, windows))]
#[path = "network_directory_tests.rs"]
mod directory_tests;

fn merge_directory_cache(
    current: &HashMap<String, PublicDevice>,
    incoming: &HashMap<String, PublicDevice>,
    is_pinned: impl Fn(&str) -> bool,
) -> Result<HashMap<String, PublicDevice>, String> {
    let mut next = HashMap::new();
    for (id, device) in current.iter().filter(|(id, _)| is_pinned(id)) {
        if next.len() >= MAX_DIRECTORY_DEVICES {
            return Err("The verified device directory exceeds its cache limit.".into());
        }
        next.insert(id.clone(), device.clone());
    }
    for (id, device) in incoming {
        if !next.contains_key(id) && next.len() >= MAX_DIRECTORY_DEVICES {
            return Err("The device directory is full. Remove old device verification before loading more devices.".into());
        }
        // A changed identity must replace its directory metadata, while its
        // original verification stays pinned in the protected MLS actor.
        next.insert(id.clone(), device.clone());
    }
    for (id, device) in current {
        if next.len() >= MAX_DIRECTORY_DEVICES {
            break;
        }
        next.entry(id.clone()).or_insert_with(|| device.clone());
    }
    Ok(next)
}

pub(super) fn update_directory_cache(
    crypto: &mut CryptoSession,
    devices: &[CryptoDevice],
    roster: &[User],
    complete_own_list: bool,
) -> Result<(), String> {
    if devices.len() > MAX_DIRECTORY_DEVICES {
        return Err("The encryption device directory exceeds its limit.".into());
    }
    let valid_id =
        |id: &str| !id.is_empty() && id.len() <= 128 && !id.chars().any(char::is_control);
    let own = crypto.device.identity();
    let mut incoming = HashMap::new();
    let mut own_incoming = HashMap::new();
    let mut seen = std::collections::HashSet::new();
    for public in devices {
        if !valid_id(&public.user_id)
            || !valid_id(&public.device_id)
            || !seen.insert(&public.device_id)
            || public.identity.len() != 64
            || !public.identity.bytes().all(|b| b.is_ascii_hexdigit())
            || public.key_package.len() > 24 * 1024
            || (public.device_id == own.device_id && public.user_id != own.user_id)
        {
            return Err("The encryption device directory contains invalid entries.".into());
        }
        let key_package = STANDARD
            .decode(&public.key_package)
            .map_err(|_| "Invalid public device KeyPackage.")?;
        if key_package.len() > 16 * 1024 {
            return Err("The public device KeyPackage exceeds its limit.".into());
        }
        let device = PublicDevice {
            identity: Identity {
                server_id: own.server_id.clone(),
                user_id: public.user_id.clone(),
                device_id: public.device_id.clone(),
            },
            fingerprint: public.identity.clone(),
            key_package,
        };
        if public.user_id == own.user_id {
            let (verified, trusted, changed, needs_review) = if public.device_id == own.device_id {
                let same = public.identity == crypto.status.own_fingerprint;
                (same, same, !same, false)
            } else {
                crate::e2ee::peer_trust(&crypto.device, &device)
            };
            own_incoming.insert(
                public.device_id.clone(),
                CryptoPeerStatus {
                    user_id: public.user_id.clone(),
                    name: "Your device".into(),
                    device_id: public.device_id.clone(),
                    fingerprint: public.identity.clone(),
                    verified,
                    trusted,
                    changed,
                    needs_review,
                },
            );
        }
        if device.identity != *own {
            incoming.insert(public.device_id.clone(), device);
        }
    }
    let next_directory = merge_directory_cache(&crypto.directory, &incoming, |id| {
        crypto.device.expected_fingerprint(id).is_some()
    })?;
    own_incoming
        .entry(own.device_id.clone())
        .or_insert_with(|| CryptoPeerStatus {
            user_id: own.user_id.clone(),
            name: "Your device".into(),
            device_id: own.device_id.clone(),
            fingerprint: crypto.status.own_fingerprint.clone(),
            verified: true,
            trusted: true,
            changed: false,
            needs_review: false,
        });
    if own_incoming.len() > MAX_DIRECTORY_DEVICES {
        return Err("Your device directory exceeds its cache limit.".into());
    }
    // A context may contain only the selected own device. Preserve useful spare
    // rows within the same bound; a full directory refresh replaces them.
    if !complete_own_list {
        for old in &crypto.status.own_devices {
            if own_incoming.len() >= MAX_DIRECTORY_DEVICES {
                break;
            }
            own_incoming
                .entry(old.device_id.clone())
                .or_insert_with(|| old.clone());
        }
    }
    let mut own_devices: Vec<_> = own_incoming.into_values().collect();
    own_devices.sort_by(|a, b| a.device_id.cmp(&b.device_id));
    let mut peers: Vec<_> = next_directory
        .values()
        .filter(|d| d.identity.user_id != own.user_id)
        .map(|device| {
            let name = roster
                .iter()
                .find(|u| u.id == device.identity.user_id)
                .map(|u| {
                    if u.profile.display_name.is_empty() {
                        u.username.clone()
                    } else {
                        u.profile.display_name.clone()
                    }
                })
                .unwrap_or_else(|| device.identity.user_id.clone());
            let (verified, trusted, changed, needs_review) =
                crate::e2ee::peer_trust(&crypto.device, device);
            CryptoPeerStatus {
                user_id: device.identity.user_id.clone(),
                name,
                device_id: device.identity.device_id.clone(),
                fingerprint: device.fingerprint.clone(),
                verified,
                trusted,
                changed,
                needs_review,
            }
        })
        .collect();
    peers.sort_by(|a, b| a.name.cmp(&b.name).then(a.device_id.cmp(&b.device_id)));
    crypto.directory = next_directory;
    crypto.status.own_devices = own_devices;
    crypto.status.peers = peers;
    crypto.status.notices = crypto.trust_notices();
    Ok(())
}
impl Worker {
    fn crypto_problem_generation(&self) -> u64 {
        self.connection
            .as_ref()
            .and_then(|c| c.crypto.as_ref())
            .map_or(0, CryptoSession::problem_generation)
    }

    fn note_crypto_problem(&mut self, source: CryptoProblemSource, error: &str) {
        if let Some(crypto) = self.connection.as_mut().and_then(|c| c.crypto.as_mut()) {
            crypto.note_problem(source, error);
        }
    }

    pub(super) fn crypto_scope(&self, target: &Target) -> Result<String, String> {
        let c = self
            .connection
            .as_ref()
            .ok_or("Connect to a community first.")?;
        Ok(scope_for(&c.server_id, &c.session.user.id, target))
    }
    pub(super) fn crypto(&mut self) -> Result<&mut CryptoSession, String> {
        self.connection
            .as_mut()
            .and_then(|c| c.crypto.as_mut())
            .ok_or("Encryption keys are not ready. Open Settings > Encryption.")
            .map_err(str::to_string)
    }
    pub(super) fn ensure_crypto(&mut self) -> Result<(), String> {
        let c = self
            .connection
            .as_mut()
            .ok_or("Connect to a community first.")?;
        if !c.chat.supports("e2ee_mls_v1") {
            return Err("Update this server to support end-to-end encryption. Relay will not send unencrypted chat or calls.".into());
        }
        if let Some(crypto) = &c.crypto {
            if crypto.device.identity().server_id != c.server_id
                || crypto.device.identity().user_id != c.session.user.id
            {
                return Err("The server or account identity changed. Encryption is paused.".into());
            }
        } else {
            c.crypto = Some(CryptoSession::open(&c.server_id, &c.session.user.id)?);
        }
        if c.crypto_registered {
            return Ok(());
        }
        self.flush_crypto_revocations()?;
        let c = self.connection.as_ref().unwrap();
        let own = c.crypto.as_ref().unwrap().advertised();
        let _: CryptoDevice = parse(
            self.request(reqwest::Method::PUT, "/api/e2ee/device")?
                .json(&RegisterCryptoDevice {
                    device_id: own.identity.device_id.clone(),
                    identity: own.fingerprint,
                    key_package: STANDARD.encode(own.key_package),
                })
                .send()
                .map_err(network_error)?,
        )?;
        self.publish_crypto_pool()?;
        self.connection.as_mut().unwrap().crypto_registered = true;
        Ok(())
    }
    fn flush_crypto_revocations(&mut self) -> Result<(), String> {
        for device_id in self.crypto()?.pending_revocations() {
            super::extended::path_id(&device_id)?;
            let response = self
                .request(
                    reqwest::Method::DELETE,
                    &format!("/api/e2ee/device/{device_id}"),
                )?
                .send()
                .map_err(network_error)?;
            if response.status() != reqwest::StatusCode::NOT_FOUND {
                checked(response)?;
            }
            self.crypto()?.committed_revocation(&device_id)?;
        }
        Ok(())
    }
    fn publish_crypto_pool(&mut self) -> Result<(), String> {
        let device_id = self.crypto()?.device.identity().device_id.clone();
        let status: CryptoKeyPackageStatus = parse(
            self.request(reqwest::Method::GET, "/api/e2ee/key-packages")?
                .query(&[("device_id", &device_id)])
                .send()
                .map_err(network_error)?,
        )?;
        if status.device_id != device_id || status.unused > 32 {
            return Err("Invalid device key package stock.".into());
        }
        if status.unused >= 16 {
            return Ok(());
        }
        let packages = self.crypto()?.key_packages((16 - status.unused) as usize)?;
        let _: serde_json::Value = self.post(
            "/api/e2ee/key-packages",
            &PublishCryptoKeyPackages {
                device_id,
                packages: packages
                    .into_iter()
                    .map(|p| STANDARD.encode(p.key_package))
                    .collect(),
            },
        )?;
        self.crypto()?.pool_committed()
    }
    fn flush_crypto_consumptions(&mut self) -> Result<(), String> {
        for receipt in self.crypto()?.pending_consumptions() {
            checked(
                self.request(reqwest::Method::POST, "/api/e2ee/key-packages/consume")?
                    .json(&receipt)
                    .send()
                    .map_err(network_error)?,
            )?;
            self.crypto()?.committed_consumption(&receipt)?;
        }
        Ok(())
    }
    fn crypto_context(&self, target: &Target, after: u64) -> Result<CryptoContext, String> {
        let c = self.connection.as_ref().ok_or("Not connected.")?;
        let device_id = c
            .crypto
            .as_ref()
            .ok_or("Encryption keys are not ready.")?
            .device
            .identity()
            .device_id
            .clone();
        let mut request = self
            .request(reqwest::Method::GET, "/api/e2ee/context")?
            .query(&[("device_id", device_id), ("after", after.to_string())])
            .timeout(Duration::from_secs(3));
        request = match target {
            Target::Channel(id) => request.query(&[("channel_id", id)]),
            Target::Direct(id) => request.query(&[("peer_id", id)]),
        };
        let context: CryptoContext = read_json(
            checked(request.send().map_err(network_error)?)?,
            8 * 1024 * 1024,
        )?;
        if context.server_id != c.server_id
            || context.target != crypto_target(target)
            || context.roster.len() > relay_crypto::MAX_MEMBERS
            || context.devices.len() > 256
            || context.events.len() > 128
            || context.sequence < after
        {
            return Err(
                "The encryption directory has invalid context or exceeds its limits.".into(),
            );
        }
        Ok(context)
    }
    /// Host side: remember the one-time secret of the invitation the server just
    /// created. `None` (an ordinary invitation) if anything about it is unusable.
    pub(super) fn issue_invitation_proof(
        &mut self,
        created: &serde_json::Value,
    ) -> Option<crate::invite_proof::InviteProof> {
        let id = created["id"].as_str()?;
        let expires = created["expires_at"].as_u64()?;
        self.crypto().ok()?.issue_invitation(id, expires).ok()
    }

    /// Friend side: prove this device came with the invitation, then pin the
    /// host's device the link named. The server stores the proof but cannot make
    /// or check it.
    pub(super) fn attest_invitation(
        &mut self,
        proof: &crate::invite_proof::InviteProof,
    ) -> Result<(), String> {
        self.ensure_crypto()?;
        let (server_id, user_id, device_id, fingerprint) = {
            let crypto = self.crypto()?;
            let identity = crypto.device.identity();
            (
                identity.server_id.clone(),
                identity.user_id.clone(),
                identity.device_id.clone(),
                crypto.status.own_fingerprint.clone(),
            )
        };
        let mac = crate::invite_proof::attest(
            proof.secret(),
            &server_id,
            &proof.invite_id,
            &user_id,
            &device_id,
            &fingerprint,
        );
        let request = self
            .request(reqwest::Method::PUT, "/api/e2ee/device/attestation")?
            .timeout(Duration::from_secs(2))
            .json(&PutInviteAttestation {
                device_id,
                invite_id: proof.invite_id.clone(),
                mac,
            });
        // One short retry covers a lost request or response. Reuse the exact
        // device-bound proof: a committed first request returns409 on replay,
        // because the server never replaces a device's existing attestation.
        // No secret or request is retained after this command finishes.
        let first = request
            .try_clone()
            .ok_or("Cannot prepare the invitation confirmation.")?;
        let response = match first.send() {
            Ok(response)
                if !response.status().is_server_error() && response.status().as_u16() != 408 =>
            {
                response
            }
            _ => request.send().map_err(network_error)?,
        };
        // 403: this account did not join with that invitation. 409: the device already
        // has a proof. Neither is an error for the person joining.
        if !matches!(response.status().as_u16(), 204 | 403 | 409) {
            checked(response)?;
        }
        self.crypto()?.remember_invited_host(proof, Instant::now());
        self.pin_invited_host(proof)
    }

    fn pin_invited_host(&mut self, proof: &crate::invite_proof::InviteProof) -> Result<(), String> {
        let devices: Vec<CryptoDevice> = read_json(
            checked(
                self.request(reqwest::Method::GET, "/api/e2ee/directory")?
                    .send()
                    .map_err(network_error)?,
            )?,
            8 * 1024 * 1024,
        )?;
        let bootstrap: Bootstrap = self.get("/api/bootstrap")?;
        update_directory_cache(self.crypto()?, &devices, &bootstrap.members, true)?;
        if self.crypto()?.approve_invited_host(proof)? {
            update_directory_cache(self.crypto()?, &devices, &bootstrap.members, true)?;
            self.publish_crypto_status();
            self.refresh_pending.store(true, Ordering::Relaxed);
        }
        Ok(())
    }

    fn update_crypto_directory(
        &mut self,
        devices: &[CryptoDevice],
        roster: &[User],
    ) -> Result<(), String> {
        update_directory_cache(self.crypto()?, devices, roster, false)?;
        self.trust_listed_devices(false)
    }

    /// Trust on first use for the listed devices, and tell the user about key
    /// changes and new devices on their own account. A person verified by
    /// hand is never trusted here; their changes wait in Encryption settings.
    fn trust_listed_devices(&mut self, complete: bool) -> Result<(), String> {
        let notices = self.crypto()?.trust_directory(complete)?;
        for notice in notices {
            let text = match notice.kind {
                crate::e2ee::TrustNoticeKind::CodeChanged => {
                    let name = self
                        .crypto()?
                        .status
                        .peers
                        .iter()
                        .find(|p| p.user_id == notice.user_id)
                        .map(|p| p.name.clone())
                        .unwrap_or_else(|| "Someone".into());
                    format!("{name}'s security code changed. Messages stay encrypted; compare codes in Encryption settings if you want to be sure.")
                }
                crate::e2ee::TrustNoticeKind::NewOwnDevice => "A new device was added to your account. If it wasn't you, revoke it in Encryption settings.".into(),
            };
            self.emit(Update::Notice(text));
        }
        Ok(())
    }

    /// The name of the first listed person with a device waiting for review
    /// among `devices`, if any.
    fn person_needing_review(&mut self, devices: &[CryptoDevice]) -> Result<Option<String>, String> {
        let crypto = self.crypto()?;
        Ok(devices.iter().find_map(|listed| {
            crypto
                .status
                .peers
                .iter()
                .find(|p| p.device_id == listed.device_id && p.needs_review)
                .map(|p| p.name.clone())
        }))
    }

    fn review_wait(&mut self, name: &str) -> String {
        self.send_waiting(
            SendWait::KeyChangeReview,
            &format!("{name}'s security code changed. Accept the new code in Encryption settings to keep using this conversation."),
        )
    }
    fn publish_crypto_status(&self) {
        let status = self
            .connection
            .as_ref()
            .and_then(|c| c.crypto.as_ref())
            .map(|c| {
                let mut status = c.status.clone();
                status.history = Some(c.history_usage());
                status
            })
            .unwrap_or_else(|| crate::e2ee::CryptoUiStatus {
                supported: self
                    .connection
                    .as_ref()
                    .is_some_and(|c| c.chat.supports("e2ee_mls_v1")),
                status: "Open Encryption settings to prepare protected chat and calls.".into(),
                ..Default::default()
            });
        self.emit(Update::CryptoStatus { status });
    }
    pub(super) fn publish_encryption_failure(&mut self, error: &str) {
        self.note_crypto_problem(CryptoProblemSource::Other, error);
        self.publish_recorded_encryption_failure(error);
    }

    fn publish_recorded_encryption_failure(&self, error: &str) {
        let mut status = self
            .connection
            .as_ref()
            .and_then(|c| c.crypto.as_ref())
            .map(|c| c.status.clone())
            .unwrap_or_default();
        status.supported = self
            .connection
            .as_ref()
            .is_some_and(|c| c.chat.supports("e2ee_mls_v1"));
        // Known operations recorded their typed source before publication.
        // Do not derive or duplicate provenance by comparing diagnostic text.
        if status.problem.is_none() {
            status.status = error.chars().take(2048).collect();
            status.problem = Some(status.status.clone());
        }
        if status.supported {
            if let Some(c) = &self.connection {
                if let Ok(request) = self.request(reqwest::Method::GET, "/api/e2ee/device") {
                    if let Ok(devices) = request
                        .timeout(Duration::from_secs(1))
                        .send()
                        .map_err(network_error)
                        .and_then(checked)
                        .and_then(|response| read_json::<Vec<CryptoDevice>>(response, 1024 * 1024))
                    {
                        status.own_devices = devices
                            .into_iter()
                            .filter(|d| d.user_id == c.session.user.id)
                            .take(4)
                            .map(|d| CryptoPeerStatus {
                                user_id: d.user_id,
                                name: "Your device".into(),
                                device_id: d.device_id,
                                fingerprint: d.identity,
                                verified: false,
                                trusted: false,
                                changed: false,
                                needs_review: false,
                            })
                            .collect();
                    }
                }
            }
        }
        self.emit(Update::CryptoStatus { status });
    }
    pub(super) fn refresh_encryption(&mut self) {
        if self.voice_recovery_expired()
            || self
                .connection
                .as_ref()
                .and_then(|c| c.voice_recovery.as_ref())
                .is_some_and(|r| !r.admitted)
        {
            return;
        }
        if self
            .connection
            .as_ref()
            .is_none_or(|c| !c.chat.supports("e2ee_mls_v1"))
        {
            return;
        }
        let open = self
            .connection
            .as_ref()
            .is_some_and(|c| c.chat.plain_allowed());
        if !open {
            let initialization_generation = self.crypto_problem_generation();
            let result = self.ensure_crypto();
            if let Err(error) = result {
                self.note_crypto_problem(CryptoProblemSource::Initialization, &error);
                if let Some(channel_id) = self
                    .connection
                    .as_ref()
                    .and_then(|c| c.voice_channel.clone())
                {
                    self.emit(Update::VoiceEncryptionPaused {
                        channel_id,
                        reconnecting: false,
                        error: error.clone(),
                    });
                }
                self.publish_recorded_encryption_failure(&error);
                return;
            }
            if let Ok(crypto) = self.crypto() {
                crypto.finish_problem_action(
                    &CryptoProblemSource::Initialization,
                    initialization_generation,
                    "Encryption keys are ready. Conversation access is checked separately.",
                );
            }
            // Look for friends using an invitation made here, or finish the
            // short, exact host pin from the invitation used to join. No
            // directory polling continues after that retry expires or settles.
            if self.crypto().is_ok_and(|crypto| {
                crypto.has_active_invitations() || crypto.has_pending_invited_host(Instant::now())
            }) {
                let _ = self.handle_crypto_command(Command::CryptoRefresh);
            }
        }
        let channel = self
            .connection
            .as_ref()
            .and_then(|c| c.voice_channel.clone());
        let generation = self.crypto_problem_generation();
        if !open {
            if let Err(error) = self.service_call_inbox() {
                self.note_crypto_problem(CryptoProblemSource::CallInbox, &error);
            } else if let Ok(crypto) = self.crypto() {
                crypto.finish_problem_action(
                    &CryptoProblemSource::CallInbox,
                    generation,
                    "Private call requests checked. Conversation access is checked separately.",
                );
            }
        }
        if let Some(channel_id) = channel {
            let call_problem = CryptoProblemSource::Call(
                self.crypto_scope(&Target::Channel(channel_id.clone()))
                    .unwrap_or_default(),
            );
            let mut recovered = true;
            match self.protected_call_keys(&channel_id) {
                Ok(keys) => {
                    if self.voice_recovery_expired() {
                        return;
                    }
                    let old = self.connection.as_ref().unwrap().crypto_call_epoch;
                    if old != Some(keys.epoch) {
                        let device_id = Some(keys.local_device_id.clone());
                        let ticket = self.post::<VoiceTicket, _>(
                            "/api/voice/join",
                            &VoiceJoin {
                                open_revision: self
                                    .connection
                                    .as_ref()
                                    .and_then(|c| c.chat.open_revision()),
                                channel_id: channel_id.clone(),
                                device_id,
                            },
                        );
                        match ticket {
                            Ok(mut ticket) => {
                                if self.voice_recovery_expired() {
                                    return;
                                }
                                self.remember_voice_admission(&ticket);
                                let c = self.connection.as_mut().unwrap();
                                let url = c.url.clone();
                                ticket.tunnel_url =
                                    Some(format!("{}/api/voice/tunnel", url.trim_end_matches('/')));
                                c.crypto_call_epoch = Some(keys.epoch);
                                c.sharing = false;
                                c.camera = false;
                                c.voice_recovery = None;
                                let pending = c.crypto_pending_join.take();
                                if let Some((wanted, request_id)) =
                                    pending.filter(|(wanted, _)| wanted == &channel_id)
                                {
                                    self.emit(Update::VoiceJoined {
                                        gateway: self.voice_gateway(&ticket),
                                        url,
                                        ticket,
                                        channel_id: wanted,
                                        request_id,
                                        keys,
                                    });
                                } else {
                                    self.emit(Update::VoiceEncryptionChanged {
                                        gateway: self.voice_gateway(&ticket),
                                        url,
                                        channel_id,
                                        ticket,
                                        keys,
                                    });
                                }
                            }
                            Err(error) => {
                                recovered = false;
                                self.note_crypto_problem(call_problem.clone(), &error);
                                self.emit(Update::VoiceEncryptionPaused {
                                    channel_id,
                                    error,
                                    reconnecting: false,
                                })
                            }
                        }
                    }
                }
                Err(error) => {
                    recovered = false;
                    self.connection.as_mut().unwrap().crypto_call_epoch = None;
                    self.note_crypto_problem(call_problem.clone(), &error);
                    self.emit(Update::VoiceEncryptionPaused {
                        channel_id,
                        error,
                        reconnecting: false,
                    });
                }
            }
            if recovered {
                if let Ok(crypto) = self.crypto() {
                    crypto.finish_problem_action(
                        &call_problem,
                        generation,
                        "Encryption is ready for this call.",
                    );
                }
            }
        }
        if open {
            self.emit(Update::CryptoStatus { status: crate::e2ee::CryptoUiStatus {
                supported: true, status: "End-to-end encryption is off for this server. The server manages call protection.".into(), ..Default::default()
            }});
        } else {
            self.publish_crypto_status();
        }
    }
    pub(super) fn handle_crypto_command(&mut self, command: Command) -> Result<(), String> {
        // A queued action can outlive its UI/account. Reject it before either
        // recording or publishing a problem in the replacement connection.
        let current_origin = match &command {
            Command::CryptoVerify { origin, .. }
            | Command::CryptoUnverify { origin, .. }
            | Command::CryptoAcceptKey { origin, .. }
            | Command::CryptoDismissNotice { origin, .. } => self
                .connection
                .as_ref()
                .is_some_and(|c| origin == &CommandOrigin::from(c)),
            Command::CryptoResetDevice {
                url,
                user_id,
                expected_device_id,
                ..
            } => self.connection.as_ref().is_some_and(|c| {
                &c.url == url
                    && &c.session.user.id == user_id
                    && c.crypto
                        .as_ref()
                        .map(|crypto| crypto.device.identity().device_id.as_str())
                        .unwrap_or("")
                        == expected_device_id
            }),
            Command::CryptoRevokeOwnDevice { url, user_id, .. } => self
                .connection
                .as_ref()
                .is_some_and(|c| &c.url == url && &c.session.user.id == user_id),
            _ => true,
        };
        if !current_origin {
            return Err(
                "This encryption request belongs to another community, account or device.".into(),
            );
        }
        let source = match &command {
            Command::CryptoRefresh => CryptoProblemSource::Directory,
            Command::CryptoVerify {
                user_id, device_id, ..
            } => CryptoProblemSource::DeviceAction(format!("verify/{user_id}/{device_id}")),
            Command::CryptoUnverify {
                user_id, device_id, ..
            } => CryptoProblemSource::DeviceAction(format!("unverify/{user_id}/{device_id}")),
            Command::CryptoAcceptKey {
                user_id, device_id, ..
            } => CryptoProblemSource::DeviceAction(format!("accept/{user_id}/{device_id}")),
            Command::CryptoResetDevice {
                expected_device_id, ..
            } => CryptoProblemSource::DeviceAction(format!("reset/{expected_device_id}")),
            Command::CryptoRevokeOwnDevice { device_id, .. } => {
                CryptoProblemSource::DeviceAction(format!("revoke/{device_id}"))
            }
            _ => CryptoProblemSource::Other,
        };
        let generation = self.crypto_problem_generation();
        let result = self.handle_crypto_command_inner(command);
        if let Err(error) = &result {
            self.note_crypto_problem(source, error);
            self.publish_recorded_encryption_failure(error);
        } else {
            if let Ok(crypto) = self.crypto() {
                let success = if source == CryptoProblemSource::Directory {
                    "Device list refreshed. Conversation access is checked separately."
                } else {
                    "Device check updated. Conversation access is checked separately."
                };
                crypto.finish_problem_action(&source, generation, success);
            }
            self.publish_crypto_status();
        }
        result
    }

    fn handle_crypto_command_inner(&mut self, command: Command) -> Result<(), String> {
        match &command {
            Command::CryptoVerify { origin, .. }
            | Command::CryptoUnverify { origin, .. }
            | Command::CryptoAcceptKey { origin, .. }
            | Command::CryptoDismissNotice { origin, .. } => {
                self.command_connection(origin)?;
            }
            _ => {}
        }
        if let Command::CryptoResetDevice {
            confirmation,
            url,
            user_id,
            expected_device_id,
        } = &command
        {
            let c = self
                .connection
                .as_ref()
                .ok_or("Sign in before replacing this device.")?;
            if &c.url != url
                || &c.session.user.id != user_id
                || confirmation != "RESET DEVICE"
                || !c.chat.supports("e2ee_mls_v1")
                || c.crypto
                    .as_ref()
                    .map(|crypto| crypto.device.identity().device_id.as_str())
                    .unwrap_or("")
                    != expected_device_id
            {
                return Err("The encryption recovery confirmation belongs to another community, account or device.".into());
            }
            if c.crypto.as_ref().is_some_and(|crypto| {
                crypto.device.identity().server_id != c.server_id
                    || crypto.device.identity().user_id != c.session.user.id
            }) {
                return Err("The encryption actor belongs to another account or community. Recovery was cancelled.".into());
            }
            if c.crypto
                .as_ref()
                .is_some_and(|crypto| !crypto.pending_revocations().is_empty())
            {
                return Err("The previous device reset is still waiting for server synchronization. Sign in or reconnect before resetting again.".into());
            }
            let server_id = c.server_id.clone();
            let old_device = c
                .crypto
                .as_ref()
                .map(|crypto| crypto.device.identity().device_id.clone());
            let storage_root = c
                .crypto
                .as_ref()
                .map(CryptoSession::storage_root)
                .transpose()?;
            if c.voice_channel.is_some() {
                self.post_empty("/api/voice/leave", &serde_json::json!({}))?;
            }
            let c = self.connection.as_mut().unwrap();
            c.voice_channel = None;
            c.voice_attempt = None;
            c.voice_admission = None;
            c.voice_recovery = None;
            c.crypto_pending_join = None;
            c.crypto_call_epoch = None;
            c.sharing = false;
            c.camera = false;
            c.crypto = None;
            c.crypto_registered = false;
            self.emit(Update::VoiceLeft);
            let new = if let Some(root) = storage_root {
                CryptoSession::reset_replacing(root, &server_id, user_id, old_device.as_deref())?
            } else {
                CryptoSession::reset(&server_id, user_id)?
            };
            self.connection.as_mut().unwrap().crypto = Some(new);
            self.ensure_crypto()?;
            self.handle_crypto_command(Command::CryptoRefresh)?;
            self.crypto()?.status.status = "A new encryption device was created. Compare its new fingerprint with the other members before sending. Previous history requires the old keys.".into();
            self.publish_crypto_status();
            return Ok(());
        }
        if let Command::CryptoRevokeOwnDevice {
            device_id,
            url,
            user_id,
        } = &command
        {
            let c = self
                .connection
                .as_ref()
                .ok_or("Sign in before revoking a device.")?;
            if &c.url != url || &c.session.user.id != user_id || !c.chat.supports("e2ee_mls_v1") {
                return Err("Device revocation belongs to another community or account.".into());
            }
            if c.crypto.as_ref().is_some_and(|crypto| {
                crypto.device.identity().server_id != c.server_id
                    || crypto.device.identity().user_id != c.session.user.id
            }) {
                return Err("The encryption actor belongs to another account or community. Revocation was cancelled.".into());
            }
            if c.crypto
                .as_ref()
                .is_some_and(|crypto| &crypto.device.identity().device_id == device_id)
            {
                return Err(
                    "Use Reset This Encryption Device to replace the device in use.".into(),
                );
            }
            super::extended::path_id(device_id)?;
            checked(
                self.request(
                    reqwest::Method::DELETE,
                    &format!("/api/e2ee/device/{device_id}"),
                )?
                .send()
                .map_err(network_error)?,
            )?;
            if self.connection.as_ref().is_some_and(|c| c.crypto.is_some()) {
                self.handle_crypto_command(Command::CryptoRefresh)?;
            } else {
                self.publish_encryption_failure("The device was revoked. Reset this local encryption device to recover sending.");
            }
            return Ok(());
        }
        self.ensure_crypto()?;
        match command {
            Command::CryptoRefresh => {
                let devices: Vec<CryptoDevice> = read_json(
                    checked(
                        self.request(reqwest::Method::GET, "/api/e2ee/directory")?
                            .send()
                            .map_err(network_error)?,
                    )?,
                    8 * 1024 * 1024,
                )?;
                if devices.len() > 256 {
                    return Err("The device directory exceeds its limit.".into());
                }
                let bootstrap: Bootstrap = self.get("/api/bootstrap")?;
                update_directory_cache(self.crypto()?, &devices, &bootstrap.members, true)?;
                // Friend side: finish a previously interrupted directory read.
                // The actor retains only the exact public pin, for at most30s.
                if self.crypto()?.retry_invited_host(Instant::now())? {
                    self.refresh_pending.store(true, Ordering::Relaxed);
                    update_directory_cache(self.crypto()?, &devices, &bootstrap.members, true)?;
                }
                // Host side: approve friends whose invitation proof checks out.
                let approved = self.crypto()?.apply_invitation_attestations(&devices)?;
                if !approved.is_empty() {
                    // The selected conversation was checked before this approval.
                    // Revisit it on the short update cycle to publish its Welcome
                    // rather than waiting for the next five-second idle refresh.
                    self.refresh_pending.store(true, Ordering::Relaxed);
                    update_directory_cache(self.crypto()?, &devices, &bootstrap.members, true)?;
                    for device_id in &approved {
                        let name = devices
                            .iter()
                            .find(|d| &d.device_id == device_id)
                            .and_then(|d| bootstrap.members.iter().find(|m| m.id == d.user_id))
                            .map(|user| {
                                if user.profile.display_name.is_empty() {
                                    user.username.clone()
                                } else {
                                    user.profile.display_name.clone()
                                }
                            })
                            .unwrap_or_else(|| "A friend".into());
                        self.emit(Update::Notice(format!(
                            "{name} joined and was verified through your invitation."
                        )));
                    }
                }
                // Everyone else listed is trusted on first use. A newly trusted
                // device may need a Welcome in the selected conversation.
                let before = self.crypto()?.device.pin_count();
                self.trust_listed_devices(true)?;
                if self.crypto()?.device.pin_count() != before {
                    self.refresh_pending.store(true, Ordering::Relaxed);
                }
            }
            Command::CryptoAcceptKey {
                user_id,
                device_id,
                fingerprint,
                ..
            } => {
                let public = self
                    .crypto()?
                    .directory
                    .get(&device_id)
                    .ok_or("Refresh the device list before accepting this security code.")?;
                if public.identity.user_id != user_id {
                    return Err("The security code belongs to another account.".into());
                }
                self.crypto()?.accept_new_key(&device_id, &fingerprint)?;
                self.refresh_pending.store(true, Ordering::Relaxed);
            }
            Command::CryptoDismissNotice { device_id, .. } => {
                self.crypto()?.dismiss_notice(&device_id)?;
            }
            Command::CryptoVerify {
                user_id,
                device_id,
                fingerprint,
                ..
            } => {
                let public = self
                    .crypto()?
                    .directory
                    .get(&device_id)
                    .ok_or("Refresh the device list before verification.")?;
                if public.identity.user_id != user_id {
                    return Err("The verification belongs to another account.".into());
                }
                self.crypto()?.approve(&device_id, &fingerprint)?;
                self.refresh_pending.store(true, Ordering::Relaxed);
            }
            Command::CryptoUnverify {
                user_id, device_id, ..
            } => {
                let public = self
                    .crypto()?
                    .directory
                    .get(&device_id)
                    .ok_or("Refresh the device list before removing verification.")?;
                if public.identity.user_id != user_id {
                    return Err("The verification belongs to another account.".into());
                }
                // Only the "Verified" mark goes; the same key stays trusted, so
                // calls and conversations continue unchanged.
                self.crypto()?.unapprove(&device_id)?;
            }
            _ => return Err("Invalid encryption operation.".into()),
        }
        self.publish_crypto_status();
        Ok(())
    }
    pub(super) fn flush_crypto_control(&mut self, scope: &str) -> Result<(), String> {
        let Some(batch) = self.crypto()?.pending_control(scope) else {
            return Ok(());
        };
        let own = self.crypto()?.device.identity().clone();
        let connection = self
            .connection
            .as_ref()
            .ok_or("Connect to a community first.")?;
        if own.server_id != connection.server_id
            || own.user_id != connection.session.user.id
            || batch.device_id != own.device_id
        {
            return Err("The pending key update belongs to another account or device. Its protected state was preserved.".into());
        }
        crate::e2ee::check_control_delivery(&batch)?;
        #[cfg(test)]
        CONTROL_DELIVERY_SIZES.with(|sizes| {
            sizes.borrow_mut().push((
                serde_json::to_vec(&batch).unwrap().len(),
                batch
                    .events
                    .iter()
                    .map(|e| STANDARD.decode(&e.body).unwrap().len())
                    .sum(),
                batch.events.iter().filter(|e| e.kind == "welcome").count(),
            ));
        });
        let response = self
            .request(reqwest::Method::POST, "/api/e2ee/events")?
            .json(&batch)
            .send()
            .map_err(network_error)?;
        if response.status() == reqwest::StatusCode::CONFLICT
            && self.crypto()?.device.pending_key_rotation(scope)?.is_some()
        {
            let target = match (&batch.target.channel_id, &batch.target.peer_id) {
                (Some(id), None) => Target::Channel(id.clone()),
                (None, Some(id)) => Target::Direct(id.clone()),
                _ => return Err("Invalid saved call-key update target.".into()),
            };
            let cursor = self.crypto()?.cursor(scope);
            let actual = self.crypto_context(&target, cursor)?;
            if actual.group_id.as_ref() != Some(&batch.group_id) {
                return Err(
                    "The call's encryption group changed. Its pending update was preserved.".into(),
                );
            }
            self.update_crypto_directory(&actual.devices, &actual.roster)?;
            let event = actual
                .events
                .first()
                .filter(|event| {
                    event.kind == "commit"
                        && event.group_id == batch.group_id
                        && cursor.checked_add(1) == Some(event.sequence)
                })
                .ok_or("Waiting for the competing call-key update to be delivered.")?;
            self.pause_call_for_key_change(&target);
            self.crypto()?
                .apply_control(scope, event, &actual.server_id, None)?;
            if self.crypto()?.pending_control(scope).is_none() {
                return Ok(());
            }
            return Err(
                "Waiting to finish the saved call-key update. No new call keys were released."
                    .into(),
            );
        }
        if response.status() == reqwest::StatusCode::CONFLICT
            && batch.events.first().is_some_and(|e| e.kind == "create")
        {
            let target = match (&batch.target.channel_id, &batch.target.peer_id) {
                (Some(id), None) => Target::Channel(id.clone()),
                (None, Some(id)) => Target::Direct(id.clone()),
                _ => return Err("Invalid pending group claim.".into()),
            };
            let actual = self.crypto_context(&target, 0)?;
            if actual
                .group_id
                .as_ref()
                .is_some_and(|id| id != &batch.group_id)
            {
                self.crypto()?.discard_initial_claim(scope)?;
                return Err("Another verified device started this conversation. Refresh to receive its keys.".into());
            }
        }
        let context: CryptoContext = read_json(checked(response)?, 8 * 1024 * 1024)?;
        let committed = control_ack_sequence(&batch, &context, &own)?;
        self.crypto()?.committed_control(scope, committed)
    }
    pub(super) fn ensure_crypto_conversation(
        &mut self,
        target: &Target,
    ) -> Result<CryptoContext, String> {
        let source = self
            .crypto_scope(target)
            .map(CryptoProblemSource::Conversation)
            .unwrap_or(CryptoProblemSource::Other);
        let generation = self.crypto_problem_generation();
        let result = self.prepare_crypto_conversation(target);
        if let Err(error) = &result {
            self.note_crypto_problem(source, error);
        } else if let Ok(crypto) = self.crypto() {
            crypto.finish_problem_action(
                &source,
                generation,
                "Keys are ready for this conversation.",
            );
        }
        self.publish_crypto_status();
        result
    }

    /// Records why a send is held back, together with the exact error it
    /// returns, so only that error becomes `Update::SendWaiting`.
    fn send_waiting(&mut self, wait: SendWait, message: &str) -> String {
        self.send_wait = Some((wait, message.to_owned()));
        message.to_owned()
    }

    fn prepare_crypto_conversation(&mut self, target: &Target) -> Result<CryptoContext, String> {
        self.ensure_crypto()?;
        let scope = self.crypto_scope(target)?;
        // Welcomes consume one-time KeyPackages. An already registered device
        // still needs fresh stock when entering further conversations. Never
        // reuse a consumed package or treat missing stock as new trust.
        if !self.crypto()?.device.is_active(&scope)? {
            self.publish_crypto_pool()?;
        }
        self.flush_crypto_consumptions()?;
        self.flush_crypto_control(&scope)?;
        let cursor = self.crypto()?.cursor(&scope);
        let mut context = self.crypto_context(target, cursor)?;
        self.update_crypto_directory(&context.devices, &context.roster)?;
        let mut mailbox = self.crypto_mailbox(target, &scope)?;
        // Each page and its cursor are saved in one transaction. A Welcome cannot
        // grant trust to devices merely because the delivery service lists them.
        for _ in 0..16 {
            let mut last = self.crypto()?.cursor(&scope);
            for event in &context.events {
                if event.sequence <= last
                    || event.group_id != context.group_id.clone().unwrap_or_default()
                    || event.body.len() > 768 * 1024
                {
                    return Err("Invalid or replayed group-key event.".into());
                }
                // Drain the previous epoch before its keys are retired. Public
                // epochs are scheduling hints; MLS authentication still decides acceptance.
                self.consume_crypto_mailbox(target, &scope, &mut mailbox)?;
                if event.kind == "commit" {
                    self.pause_call_for_key_change(target);
                }
                let receipt =
                    if event.kind == "welcome" && !self.crypto()?.device.is_active(&scope)? {
                        let own_id = self.crypto()?.device.identity().device_id.clone();
                        let package = context
                            .devices
                            .iter()
                            .find(|d| d.device_id == own_id)
                            .ok_or("Your reserved key package is missing.")?;
                        let bytes = STANDARD
                            .decode(&package.key_package)
                            .map_err(|_| "Invalid reserved key package.")?;
                        Some(ConsumeCryptoKeyPackage {
                            target: crypto_target(target),
                            device_id: own_id,
                            package_hash: format!("{:x}", sha2::Sha256::digest(&bytes)),
                            welcome_sequence: event.sequence,
                        })
                    } else {
                        None
                    };
                if let Err(error) =
                    self.crypto()?
                        .apply_control(&scope, event, &context.server_id, receipt)
                {
                    // A member this device must not trust silently (a person
                    // verified by hand) holds the update until the user decides.
                    if let Some(name) = self.person_needing_review(&context.devices)? {
                        return Err(self.review_wait(&name));
                    }
                    return Err(error);
                }
                last = event.sequence;
            }
            if context.events.is_empty() {
                // Invisible Welcomes to other devices can leave gaps in sequence.
                // An unchanged cursor needs no checkpoint: each one serializes on
                // the account-wide credential lock before the send can proceed.
                if context.sequence != last {
                    self.crypto()?.set_cursor(&scope, context.sequence)?;
                }
                break;
            }
            context = self.crypto_context(target, last)?;
            self.update_crypto_directory(&context.devices, &context.roster)?;
        }
        self.consume_crypto_mailbox(target, &scope, &mut mailbox)?;
        self.flush_crypto_consumptions()?;
        if !context.events.is_empty() && self.crypto()?.cursor(&scope) < context.sequence {
            return Err(self.send_waiting(
                SendWait::KeyUpdates,
                "More group-key updates are pending. Refresh this conversation before sending.",
            ));
        }
        let group = self.crypto()?.device.group_state(&scope).cloned();
        let own = self.crypto()?.device.identity().clone();
        let group_id = if let Some(group) = &group {
            group.group_id.clone()
        } else if context.group_id.is_some() {
            return Err(self.send_waiting(
                SendWait::DeviceApproval,
                "Waiting for this conversation's key manager to add this device. It happens automatically while they are online.",
            ));
        } else {
            uuid::Uuid::new_v4().to_string()
        };
        let may_handover = if let Some(group) = &group {
            if !context.devices.iter().any(|d| {
                d.user_id == group.controller.user_id && d.device_id == group.controller.device_id
            }) {
                self.crypto()?
                    .device
                    .member_identities_for_update(&scope)?
                    .iter()
                    .find(|m| {
                        context
                            .devices
                            .iter()
                            .any(|d| d.user_id == m.user_id && d.device_id == m.device_id)
                    })
                    == Some(&own)
            } else {
                false
            }
        } else {
            false
        };
        if group.as_ref().is_none_or(|g| g.controller == own) || may_handover {
            let current = if group.is_some() {
                self.crypto()?.device.member_identities_for_update(&scope)?
            } else {
                Vec::new()
            };
            let allowed_users: Vec<_> = context.roster.iter().map(|u| &u.id).collect();
            // Members this device does not trust with their current key: an
            // unknown device, or a key trust on first use replaced. The key
            // manager removes them; a replaced key is added again below.
            let revoked = if group.is_some() {
                self.crypto()?.device.untrusted_members(&scope)?
            } else {
                Vec::new()
            };
            let remove: Vec<_> = current
                .iter()
                .filter(|member| {
                    !allowed_users.contains(&&member.user_id)
                        || revoked.contains(&member.device_id)
                        || !context
                            .devices
                            .iter()
                            .any(|d| d.user_id == member.user_id && d.device_id == member.device_id)
                })
                .map(|m| m.device_id.clone())
                .collect();
            if !remove.is_empty() {
                self.pause_call_for_key_change(target);
                self.crypto()?.stage_membership(
                    &scope,
                    crypto_target(target),
                    group_id.clone(),
                    context.sequence,
                    &[],
                    &remove,
                    false,
                )?;
                self.flush_crypto_control(&scope)?;
                let cursor = self.crypto()?.cursor(&scope);
                context = self.crypto_context(target, cursor)?;
            }
            let current = if group.is_some() {
                self.crypto()?.device.members(&scope)?
            } else {
                Vec::new()
            };
            let crypto = self.crypto()?;
            let add: Vec<_> = context
                .devices
                .iter()
                .filter_map(|public| crypto.directory.get(&public.device_id).cloned())
                .filter(|public| {
                    !public.key_package.is_empty()
                        && !current.contains(&public.identity)
                        && crypto.device.is_approved(public)
                })
                .collect();
            if current.len().max(1) + add.len() > relay_crypto::MAX_MEMBERS {
                return Err(format!(
                    "This encrypted conversation supports up to {} devices.",
                    relay_crypto::MAX_MEMBERS
                ));
            }
            // A single 64-device Welcome batch exceeds the server's body budget.
            // Finish each exact saved operation before staging the next epoch;
            // applications and call keys are released only after this loop.
            let batches = add.len().max(1).div_ceil(MEMBERSHIP_BATCH_DEVICES);
            for index in 0..batches {
                if group.is_some() && add.is_empty() {
                    break;
                }
                let start = index * MEMBERSHIP_BATCH_DEVICES;
                let end = (start + MEMBERSHIP_BATCH_DEVICES).min(add.len());
                let peers = &add[start..end];
                if peers.iter().any(|peer| {
                    !context.devices.iter().any(|public| {
                        public.device_id == peer.identity.device_id
                            && public.user_id == peer.identity.user_id
                            && public.identity == peer.fingerprint
                            && public.key_package == STANDARD.encode(&peer.key_package)
                    })
                }) {
                    return Err("The conversation's devices changed while preparing keys. Refresh before sending.".into());
                }
                self.pause_call_for_key_change(target);
                self.crypto()?.stage_membership(
                    &scope,
                    crypto_target(target),
                    group_id.clone(),
                    context.sequence,
                    peers,
                    &[],
                    group.is_none() && index == 0,
                )?;
                self.flush_crypto_control(&scope)?;
                let cursor = self.crypto()?.cursor(&scope);
                context = self.crypto_context(target, cursor)?;
            }
        }
        // Sending needs every member trusted with its current key. A member
        // whose automatically trusted key changed waits for the key manager to
        // replace it; a person verified by hand waits for the user's decision.
        let members = match self.crypto()?.device.members(&scope) {
            Ok(members) => members,
            Err(error) => {
                self.pause_call_for_key_change(target);
                if let Some(name) = self.person_needing_review(&context.devices)? {
                    return Err(self.review_wait(&name));
                }
                if !self.crypto()?.device.untrusted_members(&scope)?.is_empty() {
                    return Err(self.send_waiting(
                        SendWait::KeyRotation,
                        "A device in this conversation has a new security code. Waiting for the conversation's key manager to update its keys.",
                    ));
                }
                return Err(error);
            }
        };
        if members.iter().any(|m| {
            !context
                .devices
                .iter()
                .any(|d| d.user_id == m.user_id && d.device_id == m.device_id)
        }) {
            self.pause_call_for_key_change(target);
            return Err(self.send_waiting(
                SendWait::KeyRotation,
                "A device left this conversation. Waiting for its key manager to rotate keys.",
            ));
        }
        // Devices trusted on first use that the key manager has not added yet
        // do not hold back sending: they read along from the epoch that adds
        // them, as any later member does. Nothing is shared with them early.
        if let Target::Direct(peer) = target {
            if !members.iter().any(|m| &m.user_id == peer) {
                let peer_devices: Vec<_> = context
                    .devices
                    .iter()
                    .filter(|d| &d.user_id == peer)
                    .cloned()
                    .collect();
                if let Some(name) = self.person_needing_review(&peer_devices)? {
                    return Err(self.review_wait(&name));
                }
                return Err(self.send_waiting(
                    SendWait::PeerUnavailable,
                    "The other person has no device ready for encrypted messages yet. Relay sets it up when they open Relay.",
                ));
            }
        }
        if self.service_call_challenges(target, &scope, &context)? {
            let cursor = self.crypto()?.cursor(&scope);
            context = self.crypto_context(target, cursor)?;
        }
        Ok(context)
    }
    fn crypto_mailbox(
        &mut self,
        target: &Target,
        scope: &str,
    ) -> Result<Vec<(Message, Option<u64>, bool)>, String> {
        if matches!(target, Target::Channel(id) if self.connection.as_ref().and_then(|c| c.voice_channel.as_ref()) == Some(id))
        {
            return Ok(Vec::new());
        }
        let mut cursor = self.crypto()?.application_cursor(scope);
        let mut mailbox = Vec::new();
        let mut total_bytes = 0usize;
        for _ in 0..42 {
            let mut request = self
                .request(reqwest::Method::GET, "/api/history")?
                .query(&[("after", cursor.to_string()), ("limit", "200".into())])
                .timeout(Duration::from_secs(3));
            request = match target {
                Target::Channel(id) => request.query(&[("channel_id", id)]),
                Target::Direct(id) => request.query(&[("peer_id", id)]),
            };
            let page: HistoryPage = read_json(
                checked(request.send().map_err(network_error)?)?,
                8 * 1024 * 1024,
            )?;
            if page.messages.len() > 200 || page.latest_sequence < cursor {
                return Err("The protected history cursor is invalid.".into());
            }
            if page.messages.is_empty() {
                return Ok(mailbox);
            }
            for message in page.messages {
                if message.sequence <= cursor {
                    return Err("Protected history arrived out of order.".into());
                }
                cursor = message.sequence;
                total_bytes = total_bytes.saturating_add(message.content.len());
                if mailbox.len() >= 8192 || total_bytes > 32 * 1024 * 1024 {
                    return Err(
                        "This conversation exceeds protected history storage limits.".into(),
                    );
                }
                let epoch = message
                    .content
                    .strip_prefix(crate::e2ee::MESSAGE_PREFIX)
                    .and_then(|encoded| STANDARD.decode(encoded).ok())
                    .and_then(|bytes| relay_crypto::application_epoch(&bytes).ok());
                mailbox.push((message, epoch, false));
            }
            if cursor >= page.latest_sequence {
                return Ok(mailbox);
            }
        }
        Err("Protected history is still catching up. Refresh before sending.".into())
    }
    fn consume_crypto_mailbox(
        &mut self,
        target: &Target,
        scope: &str,
        mailbox: &mut [(Message, Option<u64>, bool)],
    ) -> Result<(), String> {
        let Some(epoch) = self.crypto()?.device.group_state(scope).map(|g| g.epoch) else {
            return Ok(());
        };
        if !self.crypto()?.device.is_active(scope)? {
            return Ok(());
        }
        let ready: Vec<_> = mailbox
            .iter_mut()
            .filter_map(|(message, sent_epoch, consumed)| {
                if !*consumed && sent_epoch.is_none_or(|sent| sent <= epoch) {
                    *consumed = true;
                    Some(message.clone())
                } else {
                    None
                }
            })
            .collect();
        if !ready.is_empty() {
            let mut messages = ready;
            self.decrypt_messages(target, &mut messages);
            if self.crypto()?.device.is_blocked() {
                return Err("Encryption storage failed while receiving protected history.".into());
            }
        }
        if let Some(last) = mailbox
            .iter()
            .take_while(|(_, _, consumed)| *consumed)
            .last()
            .map(|(m, _, _)| m.sequence)
        {
            self.crypto()?.application_committed(scope, last)?;
        }
        Ok(())
    }
    fn pause_call_for_key_change(&mut self, target: &Target) {
        if let Target::Direct(peer) = target {
            self.emit(Update::WhisperInvalidated { peer: peer.clone() });
            return;
        }
        let Some(c) = self.connection.as_mut() else {
            return;
        };
        let Target::Channel(channel) = target else {
            return;
        };
        if c.voice_channel.as_ref() == Some(channel) && c.crypto_call_epoch.take().is_some() {
            self.emit(Update::VoiceEncryptionPaused {
                channel_id: channel.clone(),
                reconnecting: false,
                error: "The call's devices changed. Updating protected call keys.".into(),
            });
        }
    }
    pub(super) fn protected_whisper_keys(
        &mut self,
        peer: &str,
    ) -> Result<Arc<crate::e2ee::CallKeys>, String> {
        let target = Target::Direct(peer.into());
        let context = self.ensure_crypto_conversation(&target)?;
        let scope = self.crypto_scope(&target)?;
        if context.roster.len() != 2 || !context.roster.iter().any(|user| user.id == peer) {
            return Err("Whisper needs a verified private conversation with this person.".into());
        }
        self.refresh_controller_call_keys(&target, &scope, context.sequence)?;
        self.crypto()?.fresh_call_keys(
            &scope,
            &context
                .roster
                .iter()
                .map(|u| u.id.clone())
                .collect::<Vec<_>>(),
        )
    }
    pub(super) fn protected_call_keys(
        &mut self,
        channel: &str,
    ) -> Result<Arc<crate::e2ee::CallKeys>, String> {
        if self.voice_recovery_expired()
            || self
                .connection
                .as_ref()
                .and_then(|c| c.voice_recovery.as_ref())
                .is_some_and(|r| !r.admitted)
        {
            return Err("Reconnecting the call before preparing media.".into());
        }
        if self.connection.as_ref().is_some_and(|c| c.chat.is_open()) {
            return self.server_managed_call_keys(channel);
        }
        let target = Target::Channel(channel.into());
        let context = self.ensure_crypto_conversation(&target)?;
        let scope = self.crypto_scope(&target)?;
        self.refresh_controller_call_keys(&target, &scope, context.sequence)?;
        let keys = self.crypto()?.fresh_call_keys(
            &scope,
            &context
                .roster
                .iter()
                .map(|u| u.id.clone())
                .collect::<Vec<_>>(),
        )?;
        if self.voice_recovery_expired() {
            return Err("The call could not reconnect. Join the room again to retry.".into());
        }
        Ok(keys)
    }

    fn server_managed_call_keys(
        &mut self,
        channel: &str,
    ) -> Result<Arc<crate::e2ee::CallKeys>, String> {
        let c = self.connection.as_ref().ok_or("Not connected")?;
        if !c.chat.supports("protection-calls-v1") {
            return Err(
                "Update the host's Relay to use calls without end-to-end encryption.".into(),
            );
        }
        let revision = c
            .chat
            .open_revision()
            .ok_or("Accept this server's encryption choice before joining voice.")?;
        let server_id = c.server_id.clone();
        let local_user = c.session.user.id.clone();
        let local_device = c.voice_device_id().ok_or("Call identity is unavailable")?;
        let material: OpenCallMaterial = self.post(
            "/api/protection/call",
            &VoiceJoin {
                channel_id: channel.into(),
                device_id: Some(local_device.clone()),
                open_revision: Some(revision),
            },
        )?;
        let group = format!("server-managed:{server_id}/{channel}");
        if material.revision != revision
            || material.group_id != group
            || material.epoch == 0
            || material.senders.len() > 32
        {
            return Err("Invalid server-managed call context".into());
        }
        let mut senders = HashMap::new();
        for sender in material.senders {
            let opus: [u8; 32] = STANDARD
                .decode(sender.opus)
                .map_err(|_| "Invalid call key")?
                .try_into()
                .map_err(|_| "Invalid call key")?;
            let rtc: [u8; 32] = STANDARD
                .decode(sender.rtc)
                .map_err(|_| "Invalid call key")?
                .try_into()
                .map_err(|_| "Invalid call key")?;
            if senders
                .insert(
                    sender.user_id,
                    crate::e2ee::SenderKeys {
                        device_id: sender.device_id,
                        opus: zeroize::Zeroizing::new(opus),
                        rtc: zeroize::Zeroizing::new(rtc),
                    },
                )
                .is_some()
            {
                return Err("Duplicate call participant".into());
            }
        }
        let c = self.connection.as_mut().unwrap();
        if let Some(old) = &c.open_call_context {
            if old.group_id == group && old.epoch == material.epoch {
                if old.local_user_id != local_user
                    || old.local_device_id != local_device
                    || old.sender_keys.len() != senders.len()
                    || senders.iter().any(|(user, sender)| {
                        old.sender_keys.get(user).is_none_or(|previous| {
                            previous.device_id != sender.device_id
                                || previous.opus != sender.opus
                                || previous.rtc != sender.rtc
                        })
                    })
                {
                    return Err("The call keys changed without a new generation".into());
                }
                return Ok(old.clone());
            }
            if old.group_id == group && old.epoch > material.epoch {
                return Err("The server returned an older call generation".into());
            }
        }
        let keys = Arc::new(crate::e2ee::CallKeys {
            group_id: group,
            scope: format!("{server_id}/channel/{channel}"),
            epoch: material.epoch,
            local_user_id: local_user,
            local_device_id: local_device,
            sender_keys: senders,
            voice_replay: Default::default(),
        });
        crate::media::validate_call_keys(&keys)?;
        c.open_call_context = Some(keys.clone());
        Ok(keys)
    }
    fn refresh_controller_call_keys(
        &mut self,
        target: &Target,
        scope: &str,
        sequence: u64,
    ) -> Result<(), String> {
        if !self.crypto()?.needs_call_refresh(scope) {
            return Ok(());
        }
        self.pause_call_for_key_change(target);
        if !self.crypto()?.needs_controller_call_refresh(scope) {
            if self
                .connection
                .as_ref()
                .is_none_or(|c| !c.chat.supports("e2ee-call-freshness-v1"))
            {
                return Err("This call needs the new encryption update. Update the local server and the other person's Relay, then try again.".into());
            }
            // First finish any old exact operation. Reopening does not adopt
            // its nonce; a second bounded iteration prepares this actor's one.
            for _ in 0..2 {
                if !self.crypto()?.should_send_call_challenge(scope) {
                    break;
                }
                let request = self
                    .crypto()?
                    .stage_call_challenge(scope, crypto_target(target))?;
                let delivery: CallKeyDelivery = read_json(
                    checked(
                        self.request(reqwest::Method::POST, "/api/e2ee/call-requests")?
                            .json(&request)
                            .timeout(Duration::from_secs(3))
                            .send()
                            .map_err(network_error)?,
                    )?,
                    16 * 1024,
                )?;
                self.crypto()?.committed_call_request(scope, &delivery)?;
            }
            return Err(self.crypto()?.call_waiting_message(scope));
        }
        self.crypto()?
            .stage_call_refresh(scope, crypto_target(target), sequence)?;
        self.flush_crypto_control(scope)?;
        if self.crypto()?.needs_controller_call_refresh(scope) {
            return Err(
                "Finishing a saved key update. Refresh to prepare fresh keys for this call.".into(),
            );
        }
        Ok(())
    }
    fn service_call_challenges(
        &mut self,
        target: &Target,
        scope: &str,
        context: &CryptoContext,
    ) -> Result<bool, String> {
        if matches!(target, Target::Channel(id) if self.connection.as_ref().and_then(|c| c.voice_channel.as_ref()) != Some(id))
        {
            return Ok(false);
        }
        if self
            .connection
            .as_ref()
            .is_none_or(|c| !c.chat.supports("e2ee-call-freshness-v1"))
        {
            return Ok(false);
        }
        let crypto = self.crypto()?;
        if crypto
            .device
            .group_state(scope)
            .is_none_or(|g| g.controller != *crypto.device.identity())
        {
            return Ok(false);
        }
        let device_id = crypto.device.identity().device_id.clone();
        let mut request = self
            .request(reqwest::Method::GET, "/api/e2ee/call-requests")?
            .query(&[("device_id", device_id.as_str())])
            .timeout(Duration::from_secs(3));
        request = match target {
            Target::Channel(id) => request.query(&[("channel_id", id)]),
            Target::Direct(id) => request.query(&[("peer_id", id)]),
        };
        let deliveries: Vec<CallKeyDelivery> = read_json(
            checked(request.send().map_err(network_error)?)?,
            1024 * 1024,
        )?;
        if deliveries.len() > relay_crypto::MAX_MEMBERS {
            return Err("The call-key request page exceeds its limit.".into());
        }
        for delivery in &deliveries {
            // Never let untrusted routing ACK another conversation's request.
            if delivery.server_id != context.server_id
                || delivery.target != crypto_target(target)
                || delivery.request.group_id != context.group_id.clone().unwrap_or_default()
            {
                return Err("The call-key request page belongs to another conversation.".into());
            }
            if let Err(error) =
                self.crypto()?
                    .receive_call_challenge(scope, delivery, &context.devices)
            {
                if self.crypto()?.device.is_blocked() {
                    return Err(error);
                }
                // Invalid opaque requests cannot grant keys. Retire that slot
                // so its sender can retry fresh encrypted control; each failed
                // authentication rolled back its own ratchet transaction.
                self.note_crypto_problem(CryptoProblemSource::Conversation(scope.into()), &error);
            }
            let ack = AckCallKeyRequest {
                target: crypto_target(target),
                device_id: device_id.clone(),
                group_id: delivery.request.group_id.clone(),
                operation_id: delivery.request.operation_id.clone(),
            };
            let confirmed: AckCallKeyRequest = read_json(
                checked(
                    self.request(reqwest::Method::POST, "/api/e2ee/call-requests/ack")?
                        .json(&ack)
                        .timeout(Duration::from_secs(3))
                        .send()
                        .map_err(network_error)?,
                )?,
                2048,
            )?;
            if confirmed != ack {
                return Err("The call-key delivery acknowledgement changed. Relay will retry the saved receipt.".into());
            }
        }
        if !self.crypto()?.has_call_challenges(scope) {
            return Ok(false);
        }
        self.pause_call_for_key_change(target);
        self.crypto()?
            .stage_call_refresh(scope, crypto_target(target), context.sequence)?;
        self.flush_crypto_control(scope)?;
        Ok(true)
    }
    fn service_call_inbox(&mut self) -> Result<(), String> {
        if self
            .connection
            .as_ref()
            .is_none_or(|c| !c.chat.supports("e2ee-call-freshness-v1"))
        {
            return Ok(());
        }
        let device_id = self.crypto()?.device.identity().device_id.clone();
        let hints: Vec<CallKeyTarget> = read_json(
            checked(
                self.request(reqwest::Method::GET, "/api/e2ee/call-inbox")?
                    .query(&[("device_id", device_id)])
                    .timeout(Duration::from_secs(3))
                    .send()
                    .map_err(network_error)?,
            )?,
            256 * 1024,
        )?;
        for target in self.crypto()?.call_inbox_targets(&hints)? {
            let peer = target
                .peer_id
                .ok_or("The private call-key target is missing.")?;
            self.ensure_crypto_conversation(&Target::Direct(peer))?;
        }
        Ok(())
    }
    pub(super) fn encrypt_chat(
        &mut self,
        target: &Target,
        content: String,
        attachment_ids: &[String],
        client_id: &str,
        reply_to_id: Option<String>,
        mentions: Vec<String>,
    ) -> Result<String, String> {
        self.ensure_crypto_conversation(target)?;
        let scope = self.crypto_scope(target)?;
        let crypto = self.crypto()?;
        let attachments = attachment_ids
            .iter()
            .map(|id| {
                crypto
                    .file(id)?
                    .ok_or("The encrypted file key is missing. Upload the file again.".to_string())
            })
            .collect::<Result<Vec<_>, _>>()?;
        crypto.encrypted_content(Envelope {
            version: 1,
            scope,
            client_id: client_id.into(),
            content,
            reply_to_id,
            mentions,
            attachments,
            action: None,
        })
    }
    pub(super) fn encrypt_action(
        &mut self,
        target: &Target,
        client_id: &str,
        action: crate::e2ee::MessageAction,
    ) -> Result<String, String> {
        self.ensure_crypto_conversation(target)?;
        let scope = self.crypto_scope(target)?;
        self.crypto()?.encrypted_content(Envelope {
            version: 1,
            scope,
            client_id: client_id.into(),
            content: String::new(),
            reply_to_id: None,
            mentions: Vec::new(),
            attachments: Vec::new(),
            action: Some(action),
        })
    }
    pub(super) fn decrypt_messages(&mut self, target: &Target, messages: &mut Vec<Message>) {
        let raw: Vec<_> = messages
            .iter()
            .filter(|message| !message.content.starts_with(crate::e2ee::MESSAGE_PREFIX))
            .take(64)
            .cloned()
            .collect();
        let Some(c) = self.connection.as_mut() else {
            return;
        };
        let origin = CommandOrigin::from(&*c);
        let protections = decrypt_in_connection(c, target, messages);
        let reactions = reaction_updates(
            c,
            target,
            &protections
                .iter()
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>(),
        );
        let delivered: Vec<_> = protections.iter().map(|(id, _)| id.clone()).collect();
        self.emit(Update::MessageProtections {
            origin,
            protections,
        });
        for update in reactions {
            self.emit(update);
        }
        self.schedule_legacy_reactions(target, &delivered, &raw);
    }
    pub(super) fn decrypt_changes(&mut self, changes: &mut Vec<MessageChange>) {
        let Some(c) = self.connection.as_mut() else {
            return;
        };
        let origin = CommandOrigin::from(&*c);
        changes.retain(change_matches_message);
        let mut targets: HashMap<Target, Vec<String>> = HashMap::new();
        let mut raw: HashMap<Target, Vec<Message>> = HashMap::new();
        for change in changes.iter() {
            let target = match &change.channel_id {
                Some(id) => Target::Channel(id.clone()),
                None if change.user_id == c.session.user.id => {
                    Target::Direct(change.recipient_id.clone().unwrap_or_default())
                }
                None => Target::Direct(change.user_id.clone()),
            };
            if let Some(message) = change
                .message
                .as_ref()
                .filter(|message| !message.content.starts_with(crate::e2ee::MESSAGE_PREFIX))
            {
                let originals = raw.entry(target.clone()).or_default();
                if originals.len() < 64 {
                    originals.push(message.clone());
                }
            }
            targets
                .entry(target)
                .or_default()
                .push(change.message_id.clone());
        }
        let protections = decrypt_changes_in_connection(c, changes);
        let reactions: Vec<_> = targets
            .iter()
            .flat_map(|(target, ids)| reaction_updates(c, target, ids))
            .collect();
        self.emit(Update::MessageProtections {
            origin,
            protections,
        });
        for update in reactions {
            self.emit(update);
        }
        for (target, ids) in targets {
            self.schedule_legacy_reactions(
                &target,
                &ids,
                raw.get(&target).map(Vec::as_slice).unwrap_or(&[]),
            );
        }
    }
}
// The delivery service knows operation IDs. A receipt must echo the actual
// durable batch, not merely an ID or a newer advertised head. Invisible
// Welcomes are optional in the response; their known slots still belong to the
// atomic batch. Never skip a later operation when advancing the local cursor.
fn control_ack_sequence(
    batch: &AppendCryptoEvents,
    context: &CryptoContext,
    own: &Identity,
) -> Result<u64, String> {
    let invalid = || {
        "The group-key update acknowledgement does not match the protected operation. Its exact pending update was preserved for retry.".to_string()
    };
    let end = batch
        .expected_sequence
        .checked_add(batch.events.len() as u64)
        .filter(|end| *end <= i64::MAX as u64)
        .ok_or_else(invalid)?;
    if batch.events.is_empty()
        || batch.events.len() > 66
        || context.server_id != own.server_id
        || batch.device_id != own.device_id
        || context.target != batch.target
        || context.group_id.as_deref() != Some(batch.group_id.as_str())
        || context.sequence < end
        || context.sequence > i64::MAX as u64
        || context.events.len() > 128
        || context
            .events
            .windows(2)
            .any(|pair| pair[0].sequence >= pair[1].sequence)
        || context
            .events
            .iter()
            .any(|event| event.sequence == 0 || event.sequence > context.sequence)
    {
        return Err(invalid());
    }
    let mut seen = vec![false; batch.events.len()];
    for event in &context.events {
        let in_batch = event.sequence > batch.expected_sequence && event.sequence <= end;
        if event.operation_id != batch.operation_id {
            if in_batch {
                return Err(invalid());
            }
            continue;
        }
        if !in_batch {
            return Err(invalid());
        }
        let index = (event.sequence - batch.expected_sequence - 1) as usize;
        let expected = &batch.events[index];
        if event.group_id != batch.group_id
            || event.epoch != batch.epoch
            || event.author_user_id != own.user_id
            || event.author_device_id != own.device_id
            || event.kind != expected.kind
            || event.recipient_device_id != expected.recipient_device_id
            || event.body != expected.body
            || seen[index]
        {
            return Err(invalid());
        }
        seen[index] = true;
    }
    if batch.events.iter().enumerate().any(|(n, event)| {
        (event.recipient_device_id.is_none()
            || event.recipient_device_id.as_deref() == Some(own.device_id.as_str()))
            && !seen[n]
    }) {
        return Err(invalid());
    }
    Ok(end)
}
fn reaction_updates(c: &Connection, target: &Target, delivered: &[String]) -> Vec<Update> {
    let Some(crypto) = &c.crypto else {
        return Vec::new();
    };
    if crypto.device.identity().server_id != c.server_id
        || crypto.device.identity().user_id != c.session.user.id
    {
        return Vec::new();
    }
    let scope = scope_for(&c.server_id, &c.session.user.id, target);
    let delivered: std::collections::HashSet<_> = delivered.iter().collect();
    let mut changed = std::collections::HashSet::new();
    for entry in crypto.cached_envelopes(&scope) {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => return vec![Update::Error(error)],
        };
        if entry
            .message_id
            .as_ref()
            .is_some_and(|id| delivered.contains(id))
        {
            if let Some(crate::e2ee::MessageAction::Reaction { message_id, .. }) =
                entry.envelope.action
            {
                changed.insert(message_id);
            }
        }
    }
    if changed.is_empty() {
        return Vec::new();
    }
    let mut originals = HashMap::new();
    // Select affected originals while the projection validates the same
    // streaming history. No second DPAPI/JSON pass or new plaintext cache;
    // a damaged segment still discards the entire result before emission.
    let projection = match super::reactions::ReactionProjection::from_history(
        &scope,
        crypto.cached_envelopes(&scope).inspect(|entry| {
            let Ok(cached) = entry else {
                return;
            };
            if cached.envelope.action.is_some() {
                return;
            }
            let Some(id) = cached.message_id.as_ref() else {
                return;
            };
            if changed.contains(id) {
                originals
                    .entry((id.clone(), cached.envelope.client_id.clone()))
                    .or_insert_with(|| Message {
                        id: id.clone(),
                        client_id: Some(cached.envelope.client_id.clone()),
                        author: User {
                            id: cached.author.user_id.clone(),
                            ..Default::default()
                        },
                        ..Default::default()
                    });
            }
        }),
    ) {
        Ok(projection) => projection,
        Err(error) => return vec![Update::Error(error)],
    };
    let mut originals: Vec<_> = originals.into_values().collect();
    projection.apply(&mut originals);
    originals
        .into_iter()
        .map(|message| Update::ReactionSnapshot {
            url: c.url.clone(),
            target: target.clone(),
            message_id: message.id,
            target_client_id: message.client_id,
            rows: message.reactions,
        })
        .collect()
}
fn change_matches_message(change: &MessageChange) -> bool {
    // UI routing uses the outer change; decryption uses the embedded message.
    // Reject inconsistent wrappers before ratcheting, caching or projecting any
    // reaction. Host-only deletions/availability have no embedded application.
    change.message.as_ref().is_none_or(|message| {
        change.message_id == message.id
            && change.channel_id == message.channel_id
            && change.recipient_id == message.recipient_id
            && change.user_id == message.author.id
    })
}

pub(super) fn decrypt_changes_in_connection(
    c: &mut Connection,
    changes: &mut Vec<MessageChange>,
) -> Vec<(String, MessageProtection)> {
    changes.retain(change_matches_message);
    let mut grouped: HashMap<Target, Vec<Message>> = HashMap::new();
    for change in changes.iter() {
        if let Some(message) = &change.message {
            let target = match &message.channel_id {
                Some(id) => Target::Channel(id.clone()),
                None if message.author.id == c.session.user.id => {
                    Target::Direct(message.recipient_id.clone().unwrap_or_default())
                }
                None => Target::Direct(message.author.id.clone()),
            };
            grouped.entry(target).or_default().push(message.clone());
        }
    }
    let mut decoded = HashMap::new();
    let mut hidden = std::collections::HashSet::new();
    let mut protections = Vec::new();
    for (target, mut messages) in grouped {
        let original: Vec<_> = messages.iter().map(|m| m.id.clone()).collect();
        protections.extend(decrypt_in_connection(c, &target, &mut messages));
        for id in original {
            if !messages.iter().any(|m| m.id == id) {
                hidden.insert(id);
            }
        }
        decoded.extend(messages.into_iter().map(|m| (m.id.clone(), m)));
    }
    changes.retain_mut(|change| {
        if hidden.contains(&change.message_id) {
            return false;
        }
        if change.message.is_some() {
            change.message = decoded.get(&change.message_id).cloned();
        }
        true
    });
    protections
}
pub(super) fn reconcile_read_states(c: &Connection, states: &mut [ReadState]) {
    let Some(crypto) = &c.crypto else {
        return;
    };
    if crypto.device.identity().server_id != c.server_id
        || crypto.device.identity().user_id != c.session.user.id
    {
        return;
    }
    for state in states {
        let target = match (&state.channel_id, &state.peer_id) {
            (Some(id), None) => Target::Channel(id.clone()),
            (None, Some(id)) => Target::Direct(id.clone()),
            _ => continue,
        };
        let scope = scope_for(&c.server_id, &c.session.user.id, &target);
        let result = history_unread_counts(
            crypto.cached_envelopes(&scope),
            &c.session.user.id,
            state.last_read_sequence,
        );
        let Ok((actions, mentions)) = result else {
            // Damaged local history cannot supply trusted mention projections.
            state.mention_count = 0;
            continue;
        };
        state.unread_count = state.unread_count.saturating_sub(actions);
        state.mention_count = state.mention_count.saturating_add(mentions);
    }
}
#[cfg(test)]
fn authenticated_unread_counts<'a>(
    cached: impl IntoIterator<Item = &'a crate::e2ee::CachedEnvelope>,
    own_user: &str,
    last_read_sequence: u64,
) -> (u64, u64) {
    history_unread_counts(
        cached.into_iter().cloned().map(Ok),
        own_user,
        last_read_sequence,
    )
    .unwrap()
}
fn history_unread_counts(
    cached: impl IntoIterator<Item = Result<crate::e2ee::CachedEnvelope, String>>,
    own_user: &str,
    last_read_sequence: u64,
) -> Result<(u64, u64), String> {
    let mut mentions = std::collections::HashSet::new();
    let mut actions = std::collections::HashSet::new();
    // Edits retain the original delivery sequence and client ID. Count the
    // latest authenticated version observed locally, rather than every
    // retained ciphertext. Cached re-reads preserve their first-delivery
    // revision; this does not prove ordering against a malicious host.
    type DeliveryIdentity = (String, String, String, String);
    type VersionSummary = (u64, bool, bool);
    let mut versions: HashMap<DeliveryIdentity, VersionSummary> = HashMap::new();
    for cached in cached {
        let cached = cached?;
        if cached.sequence <= last_read_sequence || cached.author.user_id == own_user {
            continue;
        }
        let Some(id) = &cached.message_id else {
            continue;
        };
        let key = (
            cached.author.user_id.clone(),
            cached.author.device_id.clone(),
            cached.envelope.client_id.clone(),
            id.clone(),
        );
        let summary = (
            cached.delivery_revision,
            cached.envelope.action.is_some(),
            cached.envelope.mentions.iter().any(|id| id == own_user),
        );
        match versions.get_mut(&key) {
            Some(version) if cached.delivery_revision > version.0 => *version = summary,
            Some(_) => {}
            None => {
                versions.insert(key, summary);
            }
        };
    }
    for ((_, _, _, id), (_, action, mention)) in versions {
        if action {
            actions.insert(id);
        } else if mention {
            mentions.insert(id);
        }
    }
    Ok((actions.len() as u64, mentions.len() as u64))
}

pub(super) fn decrypt_in_connection(
    c: &mut Connection,
    target: &Target,
    messages: &mut Vec<Message>,
) -> Vec<(String, MessageProtection)> {
    let scope = scope_for(&c.server_id, &c.session.user.id, target);
    let mut protections = Vec::new();
    let mut hidden = std::collections::HashSet::new();
    let identity_matches = c.crypto.as_ref().is_some_and(|crypto| {
        crypto.device.identity().server_id == c.server_id
            && crypto.device.identity().user_id == c.session.user.id
    });
    // Consume application ratchets chronologically, regardless of HTTP page order.
    let mut order: Vec<_> = (0..messages.len()).collect();
    order.sort_by_key(|index| messages[*index].sequence);
    let mut decoded: HashMap<usize, Result<crate::e2ee::CachedEnvelope, String>> = HashMap::new();
    for chunk in order.chunks(200) {
        let encrypted: Vec<_> = chunk
            .iter()
            .copied()
            .filter(|index| {
                messages[*index]
                    .content
                    .starts_with(crate::e2ee::MESSAGE_PREFIX)
            })
            .collect();
        let wires: Vec<_> = encrypted
            .iter()
            .map(|index| messages[*index].content.as_str())
            .collect();
        let results = if let Some(crypto) = c.crypto.as_mut().filter(|_| identity_matches) {
            crypto.decrypt_contents(&scope, &wires)
        } else {
            vec![
                Err("Encryption keys are not ready for this account and community.".into());
                encrypted.len()
            ]
        };
        for (index, result) in encrypted.iter().copied().zip(results) {
            let message = &messages[index];
            let result = result.and_then(|cached| {
                let routing_matches = match target {
                    Target::Channel(id) => {
                        message.channel_id.as_ref() == Some(id) && message.recipient_id.is_none()
                    }
                    Target::Direct(id) => {
                        message.channel_id.is_none()
                            && ((message.author.id == c.session.user.id
                                && message.recipient_id.as_ref() == Some(id))
                                || (message.author.id == *id
                                    && message.recipient_id.as_ref() == Some(&c.session.user.id)))
                    }
                };
                if !routing_matches
                    || cached.author.user_id != message.author.id
                    || message.client_id.as_deref() != Some(cached.envelope.client_id.as_str())
                    || message.attachments.len() != cached.envelope.attachments.len()
                    || !cached
                        .envelope
                        .attachments
                        .iter()
                        .all(|a| message.attachments.iter().any(|outer| outer.id == a.id))
                {
                    return Err(
                        "The encrypted author, message ID or attachment context does not match."
                            .into(),
                    );
                }
                Ok(cached)
            });
            decoded.insert(index, result);
        }
        let bindings: Vec<_> = encrypted
            .into_iter()
            .filter(|index| decoded[index].is_ok())
            .collect();
        let deliveries: Vec<_> = bindings
            .iter()
            .map(|index| {
                let message = &messages[*index];
                (
                    message.content.as_str(),
                    message.id.as_str(),
                    message.sequence,
                )
            })
            .collect();
        if let Some(crypto) = c.crypto.as_mut().filter(|_| identity_matches) {
            for (index, result) in bindings
                .into_iter()
                .zip(crypto.bind_deliveries(&deliveries))
            {
                if let Err(error) = result {
                    decoded.insert(index, Err(error));
                }
            }
        }
    }
    for index in order {
        let message = &mut messages[index];
        let protection = if message.content.starts_with(crate::e2ee::MESSAGE_PREFIX) {
            let result = decoded
                .remove(&index)
                .unwrap_or_else(|| Err("The encrypted message was not processed.".into()));
            match result {
                Ok(cached) => {
                    if cached.envelope.action.is_some() {
                        hidden.insert(message.id.clone());
                    }
                    message.content = cached.envelope.content;
                    message.reply_to_id = cached.envelope.reply_to_id;
                    message.mentions = cached.envelope.mentions;
                    message.reactions.clear();
                    for attachment in &mut message.attachments {
                        let file = cached
                            .envelope
                            .attachments
                            .iter()
                            .find(|a| a.id == attachment.id)
                            .unwrap();
                        attachment.name = file.name.clone();
                        attachment.mime = file.mime.clone();
                        attachment.size = file.file.size;
                    }
                    MessageProtection::Protected
                }
                Err(reason) => {
                    message.content = "Encrypted message unavailable".into();
                    message.attachments.clear();
                    message.mentions.clear();
                    message.reply_to_id = None;
                    message.reactions.clear();
                    MessageProtection::Unavailable(reason)
                }
            }
        } else {
            MessageProtection::Legacy
        };
        protections.push((message.id.clone(), protection));
    }
    messages.retain(|message| !hidden.contains(&message.id));
    if let Some(crypto) = c.crypto.as_ref().filter(|_| identity_matches) {
        match super::reactions::ReactionProjection::from_history(
            &scope,
            crypto.cached_envelopes(&scope),
        ) {
            Ok(projection) => projection.apply(messages),
            Err(error) => {
                for message in messages {
                    message.reactions.clear();
                    if let Some((_, protection)) =
                        protections.iter_mut().find(|(id, _)| id == &message.id)
                    {
                        if matches!(protection, MessageProtection::Protected) {
                            message.content = "Encrypted message unavailable".into();
                            message.attachments.clear();
                            message.mentions.clear();
                            message.reply_to_id = None;
                            *protection = MessageProtection::Unavailable(error.clone());
                        }
                    }
                }
            }
        }
    }
    protections
}

#[cfg(test)]
mod unread_version_tests {
    use super::*;

    fn version(revision: u64, mentions: Vec<String>) -> crate::e2ee::CachedEnvelope {
        crate::e2ee::CachedEnvelope {
            author: Identity {
                server_id: "server".into(),
                user_id: "peer".into(),
                device_id: "peer-device".into(),
            },
            envelope: Envelope {
                version: 1,
                scope: "scope".into(),
                client_id: "original-send".into(),
                content: "private body".into(),
                reply_to_id: None,
                mentions,
                attachments: Vec::new(),
                action: None,
            },
            message_id: Some("original-message".into()),
            sequence: 7,
            delivery_revision: revision,
        }
    }

    #[test]
    fn newer_authenticated_edit_removes_old_mentions_regardless_of_cached_iteration_order() {
        let original = version(11, vec!["own".into()]);
        let edited = version(17, Vec::new());
        assert_eq!(authenticated_unread_counts([&original], "own", 0), (0, 1));
        assert_eq!(
            authenticated_unread_counts([&original, &edited], "own", 0),
            (0, 0)
        );
        assert_eq!(
            authenticated_unread_counts([&edited, &original, &original], "own", 0),
            (0, 0)
        );
        let later = version(23, vec!["own".into()]);
        assert_eq!(
            authenticated_unread_counts([&edited, &later, &original], "own", 0),
            (0, 1)
        );
        assert_eq!(
            authenticated_unread_counts([&original, &edited, &later], "own", 7),
            (0, 0)
        );
        assert_eq!(
            authenticated_unread_counts([&original, &edited, &later], "peer", 0),
            (0, 0)
        );
    }
}
