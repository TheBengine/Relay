//! Opt-in game activity on the Friends service.
//!
//! The announcement lives only in memory, for a short time: it is never
//! written to the database, a backup or a replica copy, and it vanishes by
//! itself when the sender stops repeating it. Who may see it is decided by the
//! same rule as presence: the person themselves and their current accepted,
//! unblocked friends, while they are online and not invisible.
use super::*;
use relay_protocol::game_activity::{self as model, GameActivity, Share};
use std::collections::HashMap;

#[derive(Clone, PartialEq, Eq)]
pub(super) struct Entry {
    game_id: String,
    join: Option<String>,
    expires_at: u64,
}

pub(super) type Store = Mutex<HashMap<String, Entry>>;

/// At most this many people hold a live announcement at once. Expired ones are
/// pruned first; a person who already has one can always renew it; a new person
/// is turned away rather than anyone being evicted.
pub(super) const MAX_USERS: usize = 10_000;

/// The friends who should hear that this person's activity changed, and the person.
fn audience(db: &Connection, user: &str) -> ApiResult<Vec<String>> {
    let mut users = social::relationships(db, user)?.friends;
    users.push(user.to_owned());
    Ok(users)
}

fn visible(entry: &Entry, at: u64) -> bool {
    entry.expires_at > at
}

/// Announce (or renew) the game being played.
pub(super) async fn share(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<Share>,
) -> ApiResult<Json<GameActivity>> {
    if !state.0.config.people {
        return Err(missing());
    }
    let auth = state.auth(&headers)?;
    let game = model::find(&input.game_id).ok_or_else(|| bad("Choose a game from the list"))?;
    let join = match input.join.as_deref().map(str::trim).filter(|l| !l.is_empty()) {
        None => None,
        Some(link) => Some(model::join_link(game, link).ok_or_else(|| {
            bad("Share a Steam lobby link for this game, in the form steam://joinlobby/<game>/<lobby>/<user>")
        })?),
    };
    let expires_at = now() + model::TTL_SECS;
    let (changed, friends) = {
        let db = lock(&state.0.db);
        active_user(&db, &auth.user.id)?;
        let mut store = lock(&state.0.activity);
        let at = now();
        store.retain(|_, entry| visible(entry, at));
        if store.len() >= MAX_USERS && !store.contains_key(&auth.user.id) {
            return Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "Game activity is busy right now; try again later",
            ));
        }
        let entry = Entry {
            game_id: game.id.into(),
            join: join.clone(),
            expires_at,
        };
        let changed = store
            .get(&auth.user.id)
            .is_none_or(|old| old.game_id != entry.game_id || old.join != entry.join);
        store.insert(auth.user.id.clone(), entry);
        (
            changed,
            if changed {
                audience(&db, &auth.user.id)?
            } else {
                vec![]
            },
        )
    };
    if changed {
        state.notify("members", None, Some(auth.user.id.clone()), Some(friends));
    }
    Ok(Json(GameActivity {
        game_id: game.id.into(),
        name: game.name.into(),
        join,
        expires_at,
    }))
}

/// Stop sharing now.
pub(super) async fn withdraw(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<StatusCode> {
    if !state.0.config.people {
        return Err(missing());
    }
    let auth = state.auth(&headers)?;
    let removed = lock(&state.0.activity).remove(&auth.user.id).is_some();
    if removed {
        let friends = audience(&lock(&state.0.db), &auth.user.id)?;
        state.notify("members", None, Some(auth.user.id), Some(friends));
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Attach live activity to the presence rows the viewer is already allowed to
/// see (themselves and current friends). Presence already shows offline,
/// invisible, stale and blocked people as "offline"; they carry no activity.
pub(super) fn overlay(
    state: &AppState,
    viewer: &str,
    friends: &[String],
    presence: &mut [UserPresence],
) {
    let allowed: std::collections::HashSet<&str> = friends.iter().map(String::as_str).collect();
    let store = lock(&state.0.activity);
    let at = now();
    for entry in presence.iter_mut() {
        entry.activity = None;
        if (entry.user_id != viewer && !allowed.contains(entry.user_id.as_str()))
            || entry.status == "offline"
            || entry.status == "invisible"
        {
            continue;
        }
        let Some(current) = store.get(&entry.user_id).filter(|e| visible(e, at)) else {
            continue;
        };
        if let Some(game) = model::find(&current.game_id) {
            entry.activity = Some(GameActivity {
                game_id: game.id.into(),
                name: game.name.into(),
                join: current.join.clone(),
                expires_at: current.expires_at,
            });
        }
    }
}

/// Test support: pretend `by` seconds have passed since this person last renewed.
#[cfg(test)]
pub(super) fn age(state: &AppState, user: &str, by: u64) {
    if let Some(entry) = lock(&state.0.activity).get_mut(user) {
        entry.expires_at = entry.expires_at.saturating_sub(by);
    }
}

/// Test support: fill the store with `count` synthetic people who renewed just now.
#[cfg(test)]
pub(super) fn fill(state: &AppState, count: usize) {
    let mut store = lock(&state.0.activity);
    for index in 0..count {
        store.insert(
            format!("synthetic-{index}"),
            Entry {
                game_id: "terraria".into(),
                join: None,
                expires_at: now() + model::TTL_SECS,
            },
        );
    }
}

/// Test support: everyone but `keep` stopped renewing long ago.
#[cfg(test)]
pub(super) fn lapse_all_but(state: &AppState, keep: &str) {
    for (user, entry) in lock(&state.0.activity).iter_mut() {
        if user != keep {
            entry.expires_at = 0;
        }
    }
}

#[cfg(test)]
pub(super) fn count(state: &AppState) -> usize {
    lock(&state.0.activity).len()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn game_activity_does_not_leak_to_a_presence_only_group_comember() {
        let data = tempfile::tempdir().unwrap();
        let state = AppState::open(data.path(), "activity fixture", 0).unwrap();
        lock(&state.0.activity).insert(
            "player".into(),
            Entry {
                game_id: "terraria".into(),
                join: None,
                expires_at: now() + 60,
            },
        );
        let mut presence = vec![UserPresence {
            user_id: "player".into(),
            status: "online".into(),
            ..Default::default()
        }];
        overlay(&state, "group-member", &[], &mut presence);
        assert!(presence[0].activity.is_none());
        overlay(&state, "friend", &["player".into()], &mut presence);
        assert!(presence[0].activity.is_some());
        overlay(&state, "player", &[], &mut presence);
        assert!(presence[0].activity.is_some());
    }
}
