//! Narrow account-bound activity command. A background Friends session never
//! borrows the selected community's credentials or changes its media/session.
use super::*;
use relay_protocol::game_activity::{self as activity, GameActivity, Share};

fn destination(origin: &CommandOrigin, bootstrap: &Bootstrap) -> Result<(), String> {
    if origin.server_id.is_empty() || origin.user_id.is_empty() || origin.url.is_empty() {
        return Err("Sign in to Friends to confirm this account and service.".into());
    }
    validate_bootstrap_identity(&origin.server_id, &origin.user_id, bootstrap)?;
    if !bootstrap.capabilities.iter().any(|v| v == "people-v1")
        || !bootstrap
            .capabilities
            .iter()
            .any(|v| v == activity::CAPABILITY)
    {
        return Err("This Friends service does not support game activity.".into());
    }
    Ok(())
}

impl Worker {
    pub(super) fn set_game_activity(
        &self,
        origin: &CommandOrigin,
        share: Option<&Share>,
    ) -> Result<Option<GameActivity>, String> {
        let connection = self
            .connection
            .as_ref()
            .filter(|c| CommandOrigin::from(*c) == *origin)
            .or_else(|| {
                self.saved_connections
                    .get(&origin.url)
                    .map(|v| &v.connection)
                    .filter(|c| CommandOrigin::from(*c) == *origin)
            })
            .ok_or("Sign in to Friends before sharing game activity.")?;
        let endpoint =
            |path: &str| crate::embedded_network::endpoint(&format!("{}{path}", origin.url));
        let token = zeroize::Zeroizing::new(connection.session.token.clone());
        // A fresh identity/capability check happens before either mutation.
        let bootstrap: Bootstrap = parse(
            self.http
                .get(endpoint("/api/bootstrap"))
                .bearer_auth(token.as_str())
                .send()
                .map_err(network_error)?,
        )?;
        destination(origin, &bootstrap)?;
        if let Some(share) = share {
            let Some(game) = activity::find(&share.game_id) else {
                return Err("Unknown game activity.".into());
            };
            if share
                .join
                .as_ref()
                .is_some_and(|link| activity::join_link(game, link).as_ref() != Some(link))
            {
                return Err("Invalid Steam lobby invitation.".into());
            }
            let result: GameActivity = parse(
                self.http
                    .put(endpoint("/api/activity"))
                    .bearer_auth(token.as_str())
                    .json(share)
                    .send()
                    .map_err(network_error)?,
            )?;
            if result.game_id != share.game_id
                || result.name != game.name
                || result.join != share.join
            {
                return Err("Friends did not confirm this game activity.".into());
            }
            Ok(Some(result))
        } else {
            checked(
                self.http
                    .delete(endpoint("/api/activity"))
                    .bearer_auth(token.as_str())
                    .send()
                    .map_err(network_error)?,
            )?;
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_background_write_requires_the_exact_friends_account_and_capability() {
        let origin = CommandOrigin {
            url: "https://accounts.invalid/people".into(),
            server_id: "friends".into(),
            user_id: "me".into(),
        };
        let mut b = Bootstrap::default();
        b.server_id = origin.server_id.clone();
        b.user.id = origin.user_id.clone();
        b.capabilities = vec!["people-v1".into(), activity::CAPABILITY.into()];
        assert!(destination(&origin, &b).is_ok());
        for (server, user) in [("community", "me"), ("friends", "someone-else")] {
            assert!(destination(
                &CommandOrigin {
                    server_id: server.into(),
                    user_id: user.into(),
                    ..origin.clone()
                },
                &b
            )
            .is_err());
        }
        b.capabilities.retain(|v| v != "people-v1");
        assert!(destination(&origin, &b).is_err());
        b.capabilities = vec!["people-v1".into()];
        assert!(destination(&origin, &b).is_err());
    }
}
