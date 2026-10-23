//! Short-lived admission across a process restart. This grants no MLS trust or
//! media keys. Clients must confirm fresh protected keys before sending media.
use super::*;

const LEASE_SECONDS: u64 = 45;
const MAX_LEASES: u32 = 256;

pub(super) fn join(state: &AppState, auth: Auth, input: VoiceJoin) -> ApiResult<Json<VoiceTicket>> {
    admit(state, auth, input, None, None).map(|result| Json(result.ticket))
}

pub(super) async fn resume(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<VoiceResume>,
) -> ApiResult<Json<VoiceTicket>> {
    // Validate bounds before hashing or touching durable state.
    if input.resume_token.len() != 43
        || URL_SAFE_NO_PAD
            .decode(&input.resume_token)
            .ok()
            .is_none_or(|b| b.len() != 32)
        || input.device_id.is_empty()
        || input.device_id.len() > 128
        || input.channel_id.is_empty()
        || input.channel_id.len() > 128
    {
        return Err(forbidden());
    }
    admit(
        &state,
        state.auth(&headers)?,
        VoiceJoin {
            open_revision: input.open_revision,
            channel_id: input.channel_id,
            device_id: Some(input.device_id),
        },
        Some(&input.resume_token),
        None,
    )
    .map(|result| Json(result.ticket))
}

struct Admitted {
    ticket: VoiceTicket,
    call: Option<PrivateCall>,
}
pub(super) fn answer(
    state: &AppState,
    auth: Auth,
    call_id: &str,
    input: PrivateCallJoin,
) -> ApiResult<Json<PrivateCallAdmission>> {
    let result = admit(
        state,
        auth,
        input.join.clone(),
        None,
        Some((call_id, &input)),
    )?;
    Ok(Json(PrivateCallAdmission {
        call: result.call.expect("switch admission has an exact call"),
        ticket: result.ticket,
    }))
}

