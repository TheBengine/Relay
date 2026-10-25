use crate::{
    community::{normalize_url, SavedCommunity},
    invitation::Invitation,
};
use serde::Deserialize;

#[derive(Clone, Deserialize)]
pub struct ServerSummary {
    pub ok: bool,
    pub server_name: String,
    pub server_id: String,
    pub protocol_version: u32,
    pub rtc_enabled: bool,
    /// Optional hint from a local server; it is not permission to register.
    #[serde(default)]
    pub setup_required: Option<bool>,
}

impl ServerSummary {
    pub fn validate(&self, expected: Option<&str>) -> Result<(), String> {
        if !self.ok
            || self.server_name.trim().is_empty()
            || self.server_name.len() > 256
            || self.server_name.chars().any(char::is_control)
            || uuid::Uuid::parse_str(&self.server_id).is_err()
        {
            return Err("This address did not return a valid Relay community.".into());
        }
        if self.protocol_version != relay_protocol::API_PROTOCOL_VERSION {
            return Err("This server uses an incompatible Relay version.".into());
        }
        if expected.is_some_and(|id| id != self.server_id) {
            return Err("This address belongs to a different community than the invitation. Ask the host for a new invitation.".into());
        }
        Ok(())
    }
}

struct CheckCancellation(std::sync::Arc<std::sync::atomic::AtomicBool>);
impl Default for CheckCancellation {
    fn default() -> Self {
        Self(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
            false,
        )))
    }
}
impl Drop for CheckCancellation {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }
}

/// Each result is bound to both the requested address and the request generation.
#[derive(Default)]
pub struct SetupState {
    /// Invitation input is separate from the extracted address and is never saved.
    pub invitation_text: String,
    pub private_connection_started: bool,
    pub saved_server_id: Option<String>,
    pub invitation: Option<Invitation>,
    /// A temporary clipboard suggestion for this flow only; never saved.
    pub clipboard: crate::invitation_clipboard::ClipboardOffer,
    pub pending: Option<(String, u64)>,
    pub result: Option<Result<ServerSummary, String>>,
    /// An invitation with several routes is being measured in the background.
    pub routing: Option<crate::route_choice::Job>,
    /// The route the invitation was refused for, kept so the user may go on.
    pub refused: Option<crate::route_choice::Refused>,
    /// One line about the chosen route, for example "Using your home network".
    pub route_note: Option<String>,
    /// "Find on local network" for this form: finite, cancelable, never joins anything.
    pub lan: crate::lan_discovery::Finder,
    pub lan_pairing: crate::lan_pairing::JoinState,
    checked_url: Option<String>,
    check_cancel: CheckCancellation,
}

/// One sign-in or join form: its setup check, address, invitation,
/// registration choice and credentials. Field types are the ones the app
/// already used; nothing here is saved, logged or printed (no `Debug`).
#[derive(Default)]
pub struct JoinFlow {
    pub setup: SetupState,
    pub url: String,
    pub username: String,
    pub password: String,
    pub second_factor: String,
    pub factor_binding: Option<(String, String, bool)>,
    pub invite: String,
    pub registering: bool,
}

impl JoinFlow {
    /// Only a worker-verified delegation may change the invitation's destination.
    /// A late answer must never replace a newly edited form or its credentials.
    pub fn accept_checked(
        &mut self,
        source: &str,
        request: u64,
        result: Result<ServerSummary, String>,
        resolved: Option<String>,
    ) {
        if self
            .setup
            .pending
            .as_ref()
            .is_none_or(|(url, id)| url != source || *id != request)
            || normalize_url(&self.url).ok().as_deref() != Some(source)
        {
            return;
        }
        let result = (|| {
            if let Some(target) = resolved {
                let invitation = self
                    .setup
                    .invitation
                    .as_ref()
                    .ok_or(crate::host_route::REFUSED)?;
                let pin = invitation
                    .authority
                    .as_ref()
                    .ok_or(crate::host_route::REFUSED)?;
                let summary = result?;
                summary.validate(Some(&pin.server_id))?;
                let target = crate::host_route::safe_destination(&target)?;
                let mut next = invitation.clone();
                next.server = target.clone();
                next.alt.clear();
                next.network_share = None;
                next.iroh = crate::iroh_route::saved_for(&target)
                    .map(|route| route.to_invitation()).transpose()?;
                self.setup.invitation = Some(next);
                self.url = target.clone();
                self.setup.pending = Some((target, request));
                self.setup.route_note = Some("Using the community's new host".into());
                Ok(summary)
            } else {
                result
            }
        })();
        let current = self.url.clone();
        self.setup.accept(&current, &current, request, result);
    }

