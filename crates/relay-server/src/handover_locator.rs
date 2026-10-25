//! Only a currently writable host can issue an encrypted route hint. The
//! independent directory never receives the invite verifier or admission key.
use super::*;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    ChaCha20Poly1305, Nonce,
};
use relay_protocol::host_locator as model;

fn hash_bytes(value: &str) -> Option<[u8; 32]> {
    if value.len() != 64 {
        return None;
    }
    let mut out = [0; 32];
    for (at, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        let digit = |v: u8| match v {
            b'0'..=b'9' => Some(v - b'0'),
            b'a'..=b'f' => Some(v - b'a' + 10),
            _ => None,
        };
        out[at] = digit(pair[0])? * 16 + digit(pair[1])?;
    }
    Some(out)
}

pub(super) async fn issue(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<model::Issue>,
) -> ApiResult<Json<model::Record>> {
    if state.0.config.people || !state.writes_allowed() {
        return Err(forbidden());
    }
    let auth = state.auth(&headers)?;
    let db = lock(&state.0.db);
    roles::require(&db, &auth.user.id, PERMISSION_ADMIN)?;
    let invite_hash = hash_bytes(&input.invite_id).ok_or_else(|| bad("Invalid route request"))?;
    let expiry: u64 = db
        .query_row(
            "SELECT expires FROM invites WHERE code_hash=?1 AND expires>?2 AND remaining_uses>0",
            params![input.invite_id, now()],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(forbidden)?;
    // The journal lock prevents retirement/rotation between ancestry capture
    // and signing. A retired source must never mint a new 'current' record.
    state
        .0
        .handover
        .issue_locator(input, invite_hash, expiry)
        .map(Json)
}

pub(super) fn seal(
    input: model::Issue,
    invite_hash: [u8; 32],
    expiry: u64,
    current: relay_protocol::handover::RoutePin,
    history: Vec<relay_protocol::handover::ActivationGrant>,
    sign: impl FnOnce(&[u8]) -> String,
) -> ApiResult<model::Record> {
    seal_for(model::Purpose::Invitation,input,invite_hash,expiry,current,history,sign)
}
pub(super) fn seal_for(
    purpose:model::Purpose,input:model::Issue,verifier:[u8;32],expiry:u64,
    current:relay_protocol::handover::RoutePin,history:Vec<relay_protocol::handover::ActivationGrant>,
    sign:impl FnOnce(&[u8])->String,
)->ApiResult<model::Record> {
    let anchored = input.anchor == current
        || history.iter().any(|g| {
            g.source_identity == input.anchor.host
                && g.receipt.body.term == input.anchor.term
                && g.receipt.body.server_id == input.anchor.server_id
        });
    if !anchored
        || input.anchor.server_id != current.server_id
        || !handover::destination_route(&input.destination.url, input.destination.iroh.as_ref())
        || input
            .destination
            .iroh
            .as_ref()
            .is_some_and(|r| r.scope.as_deref() != Some(current.host.host_id.as_str()))
        || history.last().is_some_and(|g| {
            url::Url::parse(&g.receipt.body.target_url).ok()
                != url::Url::parse(&input.destination.url).ok()
        })
    {
        return Err(bad("The route does not match this host's authority"));
    }
    let issued_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| bad("The system clock is unavailable"))?
        .as_millis()
        .min(u64::MAX as u128) as u64;
    let expires_ms = expiry
        .saturating_mul(1000)
        .min(issued_ms.saturating_add(model::MAX_LIFETIME_MS));
    if expires_ms <= issued_ms {
        return Err(forbidden());
    }
    let material=match purpose {
        model::Purpose::Invitation=>model::capability_material,
        model::Purpose::Session=>model::session_material,
    };
    let key=Sha256::digest(material(&input.anchor,&verifier,true));
    let id=format!("{:x}",Sha256::digest(material(&input.anchor,&verifier,false)));
    let mut nonce = [0u8; 12];
    OsRng.fill_bytes(&mut nonce);
    let mut record = model::Record {
        id,
        anchor: input.anchor,
        current,
        history,
        issued_ms,
        expires_ms,
        nonce: URL_SAFE_NO_PAD.encode(nonce),
        ciphertext: String::new(),
        signature: String::new(),
    };
    let raw = serde_json::to_vec(&input.destination).map_err(|_| bad("Invalid destination"))?;
    if raw.len() + 16 > model::MAX_CIPHERTEXT_BYTES {
        return Err(bad("Destination is too large"));
    }
    let encrypted = ChaCha20Poly1305::new_from_slice(&key)
        .map_err(|_| bad("Route encryption unavailable"))?
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: &raw,
                aad: &model::associated_data(&record),
            },
        )
        .map_err(|_| bad("Route encryption unavailable"))?;
    record.ciphertext = URL_SAFE_NO_PAD.encode(encrypted);
    record.signature = sign(&model::record_message(&record));
    if serde_json::to_vec(&record)
        .map_err(|_| bad("Invalid route record"))?
        .len()
        > model::MAX_RECORD_BYTES
    {
        return Err(bad("Route history is too large"));
    }
    Ok(record)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{new_invite, request, signup};
    use serde_json::json;
    #[tokio::test]
    async fn only_admin_can_issue_live_invite_hint_and_frozen_source_cannot_sign() {
        let (_temp, state, app, owner, target, change) = crate::handover::tests::accepted().await;
        let code = new_invite(&app, &owner).await;
        let pin = state.0.handover.route_pin();
        let body = json!({"invite_id":token_hash(&code),"anchor":pin,"destination":{"url":"https://source.example"}});
        for token in [None, Some(target.token.as_str())] {
            assert!(
                request(&app, "POST", model::ISSUE_PATH, token, body.clone())
                    .await
                    .0
                    .is_client_error()
            );
        }
        let (status, value) = request(
            &app,
            "POST",
            model::ISSUE_PATH,
            Some(&owner.token),
            body.clone(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{value}");
        let record: model::Record = serde_json::from_value(value).unwrap();
        assert!(crate::handover_target::verify(
            &relay_protocol::handover::HostProof {
                identity: pin.host.clone(),
                signature: record.signature.clone(),
            },
            &model::record_message(&record)
        ));
        let raw = serde_json::to_string(&record).unwrap();
        assert!(
            !raw.contains(&token_hash(&code))
                && !raw.contains(&code)
                && !raw.contains("source.example")
        );
        // Freeze is not required to succeed here: the ordinary role test did
        // not grant separate full-data consent. Simulate the same durable
        // admission barrier through the authority's existing test interface.
        state
            .0
            .handover
            .begin_restore(state.server_id().unwrap())
            .unwrap();
        assert_eq!(
            request(&app, "POST", model::ISSUE_PATH, Some(&owner.token), body)
                .await
                .0,
            StatusCode::CONFLICT
        );
        let _ = change;
    }
    #[tokio::test]
    async fn revoked_or_expired_invite_and_foreign_anchor_cannot_issue_a_hint() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::open(temp.path(), "locator", 0).unwrap();
        let app = router(state.clone());
        let owner = signup(&app, "Owner", "").await;
        let code = new_invite(&app, &owner).await;
        let mut pin = state.0.handover.route_pin();
        pin.term += 1;
        let mut body = json!({"invite_id":token_hash(&code),"anchor":pin,"destination":{"url":"https://source.example"}});
        assert_eq!(
            request(
                &app,
                "POST",
                model::ISSUE_PATH,
                Some(&owner.token),
                body.clone()
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        body["anchor"] = serde_json::to_value(state.0.handover.route_pin()).unwrap();
        lock(&state.0.db)
            .execute(
                "DELETE FROM invites WHERE code_hash=?1",
                [token_hash(&code)],
            )
            .unwrap();
        assert_eq!(
            request(&app, "POST", model::ISSUE_PATH, Some(&owner.token), body)
                .await
                .0,
            StatusCode::FORBIDDEN
        );
    }
}
