//! Destination-bound OIDC sign-in. Community passwords never cross to the IdP.
use super::*;

pub(crate) const ACCOUNT_ONLY: &str = "relay-account-only-v1";

pub(crate) async fn metadata(State(state): State<AppState>) -> ApiResult<PrivateJson> {
    Ok(private(
        json!({"provider":state.0.config.account_provider.as_ref().map(Provider::public),
        "people_service":state.0.config.people,"server_id":server_id(&lock(&state.0.db))?}),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Begin {
    destination: String,
    #[serde(default)]
    invite: Zeroizing<String>,
    #[serde(default)]
    register: bool,
    initial_protection: Option<ProtectionLevel>,
}

pub(crate) async fn begin(
    State(state): State<AppState>,
    ingress: Option<axum::Extension<NetworkIngress>>,
    Json(input): Json<Begin>,
) -> ApiResult<PrivateJson> {
    let provider = state.0.config.account_provider.as_ref().ok_or_else(|| {
        bad("This server has not enabled Relay account sign-in. Use its existing server account.")
    })?;
    // Validate an explicit destination without adopting its identity provider.
    let destination = destination_url(&input.destination)
        .map_err(|_| bad("Relay account sign-in requires HTTPS, iroh or a local connection."))?;
    // The proof is bound to this address. Only this server's own addresses are
    // accepted: otherwise another server could fetch a challenge here, show it
    // to you as its own, and use your signed proof to sign in here as you.
    if !own_destination(&state, provider, &destination) {
        return Err(bad(
            "This address does not belong to this server. Connect with the server's own address or a new invitation.",
        ));
    }
    if input.invite.len() > 512 {
        return Err(bad("Invalid invitation."));
    }
    let mut db = lock(&state.0.db);
    let tx = db.transaction()?;
    let first: bool = tx.query_row("SELECT COUNT(*)=0 FROM users", [], |r| r.get(0))?;
    if (state.0.config.people && (input.initial_protection.is_some() || !input.invite.is_empty()))
        || (first && ingress.is_some() && !state.0.config.people)
        || (input.initial_protection.is_some() && (!first || !input.register))
    {
        return Err(forbidden());
    }
    tx.execute(
        "DELETE FROM identity_login_challenges WHERE expires<=?1",
        [now()],
    )?;
    let count: u32 = tx.query_row("SELECT COUNT(*) FROM identity_login_challenges", [], |r| {
        r.get(0)
    })?;
    if count >= 256 {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "Sign-in is busy. Try again shortly.",
        ));
    }
    let challenge = id();
    let mut random = [0u8; 32];
    OsRng.fill_bytes(&mut random);
    let server = server_id(&tx)?;
    let nonce = format!(
        "relay-login-v1:{server}:{:x}:{}",
        Sha256::digest(destination.as_str().trim_end_matches('/').as_bytes()),
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random)
    );
    tx.execute(
        "INSERT INTO identity_login_challenges VALUES(?1,?2,?3,?4,?5,?6,?7)",
        params![
            challenge,
            nonce,
            provider.issuer.as_str(),
            token_hash(input.invite.trim()),
            input.register,
            input
                .initial_protection
                .map(|p| serde_json::to_string(&p))
                .transpose()
                .map_err(|_| bad("Invalid protection"))?,
            now() + LINK_SECONDS
        ],
    )?;
    tx.commit()?;
    Ok(private(
        json!({"challenge_id":challenge,"nonce":nonce,"provider":provider.public(),"server_id":server,"expires_in":LINK_SECONDS,"people_service":state.0.config.people}),
    ))
}