    /// Fill the address from a "Find on local network" answer. The answer is
    /// unauthenticated, so it can never replace a server identity already saved
    /// for the same canonical address: a different advertised id is refused and
    /// nothing changes. Without a saved identity, the advertised id is only a pin
    /// for the ordinary connection check that follows.
    pub fn select_discovered(
        &mut self,
        saved: &[SavedCommunity],
        found: &crate::lan_discovery::Found,
    ) -> Result<(), String> {
        let url = normalize_url(&found.url())?;
        let known = saved.iter().find(|community| {
            !community.server_id.is_empty()
                && normalize_url(&community.url).ok().as_ref() == Some(&url)
        });
        if let Some(known) = known {
            if !known.server_id.eq_ignore_ascii_case(&found.server_id) {
                return Err("A different server answered at this address than the one you saved. Nothing was changed. Ask the host for a new invitation.".into());
            }
        }
        let pin = known.map_or_else(|| found.server_id.clone(), |c| c.server_id.clone());
        self.url = url;
        self.setup.edited(&mut self.invite);
        self.setup.saved_server_id = Some(pin);
        Ok(())
    }

    /// Use the HTTPS or iroh route this PC already saved for the server that
    /// answered. The address and the identity pin come from the saved record, never
    /// from the answer, and the ordinary connection and identity checks still run.
    pub fn select_saved_route(
        &mut self,
        saved: &[SavedCommunity],
        found: &crate::lan_discovery::Found,
        iroh_helper_installed: bool,
    ) -> Result<(), String> {
        let Some(route) = crate::lan_discovery::secure_route(saved, found, iroh_helper_installed)
        else {
            return Err("No secure address is saved for this server.".into());
        };
        self.url = route.url;
        self.setup.edited(&mut self.invite);
        self.setup.saved_server_id = Some(route.server_id);
        Ok(())
    }
}

/// Which of the two forms a shared helper works on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlowKind {
    /// The sign-in form of the active community.
    Login,
    /// The "Join friends" form for adding another community.
    AddCommunity,
}

