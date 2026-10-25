//! Who may host this community, and what they have agreed to.
//!
//! A role here is a permission to hold, never authority to write: nothing in this
//! module lets a second machine accept changes. Offers are made by administrators
//! and take effect only when the member accepts them. Taking a role away is
//! immediate. The roster is stored as one versioned metadata value, so it adds no
//! schema version and travels with the ordinary backup.
//!
//! This release records consent only. No data is copied and no voice is routed
//! through another host yet.
//!
//! Visibility: administrators see every entry. A member sees the hosts that
//! currently hold a role, because those machines will carry community data, and
//! always their own entry in any state. Pending, declined and withdrawn entries
//! of other people stay private: they describe a personal choice and grant
//! nothing.
use super::*;
use relay_protocol::hosting::{
    self as model, Abilities, ActiveHost, CandidateView, HostRole, HostingView, Roster, Task,
};
#[cfg(test)]
mod tests;

const KEY: &str = "hosting.v1";

/// What this build can do. Raised only by the change that adds the protocol.
const ABILITIES: Abilities = Abilities {
    planned_handover: false,
    unexpected_takeover: false,
};

fn load(db: &Connection) -> ApiResult<Roster> {
    let value: Option<String> = db
        .query_row("SELECT value FROM metadata WHERE key=?1", [KEY], |r| {
            r.get(0)
        })
        .optional()?;
    let Some(value) = value else {
        return Ok(Roster::default());
    };
    let mut roster: Roster =
        serde_json::from_str(&value).map_err(|_| bad("Host settings are damaged"))?;
    if roster.schema != model::SCHEMA {
        return Err(bad("Host settings come from a newer version of Relay"));
    }
    // Someone who left or was removed keeps no role, even before the next save.
    roster
        .candidates
        .retain(|candidate| active_user(db, &candidate.user_id).is_ok());
    Ok(roster)
}

/// True while the member holds an offered-and-accepted host role. Checked on
/// each gateway/replica request; never cached.
pub(crate) fn holds_role(db: &Connection, user: &str, role: HostRole) -> bool {
    load(db).is_ok_and(|roster| {
        roster.holders(role).iter().any(|candidate| candidate.user_id == user)
    })
}

pub(crate) fn holds_replica(db: &Connection, user: &str) -> bool {
    holds_role(db, user, HostRole::Replica)
}

/// True while any member holds an offered-and-accepted `Replica` role.
pub(crate) fn any_replica(db: &Connection) -> bool {
    load(db).is_ok_and(|roster| !roster.holders(HostRole::Replica).is_empty())
}

fn save(db: &Connection, roster: &Roster) -> ApiResult<()> {
    let json = serde_json::to_string(roster).map_err(|_| bad("Invalid host settings"))?;
    db.execute(
        "INSERT INTO metadata(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![KEY, json],
    )?;
    Ok(())
}

fn display_name(db: &Connection, user: &str) -> String {
    get_user(db, user)
        .map(|user| {
            if user.profile.display_name.trim().is_empty() {
                user.username
            } else {
                user.profile.display_name
            }
        })
        .unwrap_or_else(|_| "Former member".into())
}

fn view(state: &AppState, db: &Connection, viewer: &str) -> ApiResult<HostingView> {
    let roster = load(db)?;
    let approved_replicas = 1 + roster.holders(HostRole::Replica).len();
    let now_ms = now_ms();
    let can_manage = roles::has(db, viewer, PERMISSION_ADMIN);
    let mut candidates = Vec::new();
    for candidate in &roster.candidates {
        if !can_manage && candidate.user_id != viewer && candidate.effective().is_empty() {
            continue;
        }
        // The source records an authenticated copy's own claims, with its clock.
        // This is not independent proof of storage or permission to take over.
        let copy = replica::service::measured(db, &candidate.user_id)?;
        let replica_facts = copy.and_then(|copy| {
            Some(model::ReplicaFacts {
                applied_seq: copy.claimed_head?.try_into().ok()?,
                head_seq: replica::head(db).ok()?.try_into().ok()?,
                files_pending: copy.claimed_files_pending?.try_into().ok()?,
                reported_ms: copy.claimed_ms?.try_into().ok()?,
            })
        });
        let facts = model::Facts {
            candidate: Some(candidate),
            replica: replica_facts,
            voice: None,
            capacity: None,
            abilities: ABILITIES,
            approved_replicas,
            now_ms,
        };
        let assessments = Task::ALL
            .into_iter()
            .filter(|task| candidate.offered.contains(&task.role()))
            .map(|task| model::assess(task, &facts))
            .collect();
        candidates.push(CandidateView {
            candidate: candidate.clone(),
            name: display_name(db, &candidate.user_id),
            assessments,
        });
    }
    Ok(HostingView {
        schema: model::SCHEMA,
        revision: roster.revision,
        active_host: ActiveHost {
            name: server_settings::presentation(db, &state.0.name)?.name,
            instance_id: state.0.instance_id.clone(),
        },
        abilities: ABILITIES,
        you: viewer.into(),
        can_manage,
        candidates,
    })
}