/// Addresses a sign-in proof may be bound to: the configured community URL,
/// this PC's loopback address, and addresses the host app on this PC told the
/// server (its account-free iroh address).
fn own_destination(state: &AppState, provider: &Provider, destination: &Url) -> bool {
    let wanted = destination.as_str().trim_end_matches('/');
    if wanted == provider.community_url.as_str().trim_end_matches('/') {
        return true;
    }
    if destination.scheme() == "http"
        && matches!(destination.host_str(), Some("127.0.0.1" | "[::1]"))
    {
        return true;
    }
    lock(&state.0.account_destinations).contains(wanted)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OwnDestination {
    url: String,
}

/// The host app on this PC tells its own server another address friends use
/// (the account-free iroh address), so sign-in proofs can be bound to it.
/// Loopback and the local control secret only; network ingress never reaches it.
pub(crate) async fn register_destination(
    State(state): State<AppState>,
    connect: Option<axum::Extension<ConnectInfo<SocketAddr>>>,
    ingress: Option<axum::Extension<NetworkIngress>>,
    headers: HeaderMap,
    Json(input): Json<OwnDestination>,
) -> ApiResult<StatusCode> {
    let loopback = connect.is_some_and(|c| c.0 .0.ip().is_loopback());
    let forwarded = headers.keys().any(|k| {
        let name = k.as_str();
        name == "origin" || name == "forwarded" || name.starts_with("x-forwarded-")
    });
    if ingress.is_some() || !loopback || forwarded {
        return Err(forbidden());
    }
    let expected = state
        .0
        .config
        .local_control_token
        .as_deref()
        .ok_or_else(forbidden)?;
    let value = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    let supplied = value("x-relay-local-control").ok_or_else(forbidden)?;
    if !bool::from(Sha256::digest(expected.as_bytes()).ct_eq(&Sha256::digest(supplied.as_bytes())))
    {
        return Err(forbidden());
    }
    if value("x-relay-instance-id") != Some(state.0.instance_id.as_str())
        || value("x-relay-server-id") != Some(server_id(&lock(&state.0.db))?.as_str())
    {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "The local host changed. Refresh its connection.",
        ));
    }
    let url = destination_url(&input.url).map_err(|_| bad("Invalid address."))?;
    let url = url.as_str().trim_end_matches('/').to_owned();
    let mut own = lock(&state.0.account_destinations);
    if !own.contains(&url) && own.len() >= 8 {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "Too many addresses. Restart the server to reset them.",
        ));
    }
    own.insert(url);
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Finish {
    challenge_id: String,
    id_token: Zeroizing<String>,
}