impl SetupState {
    pub fn check_cancellation(&self) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        self.check_cancel.0.clone()
    }
    /// A result, including failure, consumes the automatic attempt. Only an
    /// explicit retry may replace it; repainting must never poll the server.
    pub fn needs_automatic_check(&self) -> bool {
        self.pending.is_none() && self.result.is_none()
    }

    pub fn begin_check(
        &mut self,
        address: &str,
        request_id: u64,
        retry: bool,
    ) -> Result<Option<String>, String> {
        if self.pending.is_some() || (!retry && self.result.is_some()) {
            return Ok(None);
        }
        let url = normalize_url(address)?;
        self.expected_id(&url)?;
        self.result = None;
        self.check_cancel = Default::default();
        self.checked_url = None;
        self.pending = Some((url.clone(), request_id));
        Ok(Some(url))
    }

    pub fn verified_for(&self, address: &str) -> bool {
        let Ok(expected) = self.expected_id(address) else {
            return false;
        };
        self.pending.is_none()
            && normalize_url(address).ok().as_ref() == self.checked_url.as_ref()
            && self.result.as_ref().is_some_and(|r| {
                r.as_ref()
                    .is_ok_and(|s| s.validate(expected.as_deref()).is_ok())
            })
    }

    pub fn edited(&mut self, invite_code: &mut String) {
        self.lan_pairing = Default::default();
        if self.invitation.take().is_some() {
            invite_code.clear();
        }
        self.pending = None;
        self.check_cancel = Default::default();
        self.result = None;
        self.checked_url = None;
        self.private_connection_started = false;
        self.saved_server_id = None;
        self.routing = None;
        self.refused = None;
        self.route_note = None;
    }

    pub fn expected_id(&self, address: &str) -> Result<Option<String>, String> {
        let url = normalize_url(address)?;
        match &self.invitation {
            Some(invitation) if invitation.server != url => {
                Err("The invitation address changed. Paste the invitation again.".into())
            }
            Some(invitation) => Ok(Some(invitation.server_id.clone())),
            None => Ok(self.saved_server_id.clone()),
        }
    }

    /// Manual sign-in must retain a known server pin even after the setup UI
    /// was reset. A supplied invitation or explicit setup pin takes precedence.
    pub fn expected_id_with_saved(
        &self,
        address: &str,
        saved: &[SavedCommunity],
    ) -> Result<Option<String>, String> {
        if let Some(expected) = self.expected_id(address)? {
            return Ok(Some(expected));
        }
        let url = normalize_url(address)?;
        let Some(community) = saved.iter().find(|community| {
            !community.server_id.is_empty()
                && normalize_url(&community.url).ok().as_ref() == Some(&url)
        }) else {
            return Ok(None);
        };
        uuid::Uuid::parse_str(&community.server_id).map_err(|_| {
            "The saved server identity is invalid. Ask the host for a new invitation.".to_string()
        })?;
        Ok(Some(community.server_id.clone()))
    }

    pub fn accept(
        &mut self,
        current: &str,
        url: &str,
        request_id: u64,
        result: Result<ServerSummary, String>,
    ) {
        if self
            .pending
            .as_ref()
            .is_none_or(|(pending_url, id)| pending_url != url || *id != request_id)
            || normalize_url(current).ok().as_deref() != Some(url)
        {
            return;
        }
        self.pending = None;
        self.checked_url = Some(url.to_owned());
        self.result = Some(result.and_then(|summary| {
            summary.validate(
                self.invitation
                    .as_ref()
                    .map(|invite| invite.server_id.as_str())
                    .or(self.saved_server_id.as_deref()),
            )?;
            Ok(summary)
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn saved_community(url: &str, server_id: &str) -> SavedCommunity {
        SavedCommunity {
            url: url.into(),
            server_id: server_id.into(),
            ..Default::default()
        }
    }

    #[test]
    fn manual_sign_in_retains_saved_identity_for_the_same_canonical_address() {
        let pin = "4be44aef-8c98-4db6-a9d5-81c2eb311330";
        let saved = [saved_community(" HTTP://EXAMPLE.COM:80/// ", pin)];
        let state = SetupState::default();
        assert_eq!(
            state
                .expected_id_with_saved("http://example.com", &saved)
                .unwrap(),
            Some(pin.into())
        );
        let saved = [saved_community("http://example.com", pin)];
        assert_eq!(
            state
                .expected_id_with_saved(" HTTP://EXAMPLE.COM:80/// ", &saved)
                .unwrap(),
            Some(pin.into())
        );
        assert!(summary("dc310e12-b973-47c8-99e4-d71a40dc3c12")
            .validate(
                state
                    .expected_id_with_saved("http://example.com", &saved)
                    .unwrap()
                    .as_deref()
            )
            .is_err());
    }

    #[test]
    fn manual_sign_in_never_borrows_a_pin_from_another_address() {
        let pin = "4be44aef-8c98-4db6-a9d5-81c2eb311330";
        let saved = [saved_community("http://example.com/relay", pin)];
        let state = SetupState::default();
        for other in [
            "http://other.example.com/relay",
            "https://example.com/relay",
            "http://example.com:4580/relay",
            "http://example.com/another",
            "http://example.com",
        ] {
            assert!(state
                .expected_id_with_saved(other, &saved)
                .unwrap()
                .is_none());
        }
        let old = [saved_community("http://example.com", "")];
        assert!(state
            .expected_id_with_saved("http://example.com", &old)
            .unwrap()
            .is_none());
    }

    #[test]
    fn invitation_and_explicit_setup_identity_take_precedence_over_preferences() {
        let invite = invitation();
        let other = "dc310e12-b973-47c8-99e4-d71a40dc3c12";
        let saved = [saved_community(&invite.server, other)];
        let mut state = SetupState {
            invitation: Some(invite.clone()),
            saved_server_id: Some(other.into()),
            ..Default::default()
        };
        assert_eq!(
            state
                .expected_id_with_saved(&invite.server, &saved)
                .unwrap(),
            Some(invite.server_id.clone())
        );
        state.invitation = None;
        state.saved_server_id = Some(invite.server_id.clone());
        assert_eq!(
            state
                .expected_id_with_saved(&invite.server, &saved)
                .unwrap(),
            Some(invite.server_id)
        );
    }

    #[test]
    fn changed_invitation_address_cannot_fall_back_to_a_previously_known_server() {
        let invite = invitation();
        let other_address = "http://100.80.1.3:4580";
        let saved = [saved_community(other_address, &invite.server_id)];
        let state = SetupState {
            invitation: Some(invite),
            ..Default::default()
        };
        assert!(state
            .expected_id_with_saved(other_address, &saved)
            .unwrap_err()
            .contains("invitation address changed"));
    }

    #[test]
    fn malformed_saved_identity_fails_before_manual_authentication() {
        let state = SetupState::default();
        let saved = [saved_community("http://example.com", "not-a-server-uuid")];
        assert!(state
            .expected_id_with_saved("http://example.com", &saved)
            .is_err());
        assert!(state
            .expected_id_with_saved("http://another.example.com", &saved)
            .unwrap()
            .is_none());
    }

    #[test]
    fn stale_checks_and_edited_invitation_codes_are_discarded() {
        let mut state = SetupState {
            pending: Some(("http://localhost:4580".into(), 2)),
            ..Default::default()
        };
        state.accept(
            "http://localhost:4580",
            "http://localhost:4580",
            1,
            Err("old".into()),
        );
        state.accept(
            "http://localhost:4581",
            "http://localhost:4580",
            2,
            Err("old".into()),
        );
        assert!(state.result.is_none());
        state.accept(
            "http://localhost:4580",
            "http://localhost:4580",
            2,
            Err("current".into()),
        );
        assert!(matches!(state.result, Some(Err(_))));
        state.invitation = Some(
            Invitation::new(
                "http://localhost:4580",
                "4be44aef-8c98-4db6-a9d5-81c2eb311330",
                "test-code",
            )
            .unwrap(),
        );
        let mut code = "test-code".to_string();
        assert!(state.expected_id("http://localhost:4581").is_err());
        state.edited(&mut code);
        assert!(code.is_empty() && state.pending.is_none() && state.result.is_none());
        assert!(state
            .expected_id("http://localhost:4581")
            .unwrap()
            .is_none());
    }

    fn invitation() -> Invitation {
        Invitation::new(
            "http://100.80.1.2:4580",
            "4be44aef-8c98-4db6-a9d5-81c2eb311330",
            "test-code",
        )
        .unwrap()
    }

    fn summary(server_id: &str) -> ServerSummary {
        ServerSummary {
            ok: true,
            server_name: "Friends".into(),
            server_id: server_id.into(),
            protocol_version: relay_protocol::API_PROTOCOL_VERSION,
            rtc_enabled: true,
            setup_required: None,
        }
    }

    #[test]
    fn joining_checks_once_and_only_retries_after_an_explicit_action() {
        let invite = invitation();
        let mut state = SetupState {
            invitation: Some(invite.clone()),
            ..Default::default()
        };
        assert!(state.needs_automatic_check());
        assert!(state
            .begin_check(&invite.server, 1, false)
            .unwrap()
            .is_some());
        assert!(!state.needs_automatic_check());
        assert!(state
            .begin_check(&invite.server, 2, true)
            .unwrap()
            .is_none());
        // A disconnected sender is delivered as an ordinary, bounded failure.
        state.accept(&invite.server, &invite.server, 1, Err("Stopped".into()));
        assert!(!state.verified_for(&invite.server));
        assert!(!state.needs_automatic_check());
        assert!(state
            .begin_check(&invite.server, 3, false)
            .unwrap()
            .is_none());
        assert!(state
            .begin_check(&invite.server, 4, true)
            .unwrap()
            .is_some());
        state.accept(
            &invite.server,
            &invite.server,
            4,
            Ok(summary(&invite.server_id)),
        );
        assert!(state.verified_for(&invite.server));
        assert!(!state.needs_automatic_check());
    }

    #[test]
    fn a_new_invitation_cannot_accept_a_previous_check_or_wrong_server() {
        let first = invitation();
        let mut state = SetupState {
            invitation: Some(first.clone()),
            ..Default::default()
        };
        state.begin_check(&first.server, 1, false).unwrap();
        state.edited(&mut String::new());
        let mut second = first.clone();
        second.server_id = "dc310e12-b973-47c8-99e4-d71a40dc3c12".into();
        state.invitation = Some(second.clone());
        state.begin_check(&second.server, 2, false).unwrap();
        state.accept(
            &first.server,
            &first.server,
            1,
            Ok(summary(&first.server_id)),
        );
        assert!(state.result.is_none());
        assert!(!state.verified_for(&second.server));
        state.accept(
            &second.server,
            &second.server,
            2,
            Ok(summary(&first.server_id)),
        );
        assert!(matches!(state.result, Some(Err(_))));
        assert!(!state.verified_for(&second.server));
        assert!(state
            .begin_check("http://100.80.1.3:4580", 3, true)
            .is_err());
    }

    #[test]
    fn existing_access_needs_a_real_matching_server_result_not_a_browser_click() {
        let invite = invitation()
            .with_network_share("https://login.tailscale.com/admin/invite/example_share")
            .unwrap();
        let mut state = SetupState {
            invitation: Some(invite.clone()),
            ..Default::default()
        };
        // Possessing/opening the share link cannot manufacture a health result.
        assert!(!state.verified_for(&invite.server));
        state.begin_check(&invite.server, 1, false).unwrap();
        assert!(!state.verified_for(&invite.server));
        state.accept(
            &invite.server,
            &invite.server,
            1,
            Ok(summary(&invite.server_id)),
        );
        // Returning friends may proceed without repeating browser sharing.
        assert!(state.verified_for(&invite.server));
        assert!(!state.verified_for("http://100.80.1.3:4580"));
    }

    #[test]
    fn saved_server_reconnect_keeps_its_identity_and_checked_address() {
        let invite = invitation();
        let mut state = SetupState {
            saved_server_id: Some(invite.server_id.clone()),
            ..Default::default()
        };
        assert_eq!(
            state.expected_id(&invite.server).unwrap(),
            Some(invite.server_id.clone())
        );
        state.begin_check(&invite.server, 1, false).unwrap();
        state.accept(
            &invite.server,
            &invite.server,
            1,
            Ok(summary("dc310e12-b973-47c8-99e4-d71a40dc3c12")),
        );
        assert!(!state.verified_for(&invite.server));
        state.begin_check(&invite.server, 2, true).unwrap();
        state.accept(
            &invite.server,
            &invite.server,
            2,
            Ok(summary(&invite.server_id)),
        );
        assert!(state.verified_for(&invite.server));
        assert!(!state.verified_for("http://100.80.1.3:4580"));
        state.edited(&mut String::new());
        assert!(state.saved_server_id.is_none());
        assert!(!state.verified_for(&invite.server));
    }
}

#[cfg(test)]
mod discovery_pin_tests {
    use super::*;

    const KNOWN: &str = "4be44aef-8c98-4db6-a9d5-81c2eb311330";
    const OTHER: &str = "0e1a9f6c-1111-4222-8333-444455556666";

    fn found(ip: &str, id: &str) -> crate::lan_discovery::Found {
        crate::lan_discovery::Found {
            name: "Home".into(),
            server_id: id.into(),
            ip: ip.parse().unwrap(),
            port: 4580,
        }
    }

    fn saved(url: &str, id: &str) -> SavedCommunity {
        SavedCommunity {
            url: url.into(),
            server_id: id.into(),
            ..Default::default()
        }
    }

    #[test]
    fn a_discovery_answer_never_replaces_a_saved_server_identity() {
        let prefs = [saved("HTTP://192.168.1.20:4580/", KNOWN)];
        // A conflicting answer at the known address is refused and changes nothing.
        let mut flow = JoinFlow::default();
        flow.url = "typed-before".into();
        let error = flow
            .select_discovered(&prefs, &found("192.168.1.20", OTHER))
            .unwrap_err();
        assert!(error.contains("different server"), "{error}");
        assert_eq!(flow.url, "typed-before");
        assert!(flow.setup.saved_server_id.is_none());
        // The real selection path then the real expected-pin lookup: the saved id wins.
        let mut flow = JoinFlow::default();
        flow.select_discovered(&prefs, &found("192.168.1.20", KNOWN))
            .unwrap();
        assert_eq!(flow.url, "http://192.168.1.20:4580");
        assert_eq!(
            flow.setup
                .expected_id_with_saved(&flow.url, &prefs)
                .unwrap(),
            Some(KNOWN.to_string())
        );
        // An unknown address gets only the advertised id as the pin for the next check.
        let mut flow = JoinFlow::default();
        flow.select_discovered(&prefs, &found("192.168.1.99", OTHER))
            .unwrap();
        assert_eq!(
            flow.setup
                .expected_id_with_saved(&flow.url, &prefs)
                .unwrap(),
            Some(OTHER.to_string())
        );
        // Using the saved secure route: address and pin come from the saved record only.
        let secure = [saved("https://home.example", KNOWN)];
        let mut flow = JoinFlow::default();
        flow.select_saved_route(&secure, &found("192.168.1.20", KNOWN), true)
            .unwrap();
        assert_eq!(flow.url, "https://home.example");
        assert!(flow.setup.invitation.is_none());
        assert_eq!(
            flow.setup
                .expected_id_with_saved(&flow.url, &secure)
                .unwrap(),
            Some(KNOWN.to_string())
        );
        // No saved route for that id (or only plain HTTP): refused, nothing changes.
        let mut flow = JoinFlow::default();
        flow.url = "typed-before".into();
        for list in [
            vec![saved("https://home.example", OTHER)],
            vec![saved("http://192.168.1.20:4580", KNOWN)],
            vec![],
        ] {
            let error = flow
                .select_saved_route(&list, &found("192.168.1.20", KNOWN), true)
                .unwrap_err();
            assert!(error.contains("No secure address"), "{error}");
            assert_eq!(flow.url, "typed-before");
            assert!(flow.setup.saved_server_id.is_none());
        }
        // Even a spoofed id cannot override the pin persisted for the known address
        // through the manual path afterwards.
        let mut flow = JoinFlow::default();
        flow.url = "http://192.168.1.20:4580".into();
        assert_eq!(
            flow.setup
                .expected_id_with_saved(&flow.url, &prefs)
                .unwrap(),
            Some(KNOWN.to_string())
        );
    }
}

#[cfg(test)]
mod authority_flow_tests {
    use super::*;
    #[test]
    fn verified_move_changes_only_the_matching_form_and_keeps_invitation_and_credentials() {
        let (_, _, pin, _) = crate::host_route::tests::fixture();
        let mut flow = JoinFlow::default();
        flow.url = "https://source.example".into();
        flow.username = "owned-user".into();
        flow.password = "owned-password".into();
        flow.invite = "owned-invite".into();
        flow.setup.invitation = Some(
            Invitation::new(&flow.url, &pin.server_id, &flow.invite)
                .unwrap()
                .with_authority(Some(pin.clone()))
                .unwrap(),
        );
        let original = flow.url.clone();
        flow.setup.begin_check(&original, 7, true).unwrap();
        let summary = ServerSummary {
            ok: true,
            server_name: "Owned".into(),
            server_id: pin.server_id.clone(),
            protocol_version: relay_protocol::API_PROTOCOL_VERSION,
            rtc_enabled: true,
            setup_required: None,
        };
        flow.accept_checked(
            &original,
            6,
            Ok(summary.clone()),
            Some("https://wrong.example".into()),
        );
        assert_eq!(flow.url, original);
        assert!(flow.setup.pending.is_some());
        flow.accept_checked(
            &original,
            7,
            Ok(summary.clone()),
            Some("https://target.example".into()),
        );
        assert_eq!(flow.url, "https://target.example");
        assert!(flow.setup.verified_for(&flow.url));
        assert_eq!(flow.setup.invitation.as_ref().unwrap().authority, Some(pin));
        assert_eq!(
            (&*flow.username, &*flow.password, &*flow.invite),
            ("owned-user", "owned-password", "owned-invite")
        );
        flow.setup.edited(&mut flow.invite);
        flow.url = "https://edited.example".into();
        flow.accept_checked(
            &original,
            7,
            Ok(summary),
            Some("https://target.example".into()),
        );
        assert_eq!(flow.url, "https://edited.example");
        assert!(flow.setup.invitation.is_none());
    }
    #[test]
    fn verified_new_host_form_keeps_only_the_successors_private_route() {
        let (_, _, pin, _) = crate::host_route::tests::fixture();
        let next = crate::iroh_route::SavedIroh {
            endpoint_id: "d8".repeat(32), relay: None, token: "Z".repeat(43),
        };
        crate::iroh_route::register(&next);
        let mut flow = JoinFlow::default();
        flow.url = "https://source.example".into();
        flow.invite = "owned-invite".into();
        let mut invitation = Invitation::new(&flow.url, &pin.server_id, &flow.invite).unwrap()
            .with_authority(Some(pin.clone())).unwrap();
        invitation.iroh = Some(crate::invitation_routes::IrohRoute::new(&"d7".repeat(32), None, &"Y".repeat(43)).unwrap());
        invitation.alt.push("https://old-alternative.example".into());
        invitation.network_share = Some("old-share".into());
        flow.setup.invitation = Some(invitation);
        let source = flow.url.clone();
        flow.setup.begin_check(&source, 19, true).unwrap();
        flow.accept_checked(&source, 19, Ok(ServerSummary {
            ok: true, server_name: "Owned".into(), server_id: pin.server_id.clone(),
            protocol_version: relay_protocol::API_PROTOCOL_VERSION, rtc_enabled: true, setup_required: None,
        }), Some(next.address()));
        let found = flow.setup.invitation.as_ref().unwrap();
        assert!(flow.setup.verified_for(&next.address()));
        assert_eq!(found.authority.as_ref(), Some(&pin));
        assert_eq!(found.code, "owned-invite");
        assert!(found.alt.is_empty() && found.network_share.is_none());
        let current = found.iroh.as_ref().unwrap();
        assert_eq!((&current.endpoint_id, &current.token), (&next.endpoint_id, &next.token));
    }

}
