use super::*;
use crate::community::account_destination;
use serde_json::json;

impl Worker {
    pub(super) fn account_login_begin(
        &mut self,
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
        self.adopt_session_checked(url, Some(expected), session, Some((cancel, request_id)))
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