fn admit(
    state: &AppState,
    auth: Auth,
    input: VoiceJoin,
    resume_token: Option<&str>,
    switch: Option<(&str, &PrivateCallJoin)>,
) -> ApiResult<Admitted> {
    if switch.is_some_and(|(_, request)| request.instance_id != state.0.instance_id) {
        return Err(bad("The server restarted. Reconnect before answering this invitation."));
    }
    let mut db = lock(&state.0.db);
    private_calls::reconcile(state, &db)?;
    let tx = db.transaction()?;
    if get_channel(&tx, &input.channel_id)?.kind != "voice" {
        return Err(bad("Choose a voice channel"));
    }
    channel_access(&tx, &input.channel_id, &auth.user.id, JOIN)?;
    if (state.0.config.people || switch.is_some()) && input.device_id.is_none() {
        return Err(bad(
            "Private calls require your registered encryption device",
        ));
    }
    let protection = protection::current(&tx)?;
    if protection.level == ProtectionLevel::Open {
        if input.open_revision != Some(protection.revision)
            || input.device_id.as_deref()
                != Some(protection::open_device(&auth.token_hash).as_str())
        {
            return Err(bad(
                "Accept this server's current encryption choice before joining voice",
            ));
        }
    } else {
        if input.open_revision.is_some() {
            return Err(bad("The server's encryption choice changed"));
        }
        if let Some(device_id) = &input.device_id {
            e2ee::device(&tx, &auth.user.id, device_id)?;
        }
    }
    if tx.query_row(
        "SELECT COUNT(*) FROM sessions WHERE token_hash=?1 AND user_id=?2 AND expires>?3",
        params![auth.token_hash, auth.user.id, now()],
        |r| r.get::<_, u32>(0),
    )? != 1
    {
        return Err(unauthorized());
    }
    let permissions = voice::permissions(&tx, &input.channel_id, &auth.user.id)?;
    let switched_call = switch
        .map(|(id, _)| private_calls::prepare_switch(state, &tx, &auth, id, &input.channel_id))
        .transpose()?;
    let mut sessions = lock(&state.0.voice);
    let mut saved_state = VoiceState::default();
    if let Some((call_id, request)) = switch {
        let at = now();
        let own: Vec<_> = sessions
            .iter()
            .filter(|(_, v)| v.user.id == auth.user.id && v.alive_at(at))
            .collect();
        // Selected device must own the old seat, even when two devices share
        // a login token. Open mode's token-derived device and current revision
        // were checked above; changing protection also revokes all old seats.
        let exact = own.len() == 1
            && own.iter().any(|(id, v)| {
                Some(**id) == request.replace_session_id
                    && v.token_hash == auth.token_hash
                    && v.device_id == input.device_id
            });
        let retry = own.len() == 1
            && own.iter().any(|(_, v)| {
                v.private_call_id.as_deref() == Some(call_id)
                    && v.token_hash == auth.token_hash
                    && v.device_id == input.device_id
            });
        // A live recovery lease is still somebody's current admission. It must
        // resume through its capability before switching; expired retention is
        // neither authorization nor a reason to block a genuinely idle answer.
        let empty = own.is_empty()
            && request.replace_session_id.is_none()
            && !tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM voice_resume_leases WHERE user_id=?1 AND expires>?2)",
                params![auth.user.id, at],
                |r| r.get::<_, bool>(0),
            )?;
        if !exact && !retry && !empty {
            return Err(bad("Your current call changed. No call was replaced."));
        }
        // Accepted is idempotent only for this login's already admitted exact call.
        if switched_call
            .as_ref()
            .is_some_and(|c| c.state == "accepted")
            && lock(&state.0.private_calls)
                .get(call_id)
                .is_some_and(|c| c.state == "accepted")
            && !retry
        {
            return Err(bad(
                "This invitation was already answered. No call was replaced.",
            ));
        }
        saved_state = VoiceState {
            muted: request.state.muted,
            deafened: request.state.deafened,
            sharing: false,
            camera: false,
        };
    }
    let private_call_id;
    let mut saved_expiry = None;
    if let Some(token) = resume_token {
        let saved = tx.query_row(
            "SELECT muted,deafened,private_call_id,expires FROM voice_resume_leases WHERE user_id=?1 AND token_hash=?2 AND channel_id=?3 AND ((device_id=?4 AND open_revision IS NULL AND ?8 IS NULL) OR (device_id IS NULL AND open_revision=?8)) AND capability_hash=?5 AND instance_id!=?6 AND expires>?7",
            params![auth.user.id, auth.token_hash, input.channel_id, input.device_id, token_hash(token), state.0.instance_id, now(), input.open_revision],
            |r| Ok((VoiceState { muted:r.get(0)?, deafened:r.get(1)?, ..Default::default() }, r.get::<_,Option<String>>(2)?, r.get::<_,u64>(3)?)),
        ).optional()?.ok_or_else(forbidden)?;
        // Never replace a call on another device, or repair a kick/timeout in
        // the same running server. A lost resume response fails closed.
        if sessions.values().any(|v| v.user.id == auth.user.id) {
            return Err(forbidden());
        }
        saved_state = saved.0;
        private_call_id = saved.1;
        if let Some(id) = &private_call_id {
            private_calls::validate_binding(&tx, id, &auth.user.id, &input.channel_id)?;
            saved_expiry = Some(saved.2);
        }
    } else {
        private_call_id = if let Some(call) = &switched_call {
            Some(call.id.clone())
        } else {
            private_calls::admission_call(&tx, &auth.user.id, &input.channel_id)?
        };
    }
    if sessions.len() >= 256 && !sessions.values().any(|v| v.user.id == auth.user.id) {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "Voice capacity reached",
        ));
    }
    // Refuse before replacing seats or writing recovery state: one extra peer
    // must not make the whole room's bounded server-managed key roster invalid.
    // Rejoining your own seat and seats that have already expired do not count.
    if protection.level == ProtectionLevel::Open
        && sessions
            .values()
            .filter(|v| {
                v.channel_id == input.channel_id && v.user.id != auth.user.id && v.alive_at(now())
            })
            .count()
            >= protection::MAX_OPEN_CALL_PEERS
    {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "This voice room is full (32 people). Join another voice room.",
        ));
    }
    if !conversations::stage_speaker(&tx, &input.channel_id, &auth.user.id)? {
        saved_state.muted = true;
    }
    let mut key = [0; 32];
    OsRng.fill_bytes(&mut key);
    let session_id = loop {
        let value = OsRng.next_u64();
        if value != 0 && !sessions.contains_key(&value) {
            break value;
        }
    };
    // Replacing an admitted call with another room ends only the old exact ID.
    let old_ids: Vec<_> = sessions.values().filter(|v| v.user.id==auth.user.id)
        .filter_map(|v| v.private_call_id.as_ref()).filter(|id| Some(*id)!=private_call_id.as_ref()).cloned().collect();
    for id in old_ids { private_calls::persist_end(&tx, &id)?; }
    let old_lease_call: Option<String> = tx.query_row("SELECT private_call_id FROM voice_resume_leases WHERE user_id=?1", [&auth.user.id], |r| r.get(0)).optional()?.flatten();
    if let Some(old) = old_lease_call.filter(|old| Some(old)!=private_call_id.as_ref()) {
        private_calls::persist_end(&tx, &old)?;
    }
    tx.execute(
        "DELETE FROM voice_resume_leases WHERE expires<=?1 OR user_id=?2",
        params![now(), auth.user.id],
    )?;
    let resume = if let Some(device_id) = &input.device_id {
        if tx.query_row("SELECT COUNT(*) FROM voice_resume_leases", [], |r| {
            r.get::<_, u32>(0)
        })? >= MAX_LEASES
        {
            return Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "Voice recovery capacity reached",
            ));
        }
        let mut secret = [0; 32];
        OsRng.fill_bytes(&mut secret);
        let token = URL_SAFE_NO_PAD.encode(secret);
        tx.execute(
            "INSERT INTO voice_resume_leases(user_id,token_hash,channel_id,device_id,capability_hash,instance_id,expires,muted,deafened,open_revision,private_call_id) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![
                auth.user.id,
                auth.token_hash,
                input.channel_id,
                if input.open_revision.is_some() {
                    None
                } else {
                    Some(device_id)
                },
                token_hash(&token),
                state.0.instance_id,
                auth.expires.min(saved_expiry.unwrap_or(now() + LEASE_SECONDS)),
                saved_state.muted,
                saved_state.deafened,
                input.open_revision,
                private_call_id
            ],
        )?;
        Some(VoiceResumeCapability {
            token,
            instance_id: state.0.instance_id.clone(),
        })
    } else {
        None
    };
    // No runtime ticket is changed if durable admission fails. DB -> voice is
    // the same lock order used by revocation, state changes and bootstrap.
    let committed_calls = private_calls::durable(&tx)?;
    tx.commit()?;
    state.revoke_rtc(&auth.user.id, None);
    sessions.retain(|_, v| v.user.id != auth.user.id);
    lock(&state.0.frames).retain(|(owner, _), _| owner != &auth.user.id);
    sessions.insert(
        session_id,
        voice::VoiceSession {
            user: auth.user.clone(),
            channel_id: input.channel_id.clone(),
            device_id: input.device_id,
            token_hash: auth.token_hash,
            private_call_id,
            recovery_deadline: saved_expiry,
            key,
            state: saved_state.clone(),
            remote: None,
            tunnel_sender: None,
            tunnel_id: None,
            in_sequence: None,
            replay: Default::default(),
            out_sequence: 0,
            last_seen: now(),
            expires: auth.expires.min(now() + VOICE_LIFETIME),
            direct: Default::default(),
            can_speak: permissions.can_speak,
            can_stream: permissions.can_stream,
            whisper_channels: permissions.whisper_channels,
        },
    );
    drop(sessions);
    private_calls::committed(state, committed_calls);
    drop(db);
    private_calls::joined_other_room(state, &auth.user.id, &input.channel_id);
    state.notify("voice", Some(input.channel_id), Some(auth.user.id), None);
    Ok(Admitted {
        call: switched_call,
        ticket: VoiceTicket {
            resume,
            resumed_state: (resume_token.is_some() || switch.is_some()).then_some(saved_state),
            tunnel_url: None,
            session_id,
            key: STANDARD.encode(key),
            udp_port: state.0.udp_port,
            wire_version: 4,
        },
    })
}

