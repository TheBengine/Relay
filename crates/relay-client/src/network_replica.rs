//! Minting and revoking the member's own data-copy token.
//!
//! This is the only code that sees a pull token on its way in. It goes from
//! the community's answer straight into the OS vault; the UI is told only that
//! the access exists. It never travels through the generic administration
//! result, so it cannot reach the admin cache, a log or a support report.
use super::*;
use serde::Deserialize;
use zeroize::Zeroize;

#[derive(Deserialize)]
struct Issued {
    token: String,
    expires: u64,
    server_id: String,
}

/// A token this PC can store and send: printable, bounded, and issued by the
/// community that was asked.
fn usable(issued: &Issued, server_id: &str) -> bool {
    issued.server_id == server_id
        && !issued.token.is_empty()
        && issued.token.len() <= 256
        && issued.token.bytes().all(|byte| byte.is_ascii_graphic())
}

impl Worker {
    pub(super) fn replica_token(
        &self,
        origin: CommandOrigin,
        action: ReplicaTokenAction,
        replica_id: String,
        request_id: u64,
    ) -> Result<(), String> {
        // Queued commands of an older account or community stop here.
        let connection = self.command_connection(&origin)?;
        let offered = connection.chat.supports(crate::replica_copy::CAPABILITY);
        let source_url = crate::community::normalize_url(&origin.url)?;
        let key = crate::credential_vault::ReplicaKey {
            source_url: &source_url,
            server_id: &origin.server_id,
            user_id: &origin.user_id,
            replica_id: &replica_id,
        };
        let outcome = match action {
            ReplicaTokenAction::Mint => {
                if !offered {
                    return Err("This community does not offer a copy on this PC.".into());
                }
                let mut issued: Issued = parse(
                    self.request(reqwest::Method::POST, "/api/replica/token")?
                        .json(&serde_json::json!({}))
                        .send()
                        .map_err(network_error)?,
                )?;
                let saved = if usable(&issued, &origin.server_id) {
                    crate::credential_vault::save_replica_token(&key, &issued.token, issued.expires)
                } else {
                    Err("The community's answer could not be used for a copy.".to_string())
                };
                issued.token.zeroize();
                if let Err(error) = saved {
                    // Never leave a working token behind that this PC cannot keep.
                    let _ = self.revoke_remote();
                    return Err(error);
                }
                ReplicaTokenOutcome::Minted {
                    expires: issued.expires,
                }
            }
            ReplicaTokenAction::Revoke => {
                // The community is asked first, but a community that cannot be
                // reached never keeps the access on this PC alive.
                let remote = self.revoke_remote().is_ok();
                crate::credential_vault::forget_replica_token(&key)?;
                ReplicaTokenOutcome::Revoked { remote }
            }
        };
        self.emit(Update::ReplicaToken {
            origin,
            request_id,
            replica_id,
            outcome,
        });
        Ok(())
    }

    fn revoke_remote(&self) -> Result<(), String> {
        checked(
            self.request(reqwest::Method::DELETE, "/api/replica/token")?
                .timeout(Duration::from_secs(10))
                .send()
                .map_err(network_error)?,
        )
        .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERVER: &str = "6f6087b7-a4a8-4dba-b4c5-58bf76b59d6b";

    fn issued(token: &str, server_id: &str) -> Issued {
        Issued {
            token: token.into(),
            expires: 1,
            server_id: server_id.into(),
        }
    }

    #[test]
    fn only_a_printable_bounded_token_from_the_asked_community_is_kept() {
        assert!(usable(&issued("synthetic-token_123", SERVER), SERVER));
        assert!(!usable(&issued("synthetic-token", "another"), SERVER));
        assert!(!usable(&issued("", SERVER), SERVER));
        assert!(!usable(&issued("two words", SERVER), SERVER));
        assert!(!usable(&issued("line\nbreak", SERVER), SERVER));
        assert!(!usable(&issued(&"x".repeat(257), SERVER), SERVER));
        assert!(usable(&issued(&"x".repeat(256), SERVER), SERVER));
    }

    #[test]
    fn the_answer_shape_matches_the_community_and_ignores_its_note() {
        let issued: Issued = serde_json::from_value(serde_json::json!({
            "token": "synthetic", "expires": 99, "server_id": SERVER,
            "note": "Shown once."
        }))
        .unwrap();
        assert_eq!(issued.expires, 99);
        assert!(serde_json::from_value::<Issued>(serde_json::json!({"token":"x"})).is_err());
    }
}
