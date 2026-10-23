//! Bounded opaque call control. Delivery and ACKs do not establish client trust.
use super::*;

fn scoped_group(
    state: &AppState,
    db: &Connection,
    user: &str,
    target: &CryptoTarget,
    device_id: &str,
) -> ApiResult<(String, u64)> {
    device(db, user, device_id)?;
    if let Some(channel) = &target.channel_id {
        if get_channel(db, channel)?.kind != "voice" {
            return Err(bad(
                "Call-key control requires a voice channel or private conversation",
            ));
        }
    }
    let (kind, id, _) = roster(state, db, user, target)?;
    if let Some(selected) = selected_voice_devices(state, db, &kind, &id)? {
        if selected
            .get(user)
            .is_none_or(|d| d.as_deref() != Some(device_id))
        {
            return Err(forbidden());
        }
    }
    db.query_row(
        "SELECT group_id,epoch FROM crypto_groups WHERE target_kind=?1 AND target_id=?2",
        params![kind, id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()?
    .ok_or_else(|| conflict("Prepare this encrypted conversation before requesting call keys"))
}

pub async fn send_call_request(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<CallKeyRequest>,
) -> ApiResult<Json<CallKeyDelivery>> {
    let auth = state.auth(&headers)?;
    if !identifier(&input.group_id)
        || !identifier(&input.operation_id)
        || input.epoch > i64::MAX as u64
    {
        return Err(bad("Invalid call-key request"));
    }
    decoded(&input.body, 8 * 1024)?;
    let json = serde_json::to_string(&input)?;
    let mut db = lock(&state.0.db);
    let tx = db.transaction()?;
    let (group_id, epoch) =
        scoped_group(&state, &tx, &auth.user.id, &input.target, &input.device_id)?;
    if group_id != input.group_id {
        return Err(conflict("The call-key request belongs to another group"));
    }
    let old: Option<(String,String,String,bool)> = tx.query_row(
        "SELECT author_user_id,operation_id,request_json,acknowledged FROM crypto_call_requests WHERE group_id=?1 AND author_device_id=?2",
        params![group_id,input.device_id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
    let retry = if let Some((user, operation, body, acknowledged)) = &old {
        if user != &auth.user.id {
            return Err(forbidden());
        }
        if operation == &input.operation_id {
            if body != &json {
                return Err(conflict(
                    "A saved call-key operation cannot change its body",
                ));
            }
            true
        } else {
            if !acknowledged {
                return Err(conflict(
                    "The previous call-key request is still waiting for its key manager",
                ));
            }
            false
        }
    } else {
        false
    };
    if !retry {
        if input.epoch > epoch || epoch.saturating_sub(input.epoch) > 4 {
            return Err(conflict("Refresh encryption before requesting call keys"));
        }
        let count: u32 = tx.query_row(
            "SELECT COUNT(*) FROM crypto_call_requests WHERE group_id=?1",
            [&group_id],
            |r| r.get(0),
        )?;
        let owned: u32 = tx.query_row(
            "SELECT COUNT(*) FROM crypto_call_requests WHERE author_user_id=?1",
            [&auth.user.id],
            |r| r.get(0),
        )?;
        if old.is_none() && (count >= MAX_ROSTER as u32 || owned >= 128) {
            return Err(conflict(
                "Call-key request storage reached its bounded limit",
            ));
        }
        let operation_owner: Option<String> = tx.query_row("SELECT author_device_id FROM crypto_call_requests WHERE group_id=?1 AND operation_id=?2", params![group_id,input.operation_id], |r| r.get(0)).optional()?;
        if operation_owner.is_some_and(|d| d != input.device_id) {
            return Err(conflict(
                "This call-key operation belongs to another device",
            ));
        }
        tx.execute("INSERT INTO crypto_call_requests(group_id,author_user_id,author_device_id,operation_id,request_json,acknowledged) VALUES(?1,?2,?3,?4,?5,0) ON CONFLICT(group_id,author_device_id) DO UPDATE SET operation_id=excluded.operation_id,request_json=excluded.request_json,acknowledged=0",
            params![group_id,auth.user.id,input.device_id,input.operation_id,json])?;
        check_quota(&state, &tx, &auth.user.id)?;
    }
    let delivery = CallKeyDelivery {
        server_id: server_id(&tx)?,
        target: input.target.clone(),
        author_user_id: auth.user.id,
        request: input,
    };
    tx.commit()?;
    // Polling is scoped and bounded. This never creates chat/unread events.
    Ok(Json(delivery))
}

pub async fn call_requests(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ContextQuery>,
) -> ApiResult<Json<Vec<CallKeyDelivery>>> {
    let auth = state.auth(&headers)?;
    let db = lock(&state.0.db);
    let target = CryptoTarget {
        channel_id: query.channel_id,
        peer_id: query.peer_id,
    };
    let (group_id, _) = scoped_group(&state, &db, &auth.user.id, &target, &query.device_id)?;
    let (kind, id, roster) = roster(&state, &db, &auth.user.id, &target)?;
    let selected = selected_voice_devices(&state, &db, &kind, &id)?;
    let mut statement = db.prepare("SELECT author_user_id,author_device_id,request_json FROM crypto_call_requests WHERE group_id=?1 AND acknowledged=0 ORDER BY author_device_id LIMIT 64")?;
    let mut rows = statement.query([group_id])?;
    let server_id = server_id(&db)?;
    let mut deliveries = Vec::new();
    while let Some(row) = rows.next()? {
        let user: String = row.get(0)?;
        let device_id: String = row.get(1)?;
        if !roster.iter().any(|m| m.id == user) || device(&db, &user, &device_id).is_err() {
            continue;
        }
        if let Some(selected) = &selected {
            if selected
                .get(&user)
                .is_none_or(|d| d.as_deref() != Some(device_id.as_str()))
            {
                continue;
            }
        }
        let json: String = row.get(2)?;
        let request = serde_json::from_str(&json)?;
        deliveries.push(CallKeyDelivery {
            server_id: server_id.clone(),
            target: target.clone(),
            author_user_id: user,
            request,
        });
    }
    Ok(Json(deliveries))
}

pub async fn ack_call_request(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<AckCallKeyRequest>,
) -> ApiResult<Json<AckCallKeyRequest>> {
    let auth = state.auth(&headers)?;
    if !identifier(&input.operation_id) {
        return Err(bad("Invalid call-key acknowledgement"));
    }
    let mut db = lock(&state.0.db);
    let tx = db.transaction()?;
    let (group_id, _) = scoped_group(&state, &tx, &auth.user.id, &input.target, &input.device_id)?;
    if group_id != input.group_id {
        return Err(forbidden());
    }
    tx.execute(
        "UPDATE crypto_call_requests SET acknowledged=1 WHERE group_id=?1 AND operation_id=?2",
        params![group_id, input.operation_id],
    )?;
    tx.commit()?;
    Ok(Json(input))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InboxQuery {
    device_id: String,
}
pub async fn call_inbox(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<InboxQuery>,
) -> ApiResult<Json<Vec<CallKeyTarget>>> {
    let auth = state.auth(&headers)?;
    let db = lock(&state.0.db);
    device(&db, &auth.user.id, &query.device_id)?;
    let mut statement = db.prepare("SELECT DISTINCT g.group_id,g.target_id FROM crypto_groups g JOIN crypto_call_requests r ON r.group_id=g.group_id JOIN crypto_devices d ON d.device_id=r.author_device_id WHERE g.target_kind='direct' AND r.acknowledged=0 AND d.active=1 AND (substr(g.target_id,1,instr(g.target_id,':')-1)=?1 OR substr(g.target_id,instr(g.target_id,':')+1)=?1) ORDER BY g.group_id LIMIT 129")?;
    let rows = statement.query_map([&auth.user.id], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    let server_id = server_id(&db)?;
    let mut targets = Vec::new();
    for row in rows {
        let (group_id, id) = row?;
        let Some((left, right)) = id.split_once(':') else {
            continue;
        };
        let peer = if left == auth.user.id {
            right
        } else if right == auth.user.id {
            left
        } else {
            continue;
        };
        if social::direct_access(&db, &auth.user.id, peer).is_err()
            || active_user(&db, peer).is_err()
        {
            continue;
        }
        targets.push(CallKeyTarget {
            server_id: server_id.clone(),
            target: CryptoTarget {
                channel_id: None,
                peer_id: Some(peer.into()),
            },
            group_id,
        });
    }
    if targets.len() > 128 {
        return Err(conflict(
            "The private call-key inbox reached its bounded limit",
        ));
    }
    Ok(Json(targets))
}