pub(super) fn renew_lease(state: &AppState, db: &Connection, auth: &Auth) -> ApiResult<()> {
    let at = now();
    let mut sessions = lock(&state.0.voice);
    if let Some(session) = sessions.values_mut().find(|v| {
        v.user.id == auth.user.id
            && v.token_hash == auth.token_hash
            && v.alive_at(at)
            && channel_access(db, &v.channel_id, &auth.user.id, JOIN).is_ok()
    }) {
        if session.recovery_deadline.is_none() {
            db.execute(
            "UPDATE voice_resume_leases SET expires=?1 WHERE user_id=?2 AND token_hash=?3 AND instance_id=?4 AND expires>?5",
            params![auth.expires.min(at+LEASE_SECONDS),auth.user.id,auth.token_hash,state.0.instance_id,at],
            )?;
        }
        // A live, authenticated call can last beyond its initial four-hour window.
        // Keep its admission/key/counters and idle deadline; never revive an ended
        // ticket or extend it beyond the current login's expiry.
        session.expires = auth.expires.min(at + VOICE_LIFETIME);
    }
    Ok(())
}

pub(super) fn save_state(db: &Connection, auth: &Auth, state: &VoiceState) -> ApiResult<()> {
    db.execute(
        "UPDATE voice_resume_leases SET muted=?1,deafened=?2 WHERE user_id=?3 AND token_hash=?4",
        params![state.muted, state.deafened, auth.user.id, auth.token_hash],
    )?;
    Ok(())
}

