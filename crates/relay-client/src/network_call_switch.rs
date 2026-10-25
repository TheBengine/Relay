//! User-confirmed call handoff. Destination admission precedes exact source release.
use super::*;

#[derive(Clone)]
pub(crate) struct Request {
    pub origin: CommandOrigin,
    pub call_id: String,
    pub previous: CommandOrigin,
    pub previous_channel: String,
    pub previous_attempt: u64,
    pub media_generation: u64,
    pub operation: u64,
    pub voice_attempt: u64,
    pub state: VoiceState,
}
pub(crate) struct Ready {
    pub request: Request,
    pub session: Session,
    pub bootstrap: Bootstrap,
    pub admission: PrivateCallAdmission,
    pub keys: Arc<crate::e2ee::CallKeys>,
    pub warning: Option<String>,
    pub media_generation: u64,
    pub gateway: Option<crate::gateway_control::Config>,
}
pub(crate) struct Failure {
    pub message: String,
    pub previous_kept: bool,
    pub media_generation: u64,
}
fn target<'a>(
    origin: &CommandOrigin,
    call: &str,
    b: &'a Bootstrap,
) -> Result<&'a PrivateCall, String> {
    validate_bootstrap_identity(&origin.server_id, &origin.user_id, b)?;
    if !b.capabilities.iter().any(|c| c == "private-call-switch-v1")
        || uuid::Uuid::parse_str(call).is_err()
    {
        return Err("Update this Relay server before using Leave and answer.".into());
    }
    let at = if b.social.server_time != 0 {
        b.social.server_time
    } else {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    };
    b.social
        .calls
        .iter()
        .find(|c| {
            c.id == call
                && c.recipient_id == origin.user_id
                && c.caller_id != origin.user_id
                && matches!(c.state.as_str(), "ringing" | "accepted")
                && (c.state == "accepted" || c.expires_at > at)
                && b.social.friends.contains(&c.caller_id)
                && !b.social.blocked.contains(&c.caller_id)
                && b.members.iter().any(|u| u.id == c.caller_id)
                && b.channels
                    .iter()
                    .any(|room| room.id == c.channel_id && room.kind == "voice" && room.can_join)
        })
        .ok_or_else(|| "This invitation is no longer available.".into())
}
impl Worker {
    fn switch_bootstrap(&self) -> Result<Bootstrap, String> {
        parse(
            self.request(reqwest::Method::GET, "/api/bootstrap")?
                .timeout(Duration::from_secs(2))
                .send()
                .map_err(network_error)?,
        )
    }
    fn switch_source(&self, r: &Request) -> Result<u64, String> {
        let c = self.command_connection(&r.previous)?;
        if c.voice_channel.as_ref() != Some(&r.previous_channel)
            || c.voice_attempt != Some(r.previous_attempt)
            || self.live.epoch.load(Ordering::Acquire) != r.media_generation
            || self.live.shutdown.load(Ordering::Acquire)
        {
            return Err("Your current call changed. Confirm the invitation again.".into());
        }
        c.voice_admission
            .as_ref()
            .map(|a| a.session)
            .ok_or_else(|| "Reconnect your current call before using Leave and answer.".into())
    }
    pub(super) fn release_switched_call(
        &self,
        origin: &CommandOrigin,
        instance: &str,
        session: u64,
        channel: &str,
        device: &str,
        open_revision: Option<u64>,
    ) -> Result<(), String> {
        let c = self
            .connection
            .as_ref()
            .filter(|c| CommandOrigin::from(*c) == *origin)
            .or_else(|| {
                self.saved_connections
                    .get(&origin.url)
                    .map(|s| &s.connection)
                    .filter(|c| CommandOrigin::from(*c) == *origin)
            })
            .ok_or("This account is no longer signed in.")?;
        checked(
            self.http
                .post(crate::embedded_network::endpoint(&format!(
                    "{}/api/voice/leave-exact",
                    origin.url
                )))
                .bearer_auth(&c.session.token)
                .json(&VoiceLeaveExact {
                    instance_id: instance.into(),
                    session_id: session,
                    channel_id: channel.into(),
                    device_id: device.into(),
                    open_revision,
                })
                .timeout(Duration::from_secs(2))
                .send()
                .map_err(network_error)?,
        )?;
        Ok(())
    }
    pub(super) fn leave_and_answer(&mut self, r: &Request) -> Result<Ready, Failure> {
        let kept = |message| Failure {
            message,
            previous_kept: true,
            media_generation: r.media_generation,
        };
        let old_session = self.switch_source(r).map_err(kept)?;
        let source = self.switch_bootstrap().map_err(kept)?;
        let source_device = self
            .connection
            .as_ref()
            .and_then(Connection::voice_device_id)
            .ok_or_else(|| {
                kept(
                    "The current call's exact device is unavailable. Reconnect before answering."
                        .into(),
                )
            })?;
        let source_revision = self
            .connection
            .as_ref()
            .and_then(|c| c.chat.open_revision());
        validate_bootstrap_identity(&r.previous.server_id, &r.previous.user_id, &source)
            .map_err(kept)?;
        if !source
            .capabilities
            .iter()
            .any(|c| c == "private-call-switch-v1")
            || !source.voice_members.iter().any(|v| {
                v.session_id == old_session
                    && v.user.id == r.previous.user_id
                    && v.channel_id == r.previous_channel
                    && v.device_id.as_deref() == Some(source_device.as_str())
            })
        {
            return Err(kept(
                "Your current admission changed, or its server needs an update. No call was left."
                    .into(),
            ));
        }
        let old_instance = source.instance_id.clone();
        if self
            .connection
            .as_ref()
            .and_then(|c| c.voice_admission.as_ref())
            .is_none_or(|a| a.instance != old_instance)
        {
            return Err(kept(
                "The server restarted. Let the current call reconnect before answering.".into(),
            ));
        }
        let cross = r.origin != r.previous;
        let destination = if cross {
            let saved = self.saved_connections.get(&r.origin.url)
                .filter(|s| CommandOrigin::from(&s.connection) == r.origin)
                .ok_or_else(|| kept(
                    "Sign in to the invitation's account before answering. Your current call was kept.".into()
                ))?;
            if saved.connection.voice_channel.is_some() || saved.connection.voice_recovery.is_some()
            {
                return Err(kept("Reconnect or leave the destination account's current call before answering this invitation.".into()));
            }
            let b: Bootstrap = parse(
                self.http
                    .get(crate::embedded_network::endpoint(&format!(
                        "{}/api/bootstrap",
                        r.origin.url
                    )))
                    .bearer_auth(&saved.connection.session.token)
                    .timeout(Duration::from_secs(2))
                    .send()
                    .map_err(network_error)
                    .map_err(kept)?,
            )
            .map_err(kept)?;
            b
        } else {
            source
        };
        let call = target(&r.origin, &r.call_id, &destination)
            .map_err(kept)?
            .clone();
        self.switch_source(r).map_err(kept)?;
        let (previous, mut parked) = if cross {
            let saved = self
                .saved_connections
                .remove(&r.origin.url)
                .expect("destination checked");
            (
                self.connection.replace(saved.connection),
                Some((saved.stop, saved.dirty)),
            )
        } else {
            (None, None)
        };
        // The temporary destination owns only its existing crypto/session data.
        self.call_switch_preflight = cross;
        self.connection
            .as_mut()
            .unwrap()
            .chat
            .update_bootstrap(&destination);
        let mut committed = None;
        let mut uncertain = false;
        let handoff_generation = r.media_generation.wrapping_add(1);
        let prepared =
            (|| -> Result<(PrivateCallAdmission, Arc<crate::e2ee::CallKeys>, Bootstrap), String> {
                if self.connection.as_ref().is_some_and(|c| c.chat.is_open()) {
                    if !self
                        .connection
                        .as_ref()
                        .is_some_and(|c| c.chat.plain_allowed())
                    {
                        return Err("Review the destination server's encryption choice before answering. Your current call was kept.".into());
                    }
                } else {
                    self.ensure_crypto()?;
                    // Fresh device/roster keys must be available before replacing a seat.
                    self.protected_call_keys(&call.channel_id)?;
                }
                if self.live.epoch.load(Ordering::Acquire) != r.media_generation {
                    return Err("Your current call changed before confirmation.".into());
                }
                let c = self.connection.as_ref().unwrap();
                let input = PrivateCallJoin {
                    instance_id: destination.instance_id.clone(),
                    join: VoiceJoin {
                        channel_id: call.channel_id.clone(),
                        device_id: c.voice_device_id(),
                        open_revision: c.chat.open_revision(),
                    },
                    replace_session_id: (!cross).then_some(old_session),
                    state: r.state.clone(),
                };
                let mut answer = None;
                // Lost replies retry only this exact call. The server recognizes its
                // already admitted seat and never creates a different call identity.
                for _ in 0..2 {
                    let response = self
                        .request(
                            reqwest::Method::POST,
                            &format!("/api/private-calls/{}/join", r.call_id),
                        )?
                        .json(&input)
                        .timeout(Duration::from_secs(2))
                        .send();
                    let outcome = match response {
                        Ok(response) if !response.status().is_success() => {
                            return Err(parse::<PrivateCallAdmission>(response)
                                .err()
                                .unwrap_or_else(|| "The call was not admitted.".into()))
                        }
                        Ok(response) => {
                            uncertain = true;
                            parse::<PrivateCallAdmission>(response)
                        }
                        Err(error) => {
                            uncertain = true;
                            Err(network_error(error))
                        }
                    };
                    match outcome {
                        Ok(value) => {
                            answer = Some(value);
                            break;
                        }
                        Err(error) => {
                            if self.live.epoch.load(Ordering::Acquire) != r.media_generation {
                                return Err(error);
                            }
                            let b: Bootstrap = self.switch_bootstrap()?;
                            validate_bootstrap_identity(
                                &r.origin.server_id,
                                &r.origin.user_id,
                                &b,
                            )?;
                            if !cross
                                && b.voice_members.iter().any(|v| {
                                    v.user.id == r.previous.user_id
                                        && v.session_id == old_session
                                        && v.channel_id == r.previous_channel
                                })
                            {
                                uncertain = false;
                            }
                            target(&r.origin, &r.call_id, &b)?;
                            if !b
                                .social
                                .calls
                                .iter()
                                .any(|v| v.id == r.call_id && v.state == "accepted")
                            {
                                return Err(error);
                            }
                        }
                    }
                }
                let answer = answer.ok_or(
                    "The server did not confirm the new call. Check its status before trying again."
                )?;
                if answer.call.id != call.id
                    || answer.call.channel_id != call.channel_id
                    || answer.call.caller_id != call.caller_id
                    || answer.call.recipient_id != call.recipient_id
                    || answer.call.state != "accepted"
                    || answer.ticket.session_id == 0
                    || answer.ticket.resume.as_ref().is_none_or(|resume| {
                        resume.instance_id != destination.instance_id
                    })
                {
                    return Err("The admission did not match this exact invitation.".into());
                }
                uncertain = false;
                committed = Some((
                    answer.ticket.session_id,
                    answer.call.channel_id.clone(),
                    answer.ticket.resume.as_ref().unwrap().instance_id.clone(),
                    input.join.device_id.clone().ok_or("The selected call device is unavailable")?,
                    input.join.open_revision,
                ));
                let keys = self.protected_call_keys(&call.channel_id)?;
                crate::media::validate_call_keys(&keys)?;
                if self.live.epoch.load(Ordering::Acquire) != r.media_generation
                    || self.live.shutdown.load(Ordering::Acquire)
                {
                    return Err("The call change was cancelled before media started.".into());
                }
                let latest: Bootstrap = self.switch_bootstrap()?;
                validate_bootstrap_identity(&r.origin.server_id, &r.origin.user_id, &latest)?;
                let current = target(&r.origin, &r.call_id, &latest)?;
                if current.state != "accepted" || current.channel_id != answer.call.channel_id
                    || latest.protection.level != destination.protection.level
                    || latest.protection.revision != destination.protection.revision
                    || !latest.voice_members.iter().any(|v| {
                        v.user.id == r.origin.user_id && v.session_id == answer.ticket.session_id
                            && v.channel_id == call.channel_id && v.device_id == input.join.device_id
                    })
                {
                    return Err("The accepted call changed before media could start.".into());
                }
                if self.live.epoch.load(Ordering::Acquire) != r.media_generation
                    || self.live.shutdown.load(Ordering::Acquire)
                {
                    return Err("The call change was cancelled before handoff.".into());
                }
                // This is the commit boundary for the local handoff. Keep this
                // generation fixed in the result; a later user stop/account
                // change must not be overwritten by another worker increment.
                self.live.epoch.compare_exchange(
                    r.media_generation, handoff_generation,
                    Ordering::AcqRel, Ordering::Acquire,
                ).map_err(|_| "The call change was cancelled before handoff.")?;
                Ok((answer, keys, latest))
            })();
        self.call_switch_preflight = false;
        match prepared {
            Err(mut message) => {
                if let Some((session, channel, instance, device, revision)) = &committed {
                    if self
                        .release_switched_call(
                            &r.origin, instance, *session, channel, device, *revision,
                        )
                        .is_err()
                    {
                        message.push_str(" Relay could not confirm releasing the new admission; its inactive connection will expire.");
                    }
                } else if cross && uncertain {
                    message.push_str(" The destination may have accepted this exact invitation; check its call status before retrying. Your previous call was kept.");
                }
                if let Some(old) = previous {
                    let (stop, dirty) = parked.take().unwrap();
                    self.saved_connections.insert(
                        r.origin.url.clone(),
                        SavedConnection {
                            connection: self.connection.take().unwrap(),
                            stop,
                            dirty,
                        },
                    );
                    self.connection = Some(old);
                } else if committed.is_some() || uncertain {
                    self.clear_voice_intent();
                }
                Err(Failure {
                    message: if !cross && uncertain {
                        format!("The server may have changed calls, but Relay could not confirm the result. Check the call status before retrying. {message}")
                    } else if !cross && committed.is_some() {
                        format!("The original call was replaced, but the new call could not start. Start a new call to retry. {message}")
                    } else {
                        message
                    },
                    previous_kept: cross || (committed.is_none() && !uncertain),
                    media_generation: self.live.epoch.load(Ordering::Acquire),
                })
            }
            Ok((mut admission, keys, bootstrap)) => {
                let mut warning = None;
                if let Some(mut old) = previous {
                    // The destination is now admitted and has validated keys.
                    // Cross-server cleanup is exact and best-effort, never an
                    // assertion of a distributed atomic transaction.
                    let response = self
                        .http
                        .post(crate::embedded_network::endpoint(&format!(
                            "{}/api/voice/leave-exact",
                            r.previous.url
                        )))
                        .bearer_auth(&old.session.token)
                        .json(&VoiceLeaveExact {
                            instance_id: old_instance,
                            session_id: old_session,
                            channel_id: r.previous_channel.clone(),
                            device_id: source_device,
                            open_revision: source_revision,
                        })
                        .timeout(Duration::from_secs(2))
                        .send();
                    if response.map_err(network_error).and_then(checked).is_err() {
                        warning=Some("The new call is ready. Relay could not confirm closing the previous call; its inactive connection will expire.".into());
                    }
                    old.voice_channel = None;
                    old.voice_attempt = None;
                    old.voice_admission = None;
                    old.voice_recovery = None;
                    old.crypto_pending_join = None;
                    old.crypto_call_epoch = None;
                    old.sharing = false;
                    old.camera = false;
                    old.frame_ids.clear();
                    self.saved_connections.insert(
                        r.previous.url.clone(),
                        SavedConnection {
                            connection: old,
                            stop: self.socket_stop.clone(),
                            dirty: self.refresh_pending.clone(),
                        },
                    );
                    let (stop, dirty) = parked.take().unwrap();
                    self.socket_stop = stop;
                    self.refresh_pending = dirty;
                }
                let c = self.connection.as_mut().unwrap();
                c.voice_channel = Some(call.channel_id.clone());
                c.voice_attempt = Some(r.voice_attempt);
                c.voice_recovery = None;
                c.crypto_pending_join = None;
                c.crypto_call_epoch = Some(keys.epoch);
                c.sharing = false;
                c.camera = false;
                c.frame_ids.clear();
                self.remember_voice_admission(&admission.ticket);
                // Retire only the old live transport here. UI confirmation
                // starts media and its normal VoiceState command installs the
                // new live context; no target media can precede that decision.
                *self.live.context.lock().unwrap_or_else(|e| e.into_inner()) = None;
                self.live.activity.notify_all();
                admission.ticket.tunnel_url = Some(format!(
                    "{}/api/voice/tunnel",
                    r.origin.url.trim_end_matches('/')
                ));
                let c = self.connection.as_ref().unwrap();
                let session = c.session.clone();
                if let Some(c) = self.connection.as_mut() {
                    c.chat.update_bootstrap(&bootstrap);
                }
                self.publish_communities();
                // Capture only the accepted destination account after the old call is
                // retired. The UI never builds gateway requests from its foreground URL.
                let gateway = self.voice_gateway(&admission.ticket);
                Ok(Ready {
                    gateway,
                    request: r.clone(),
                    session,
                    bootstrap,
                    admission,
                    keys,
                    warning,
                    media_generation: handoff_generation,
                })
            }
        }
    }
}
