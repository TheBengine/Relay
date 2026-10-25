use super::*;
use crate::community::account_destination;
use serde_json::json;

impl Worker {
    pub(super) fn confirm_login_authority(
        &self,
        url: &str,
        expected: Option<&str>,
        invited: Option<&handover::RoutePin>,
    ) -> Result<Option<handover::RoutePin>, String> {
        let remembered = crate::credential_vault::load(url)?;
        let known = self
            .connection
            .as_ref()
            .filter(|c| c.url == url)
            .or_else(|| self.saved_connections.get(url).map(|c| &c.connection))
            .and_then(|c| c.chat.authority.as_ref())
            .or_else(|| {
                remembered
                    .as_ref()
                    .and_then(|r| r.authority.as_ref())
                    .map(|r| &r.pin)
            });
        let mut pins = Vec::new();
        for pin in [invited, known].into_iter().flatten() {
            if expected.is_some_and(|id| id != pin.server_id) || !crate::host_route::valid_pin(pin)
            {
                return Err(crate::host_route::REFUSED.into());
            }
            if !pins.contains(pin) {
                pins.push(pin.clone());
            }
        }
        crate::host_route::confirm_login(url, &pins)
    }

    pub(super) fn account_login_begin(
        &mut self,
        authority: Option<handover::RoutePin>,
        url: String,
        expected: Option<String>,
        invite: zeroize::Zeroizing<String>,
        register: bool,
        initial_protection: Option<ProtectionLevel>,
        request_id: u64,
    ) -> Result<(), String> {
        let url = account_destination(&url)?;
        let summary = check_server(&self.http, &url)?;
        if expected.is_some_and(|id| id != summary.server_id) {
            return Err(IDENTITY_CHANGED.into());
        }
        self.confirm_login_authority(&url, Some(&summary.server_id), authority.as_ref())?;
        let data:serde_json::Value=parse(self.http.post(crate::embedded_network::endpoint(&format!("{url}/api/account-login/challenge")))
            .json(&json!({"destination":url,"invite":invite.as_str(),"register":register,"initial_protection":initial_protection})).send().map_err(network_error)?)?;
        if data["server_id"].as_str() != Some(summary.server_id.as_str()) {
            return Err(IDENTITY_CHANGED.into());
        }
        self.emit(Update::AccountLoginChallenge {
            url,
            data,
            request_id,
        });
        Ok(())
    }
    pub(super) fn account_login_finish(
        &mut self,
        authority: Option<handover::RoutePin>,
        url: String,
        expected: String,
        challenge: String,
        proof: zeroize::Zeroizing<String>,
        cancel: Arc<AtomicBool>,
        request_id: u64,
    ) -> Result<(), String> {
        if cancel.load(Ordering::Acquire) {
            return Ok(());
        }
        let url = account_destination(&url)?;
        let summary = check_server(&self.http, &url)?;
        if summary.server_id != expected {
            return Err(IDENTITY_CHANGED.into());
        }
        if cancel.load(Ordering::Acquire) {
            return Ok(());
        }
        let authority = self.confirm_login_authority(&url, Some(&expected), authority.as_ref())?;
        if cancel.load(Ordering::Acquire) {
            return Ok(());
        }
        let session: Session = parse(
            self.http
                .post(crate::embedded_network::endpoint(&format!(
                    "{url}/api/account-login"
                )))
                .json(&json!({"challenge_id":challenge,"id_token":proof.as_str()}))
                .send()
                .map_err(network_error)?,
        )?;
        if cancel.load(Ordering::Acquire) {
            let _ = self
                .http
                .post(crate::embedded_network::endpoint(&format!(
                    "{url}/api/logout"
                )))
                .bearer_auth(&session.token)
                .send();
            return Ok(());
        }
        self.adopt_session_checked(
            url,
            Some(expected),
            session,
            Some((cancel, request_id)),
            None,
            authority,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn global_account_proofs_require_a_protected_destination() {
        for address in [
            "https://community.example.test",
            "http://127.0.0.1:4580",
            "http://[::1]:4580",
            &format!("http://{}.iroh:4580", "a".repeat(64)),
        ] {
            assert!(account_destination(address).is_ok(), "{address}");
        }
        for address in [
            "http://192.168.111.6:4580",
            "http://ordinary.example.test",
            "http://not-a-peer.iroh",
            "https://user:password@example.test",
        ] {
            assert!(account_destination(address).is_err(), "{address}");
        }
    }
}