pub(super) fn enforce_leases(db: &Connection) -> ApiResult<()> {
    // Include offline leases, not just live UDP sessions: permissions can be
    // removed then restored while the client is waiting for a restart.
    let leases = {
        let mut q = db.prepare("SELECT user_id,channel_id FROM voice_resume_leases")?;
        let rows = q
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        rows
    };
    for (user, channel) in leases {
        if channel_access(db, &channel, &user, JOIN).is_err() {
            db.execute("DELETE FROM voice_resume_leases WHERE user_id=?1", [&user])?;
        } else if !conversations::stage_speaker(db, &channel, &user)? {
            db.execute(
                "UPDATE voice_resume_leases SET muted=1 WHERE user_id=?1",
                [&user],
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "voice_recovery_tests.rs"]
mod tests;

/// Release only the authenticated exact seat named by a completed handoff.
/// A late release cannot end a successor on this or another login/device.
pub(super) async fn leave_exact(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<VoiceLeaveExact>,
) -> ApiResult<StatusCode> {
    let auth = state.auth(&headers)?;
    if input.instance_id != state.0.instance_id {
        return Ok(StatusCode::NO_CONTENT);
    }
    let db = lock(&state.0.db);
    let protection = protection::current(&db)?;
    if (protection.level == ProtectionLevel::Open
        && (input.open_revision != Some(protection.revision)
            || input.device_id != protection::open_device(&auth.token_hash)))
        || (protection.level == ProtectionLevel::Protected
            && (input.open_revision.is_some()
                || e2ee::device(&db, &auth.user.id, &input.device_id).is_err()))
    {
        return Ok(StatusCode::NO_CONTENT);
    }
    let old_call = {
        let seats = lock(&state.0.voice);
        let Some(seat) = seats.get(&input.session_id).filter(|v| {
            v.user.id == auth.user.id
                && v.token_hash == auth.token_hash
                && v.channel_id == input.channel_id
                && v.device_id.as_deref() == Some(input.device_id.as_str())
        }) else {
            return Ok(StatusCode::NO_CONTENT);
        };
        seat.private_call_id.clone()
    };
    let tx = db.unchecked_transaction()?;
    if let Some(id) = &old_call {
        private_calls::persist_end(&tx, id)?;
    }
    tx.execute("DELETE FROM voice_resume_leases WHERE user_id=?1 AND token_hash=?2 AND channel_id=?3 AND private_call_id IS ?4", params![auth.user.id,auth.token_hash,input.channel_id,old_call])?;
    let saved = private_calls::durable(&tx)?;
    tx.commit()?;
    private_calls::committed(&state, saved);
    lock(&state.0.voice).remove(&input.session_id);
    state.revoke_rtc(&auth.user.id, Some(&auth.token_hash));
    lock(&state.0.frames).retain(|(owner, _), _| owner != &auth.user.id);
    state.notify("voice", Some(input.channel_id), Some(auth.user.id), None);
    Ok(StatusCode::NO_CONTENT)
}
