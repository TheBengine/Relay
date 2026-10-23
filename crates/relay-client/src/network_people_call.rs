//! A narrow background action; generic Admin remains foreground-account only.
use super::*;

fn decline_target<'a>(
    origin: &CommandOrigin,
    call_id: &str,
    bootstrap: &'a Bootstrap,
    now: u64,
) -> Result<&'a PrivateCall, String> {
    if origin.server_id.is_empty() || origin.user_id.is_empty() {
        return Err("Sign in to Friends before answering this invitation.".into());
    }
    validate_bootstrap_identity(&origin.server_id, &origin.user_id, bootstrap)?;
    if uuid::Uuid::parse_str(call_id).is_err()
        || !bootstrap.capabilities.iter().any(|c| c == "people-v1")
    {
        return Err("This is not an account-wide Friends call.".into());
    }
    bootstrap
        .social
        .calls
        .iter()
        .find(|call| {
            call.id == call_id
                && call.recipient_id == origin.user_id
                && call.caller_id != origin.user_id
                && call.state == "ringing"
                && call.expires_at > now
                && bootstrap.social.friends.contains(&call.caller_id)
                && !bootstrap.social.blocked.contains(&call.caller_id)
                && bootstrap.members.iter().any(|u| u.id == call.caller_id)
        })
        .ok_or_else(|| {
            "This friend's call is no longer ringing. Open Friends to see its status.".into()
        })
}

impl Worker {
    pub(super) fn decline_people_call(
        &mut self,
        origin: &CommandOrigin,
        call_id: &str,
    ) -> Result<(), String> {
        // Resolve only an already authenticated, exact account/service identity.
        // No resume/login, URL discovery, voice command or generic Admin request.
        let connection = self
            .connection
            .as_ref()
            .filter(|c| CommandOrigin::from(*c) == *origin)
            .or_else(|| {
                self.saved_connections
                    .get(&origin.url)
                    .map(|saved| &saved.connection)
                    .filter(|c| CommandOrigin::from(*c) == *origin)
            })
            .ok_or("This call belongs to a signed-out account. Open Friends to sign in again.")?;
        let token = zeroize::Zeroizing::new(connection.session.token.clone());
        let endpoint =
            |path: &str| crate::embedded_network::endpoint(&format!("{}{path}", origin.url));
        let mut bootstrap: Bootstrap = parse(
            self.http
                .get(endpoint("/api/bootstrap"))
                .bearer_auth(token.as_str())
                .send()
                .map_err(network_error)?,
        )?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let call = decline_target(origin, call_id, &bootstrap, now)?.clone();
        let answer: PrivateCall = parse(
            self.http
                .post(endpoint(&format!("/api/private-calls/{call_id}")))
                .bearer_auth(token.as_str())
                .json(&serde_json::json!({"action": "decline"}))
                .send()
                .map_err(network_error)?,
        )?;
        if answer.id != call.id
            || answer.caller_id != call.caller_id
            || answer.recipient_id != call.recipient_id
            || answer.channel_id != call.channel_id
            || answer.state != "declined"
        {
            return Err("The call response did not match this invitation. Open Friends to check its status.".into());
        }
        // Apply only the authenticated call answer to the already validated
        // snapshot. A second, optional network fetch cannot turn success into a
        // misleading failure or hide it behind an unrelated foreground error.
        if let Some(row) = bootstrap.social.calls.iter_mut().find(|c| c.id == call.id) {
            *row = answer;
        }
        if self
            .connection
            .as_ref()
            .is_some_and(|c| CommandOrigin::from(c) == *origin)
        {
            self.emit(Update::Bootstrap(bootstrap));
        } else {
            self.emit(Update::CommunityBootstrap {
                url: origin.url.clone(),
                bootstrap,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_decline_requires_the_exact_service_account_and_live_friend_ring() {
        let origin = CommandOrigin {
            url: "https://accounts.invalid/people".into(),
            server_id: "friends".into(),
            user_id: "me".into(),
        };
        let mut b = Bootstrap {
            server_id: origin.server_id.clone(),
            ..Default::default()
        };
        b.user.id = origin.user_id.clone();
        b.capabilities.push("people-v1".into());
        b.members.push(User {
            id: "friend".into(),
            ..Default::default()
        });
        b.social.friends.push("friend".into());
        let id = uuid::Uuid::new_v4().to_string();
        b.social.calls.push(PrivateCall {
            id: id.clone(),
            caller_id: "friend".into(),
            recipient_id: "me".into(),
            state: "ringing".into(),
            expires_at: 200,
            channel_id: "pair".into(),
        });
        assert!(decline_target(&origin, &id, &b, 100).is_ok());
        assert!(decline_target(&origin, &id, &b, 200).is_err());
        for (server_id, user_id) in [("other", "me"), ("friends", "other")] {
            let other = CommandOrigin {
                server_id: server_id.into(),
                user_id: user_id.into(),
                ..origin.clone()
            };
            assert!(decline_target(&other, &id, &b, 100).is_err());
        }
        for state in ["accepted", "ended", "declined", "missed"] {
            b.social.calls[0].state = state.into();
            assert!(decline_target(&origin, &id, &b, 100).is_err());
        }
        b.social.calls[0].state = "ringing".into();
        b.social.calls[0].recipient_id = "outsider".into();
        assert!(decline_target(&origin, &id, &b, 100).is_err());
        b.social.calls[0].recipient_id = "me".into();
        b.social.blocked.push("friend".into());
        assert!(decline_target(&origin, &id, &b, 100).is_err());
        b.social.blocked.clear();
        b.social.friends.clear();
        assert!(decline_target(&origin, &id, &b, 100).is_err());
    }
}