fn refused(error: model::Error) -> ApiError {
    bad(error.message())
}

pub(super) async fn get_hosting(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<HostingView>> {
    let auth = state.auth(&headers)?;
    let db = lock(&state.0.db);
    active_user(&db, &auth.user.id)?;
    Ok(Json(view(&state, &db, &auth.user.id)?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Offer {
    user_id: String,
    roles: Vec<HostRole>,
}

pub(super) async fn offer_roles(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<Offer>,
) -> ApiResult<Json<HostingView>> {
    let auth = state.auth(&headers)?;
    let result = {
        let mut db = lock(&state.0.db);
        let tx = db.transaction()?;
        roles::require(&tx, &auth.user.id, PERMISSION_ADMIN)?;
        active_user(&tx, &input.user_id)?;
        let mut roster = load(&tx)?;
        roster
            .offer(&input.user_id, &input.roles, &auth.user.id, now_ms())
            .map_err(refused)?;
        save(&tx, &roster)?;
        roles::audit(&tx, &auth.user.id, "hosting.offered", &input.user_id)?;
        let result = view(&state, &tx, &auth.user.id)?;
        tx.commit()?;
        result
    };
    Ok(Json(result))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Answer {
    accepted: Vec<HostRole>,
}

/// The member answers for themselves. Nobody can accept for someone else.
pub(super) async fn answer_offer(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<Answer>,
) -> ApiResult<Json<HostingView>> {
    let auth = state.auth(&headers)?;
    let result = {
        let mut db = lock(&state.0.db);
        let tx = db.transaction()?;
        active_user(&tx, &auth.user.id)?;
        let mut roster = load(&tx)?;
        roster
            .respond(&auth.user.id, &input.accepted, now_ms())
            .map_err(refused)?;
        save(&tx, &roster)?;
        roles::audit(&tx, &auth.user.id, "hosting.answered", &auth.user.id)?;
        replica::service::drop_token_if_role_lost(&tx, &auth.user.id)?;
        let result = view(&state, &tx, &auth.user.id)?;
        tx.commit()?;
        result
    };
    // Stepping down from a role ends its local voice gateway work at once.
    if !holds_role(&lock(&state.0.db), &auth.user.id, HostRole::VoiceGateway) {
        state.0.gateways.cancel_user(&auth.user.id);
    }
    if !holds_replica(&lock(&state.0.db), &auth.user.id) {
        replica::service::after_roster_change(&state, &auth.user.id);
    }
    Ok(Json(result))
}

pub(super) async fn revoke_roles(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(user): Path<String>,
) -> ApiResult<Json<HostingView>> {
    let auth = state.auth(&headers)?;
    let result = {
        let mut db = lock(&state.0.db);
        let tx = db.transaction()?;
        roles::require(&tx, &auth.user.id, PERMISSION_ADMIN)?;
        let mut roster = load(&tx)?;
        roster.revoke(&user, now_ms()).map_err(refused)?;
        save(&tx, &roster)?;
        roles::audit(&tx, &auth.user.id, "hosting.revoked", &user)?;
        replica::service::drop_token_if_role_lost(&tx, &user)?;
        let result = view(&state, &tx, &auth.user.id)?;
        tx.commit()?;
        result
    };
    state.0.gateways.cancel_user(&user);
    replica::service::after_roster_change(&state, &user);
    Ok(Json(result))
}