pub(crate) async fn finish(
    State(state): State<AppState>,
    ingress: Option<axum::Extension<NetworkIngress>>,
    Json(input): Json<Finish>,
) -> ApiResult<PrivateJson> {
    if Uuid::parse_str(&input.challenge_id).is_err()
        || input.id_token.is_empty()
        || input.id_token.len() > 32 * 1024
    {
        return Err(invalid());
    }
    let provider = state
        .0
        .config
        .account_provider
        .as_ref()
        .ok_or_else(invalid)?;
    let _permit = state
        .0
        .identity_jobs
        .clone()
        .try_acquire_owned()
        .map_err(|_| {
            ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "Account sign-in is busy. Try again.",
            )
        })?;
    let (nonce, invite_hash, register, protection) = {
        let mut db = lock(&state.0.db);
        let tx = db.transaction()?;
        let row: Option<(String,String,bool,Option<String>)>=tx.query_row("SELECT nonce,invite_hash,register,initial_protection FROM identity_login_challenges WHERE challenge_id=?1 AND issuer=?2 AND expires>?3",params![input.challenge_id,provider.issuer.as_str(),now()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
        let row = row.ok_or_else(invalid)?;
        tx.execute(
            "DELETE FROM identity_login_challenges WHERE challenge_id=?1",
            [input.challenge_id],
        )?;
        tx.commit()?;
        row
    };
    // Ordinary SSO accepts the provider's existing login session, while every
    // new proof still has a fresh signed issue time and one-use server nonce.
    // Sensitive linking keeps its separate recent-authentication requirement.
    let (subject, handle) = verify_identity(provider, &input.id_token, &nonce, false).await?;
    let session = {
        let mut db = lock(&state.0.db);
        let tx = db.transaction()?;
        let existing: Option<String> = tx
            .query_row(
                "SELECT user_id FROM linked_identities WHERE issuer=?1 AND subject=?2",
                params![provider.issuer.as_str(), subject],
                |r| r.get(0),
            )
            .optional()?;
        let user = if let Some(user_id) = existing {
            // A community authenticator is password-sealed. Never bypass it by
            // using a linked global identity; its old sign-in remains available.
            if authenticator::record(&tx, &user_id)?.is_some() {
                return Err(bad("This existing server account has its own authenticator. Ask this server's owner to update your account access."));
            }
            let status: String = tx.query_row(
                "SELECT status FROM membership WHERE user_id=?1",
                [&user_id],
                |r| r.get(0),
            )?;
            if status == "removed" && register {
                redeem(&tx, &invite_hash)?;
                tx.execute(
                    "UPDATE membership SET status='active' WHERE user_id=?1",
                    [&user_id],
                )?;
                tx.execute(
                    "INSERT OR REPLACE INTO member_invitations VALUES(?1,?2)",
                    params![user_id, invite_hash],
                )?;
                server_settings::record_system_event(&tx, &user_id, "joined")?;
            } else if status != "active" {
                return Err(forbidden());
            }
            tx.query_row("SELECT u.id,u.username,u.role,p.display_name,p.bio,p.version,p.avatar IS NOT NULL,p.appearance FROM users u LEFT JOIN user_profiles p ON p.user_id=u.id WHERE u.id=?1",[&user_id],row_user)?
        } else {
            if !register && !state.0.config.people {
                return Err(bad("Your Relay account has not joined this server yet. Ask its owner for an invitation, or go back and create your own server."));
            }
            let first: bool = tx.query_row("SELECT COUNT(*)=0 FROM users", [], |r| r.get(0))?;
            if first && ingress.is_some() && !state.0.config.people {
                return Err(forbidden());
            }
            if !first && !state.0.config.people {
                if protection.is_some() {
                    return Err(forbidden());
                }
                redeem(&tx, &invite_hash)?;
            }
            let uid = id();
            // A name collision never adopts a legacy user's account or keys.
            let name_taken: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM users WHERE username=?1 COLLATE NOCASE)",
                [&handle],
                |r| r.get(0),
            )?;
            let username = if name_taken {
                if state.0.config.people {
                    return Err(ApiError::new(StatusCode::CONFLICT, "That Relay name belongs to a different account here. Contact the account operator."));
                }
                format!("relay_{}", &uid[..12])
            } else {
                handle.clone()
            };
            let user = User {
                id: uid,
                username,
                role: if first && !state.0.config.people {
                    "admin"
                } else {
                    "member"
                }
                .into(),
                profile: UserProfile {
                    display_name: handle.clone(),
                    ..Default::default()
                },
            };
            tx.execute(
                "INSERT INTO users VALUES(?1,?2,?3,?4)",
                params![user.id, user.username, ACCOUNT_ONLY, user.role],
            )?;
            tx.execute("INSERT INTO membership(user_id) VALUES(?1)", [&user.id])?;
            tx.execute(
                "INSERT INTO user_profiles(user_id,display_name) VALUES(?1,?2)",
                params![user.id, handle],
            )?;
            tx.execute(
                "INSERT INTO linked_identities VALUES(?1,?2,?3,?4,?5)",
                params![user.id, provider.issuer.as_str(), subject, handle, now()],
            )?;
            if !first && !state.0.config.people {
                tx.execute(
                    "INSERT INTO member_invitations VALUES(?1,?2)",
                    params![user.id, invite_hash],
                )?;
            }
            if first && protection.as_deref() == Some("\"open\"") {
                tx.execute(
                    "INSERT INTO metadata(key,value) VALUES('protection',?1)",
                    [serde_json::to_string(&CommunityProtection {
                        level: ProtectionLevel::Open,
                        revision: 1,
                        changed_by: Some(user.id.clone()),
                    })
                    .map_err(|_| bad("Invalid protection"))?],
                )?;
                roles::audit(&tx, &user.id, "server.protection", "open")?;
            }
            roles::audit(&tx, &user.id, "account.joined", &user.id)?;
            server_settings::record_system_event(&tx, &user.id, "joined")?;
            user
        };
        let session = new_session(&tx, user)?;
        tx.commit()?;
        session
    };
    state.notify("members", None, Some(session.user.id.clone()), None);
    Ok(private(
        serde_json::to_value(session).map_err(|_| invalid())?,
    ))
}

fn redeem(db: &Connection, hash: &str) -> ApiResult<()> {
    if db.execute("UPDATE invites SET remaining_uses=remaining_uses-1 WHERE code_hash=?1 AND expires>?2 AND remaining_uses>0",params![hash,now()])?!=1 {return Err(forbidden());}
    Ok(())
}
