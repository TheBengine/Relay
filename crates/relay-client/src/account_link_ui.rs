//! Account linking has its own explicit browser approval and origin-bound state.
use super::*;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use zeroize::{Zeroize, Zeroizing};

#[cfg(all(test, windows))]
#[path = "account_link_native_fixture.rs"]
mod native_fixture;

#[derive(Default)]
pub(super) struct State {
    origin: Option<CommandOrigin>,
    pending: Option<String>,
    tried: bool,
    provider: Option<relay_protocol::AccountProvider>,
    linked: Option<relay_protocol::LinkedIdentity>,
    editing: bool,
    password: Zeroizing<String>,
    code: Zeroizing<String>,
    need_factor: bool,
    challenge: Option<String>,
    browser: Option<crate::central_account::Controller>,
    start: Option<(String, crate::central_account::Account)>,
    expected: Option<(String, String)>,
    error: Option<String>,
}

impl State {
    pub(super) fn waiting(&self) -> bool {
        self.browser
            .as_ref()
            .is_some_and(|browser| browser.waiting())
    }
    pub(super) fn cancel_signin(&mut self) {
        self.browser = None;
        self.start = None;
        self.pending = None;
        self.challenge = None;
        self.expected = None;
    }
}

fn canonical(value: &str) -> Option<String> {
    let url = url::Url::parse(value).ok()?;
    (url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none())
    .then(|| url.as_str().trim_end_matches('/').to_owned())
}
fn challenge_matches(nonce: &str, origin: &CommandOrigin) -> bool {
    let Some(url) = canonical(&origin.url) else {
        return false;
    };
    let hash = format!("{:x}", Sha256::digest(url.as_bytes()));
    let prefix = format!(
        "relay-link-v1:{}:{}:{}:",
        origin.server_id, origin.user_id, hash
    );
    nonce.strip_prefix(&prefix).is_some_and(|random| {
        random.len() == 43
            && random
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    })
}

pub(super) fn linked_name<'a>(
    bootstrap: &'a Bootstrap,
    service: Option<&crate::central_account::Provider>,
    user: &str,
) -> Option<&'a relay_protocol::LinkedIdentity> {
    let provider = bootstrap.account_provider.as_ref()?;
    if !service.is_some_and(|p| p.matches(provider)) {
        return None;
    }
    bootstrap.linked_identities.iter().find(|l| {
        l.user_id == user
            && l.issuer == provider.issuer
            && (3..=24).contains(&l.handle.len())
            && l.handle.as_bytes()[0].is_ascii_lowercase()
            && l.handle
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    })
}

impl RelayApp {
    fn account_link_request(
        &mut self,
        action: &str,
        proof: Option<crate::central_account::LinkProof>,
    ) {
        if self.account_link_ui.pending.is_some() {
            return;
        }
        if self.command_origin().as_ref() != self.account_link_ui.origin.as_ref() {
            return;
        }
        let generation = self.next_request();
        let key = format!("account-link:{action}:{generation}");
        if action == "begin" {
            let Some(account) = &self.central_account.account else {
                return;
            };
            self.account_link_ui.expected = Some((account.issuer.clone(), account.subject.clone()));
        }
        let body = match action {
            "status" => Value::Null,
            "link" => {
                let (Some(challenge), Some(proof)) = (self.account_link_ui.challenge.take(), proof)
                else {
                    return;
                };
                json!({"action":"link","challenge_id":challenge,"id_token":proof.id_token.as_str()})
            }
            _ => {
                json!({"action":action,"password":self.account_link_ui.password.as_str(),"code":self.account_link_ui.code.trim()})
            }
        };
        self.admin_request(
            if action == "status" {
                AdminMethod::Get
            } else {
                AdminMethod::Post
            },
            "/api/account-link".into(),
            body,
            key.clone(),
        );
        if self.admin_requests.values().any(|entry| entry == &key) {
            self.account_link_ui.pending = Some(key);
        }
        self.account_link_ui.password.zeroize();
        self.account_link_ui.code.zeroize();
        self.account_link_ui.tried = true;
        self.account_link_ui.error = None;
    }

    pub(super) fn account_link_failed(&mut self, key: &str, error: String) {
        if self.account_link_ui.pending.as_deref() != Some(key) {
            return;
        }
        let state = &mut self.account_link_ui;
        state.pending = None;
        state.browser = None;
        state.challenge = None;
        state.start = None;
        state.expected = None;
        state.need_factor = matches!(
            error.as_str(),
            relay_protocol::AUTHENTICATOR_REQUIRED | relay_protocol::AUTHENTICATOR_INVALID
        );
        state.error = Some(error);
        state.password.zeroize();
        state.code.zeroize();
    }

    pub(super) fn account_link_result(&mut self, key: &str, data: Value) {
        let Some(origin) = self.command_origin() else {
            return;
        };
        if self.account_link_ui.pending.as_deref() != Some(key)
            || self.account_link_ui.origin.as_ref() != Some(&origin)
        {
            return;
        }
        self.account_link_ui.pending = None;
        let provider: Option<relay_protocol::AccountProvider> =
            serde_json::from_value(data["provider"].clone())
                .ok()
                .flatten();
        if key.starts_with("account-link:begin:") {
            let expected = self.central_account.account.clone();
            let service = self.central_account.provider.clone();
            let nonce = data["nonce"]
                .as_str()
                .filter(|n| n.len() <= 512 && challenge_matches(n, &origin));
            let challenge = data["challenge_id"]
                .as_str()
                .filter(|c| uuid::Uuid::parse_str(c).is_ok());
            if let (Some(expected), Some(service), Some(provider), Some(nonce), Some(challenge)) =
                (expected, service, provider, nonce, challenge)
            {
                if self.account_link_ui.expected.as_ref()
                    == Some(&(expected.issuer.clone(), expected.subject.clone()))
                    && service.matches(&provider)
                    && canonical(&provider.community_url) == canonical(&origin.url)
                {
                    let mut browser = crate::central_account::Controller::default();
                    browser.provider = Some(service);
                    self.account_link_ui.start = Some((nonce.into(), expected));
                    self.account_link_ui.challenge = Some(challenge.into());
                    self.account_link_ui.browser = Some(browser);
                    return;
                }
            }
            self.account_link_ui.error = Some(
                "The linking service or community address changed. Refresh and try again.".into(),
            );
            return;
        }
        self.account_link_ui.provider = provider;
        self.account_link_ui.linked = serde_json::from_value::<
            Option<relay_protocol::LinkedIdentity>,
        >(data["linked"].clone())
        .ok()
        .flatten()
        .filter(|link| link.user_id == origin.user_id);
        if !key.starts_with("account-link:status:") {
            self.account_link_ui.editing = false;
            self.account_link_ui.need_factor = false;
            self.account_link_ui.expected = None;
            if let Some(bootstrap) = self.bootstrap.as_mut().map(Arc::make_mut) {
                bootstrap
                    .linked_identities
                    .retain(|link| link.user_id != origin.user_id);
                if let Some(link) = &self.account_link_ui.linked {
                    bootstrap.linked_identities.push(link.clone());
                }
            }
        }
    }

    pub(super) fn account_link_tick(&mut self, ctx: &egui::Context) {
        let visible = self.settings_open
            && self.settings_tab == SettingsTab::Account
            && self.session.is_some();
        if !visible || self.account_link_ui.origin.as_ref() != self.command_origin().as_ref() {
            self.account_link_ui = State::default();
            return;
        }
        let mut proof = None;
        if self.account_link_ui.expected.is_some()
            && self.account_link_ui.expected
                != self
                    .central_account
                    .account
                    .as_ref()
                    .map(|a| (a.issuer.clone(), a.subject.clone()))
        {
            self.account_link_ui.pending = None;
            self.account_link_ui.start = None;
            self.account_link_ui.browser = None;
            self.account_link_ui.challenge = None;
            self.account_link_ui.expected = None;
            self.account_link_ui.error =
                Some("Your Relay account changed. Start linking again.".into());
            return;
        }
        if let Some((nonce, expected)) = self.account_link_ui.start.take() {
            if let Some(browser) = &mut self.account_link_ui.browser {
                browser.start_link(ctx.clone(), nonce, expected);
            }
        }
        let mut failed = None;
        if let Some(browser) = &mut self.account_link_ui.browser {
            if let Some(url) = browser.poll() {
                #[cfg(windows)]
                if let Some(surface) = self
                    .account_webview
                    .as_mut()
                    .filter(|surface| !surface.browser_only)
                {
                    if let Err(error) = surface.open(url.clone(), ctx) {
                        surface.hide();
                        failed = Some(error.to_owned());
                    }
                } else {
                    ctx.open_url(egui::OpenUrl {
                        url: url.into(),
                        new_tab: true,
                    });
                }
                #[cfg(not(windows))]
                ctx.open_url(egui::OpenUrl {
                    url: url.into(),
                    new_tab: true,
                });
            }
            proof = browser.take_link_proof();
            if let Some(error) = browser.error {
                failed = Some(error.to_owned());
            }
            if browser.waiting() {
                ctx.request_repaint_after(Duration::from_millis(100));
            }
        }
        if let Some(error) = failed {
            self.account_link_ui.error = Some(error);
            self.account_link_ui.browser = None;
            self.account_link_ui.challenge = None;
            self.account_link_ui.expected = None;
        }
        if proof.is_some() {
            self.account_link_ui.browser = None;
            self.account_link_request("link", proof);
        }
    }

    pub(super) fn account_link_settings(&mut self, ui: &mut egui::Ui) {
        if self
            .bootstrap
            .as_ref()
            .is_some_and(|b| b.account_login_only)
        {
            ui.label("This server uses your Relay account. Your messages and encryption keys belong to your account on this server.");
            return;
        }
        if !self
            .bootstrap
            .as_ref()
            .is_some_and(|b| b.capabilities.iter().any(|s| s == "account-link-v1"))
        {
            return;
        }
        let origin = self.command_origin();
        if self.account_link_ui.origin != origin {
            self.account_link_ui = State {
                origin,
                ..Default::default()
            };
        }
        if !self.account_link_ui.tried {
            self.account_link_request("status", None);
        }
        if self.account_link_ui.provider.is_none()
            && self.account_link_ui.linked.is_none()
            && self.account_link_ui.error.is_none()
        {
            return;
        }
        settings_heading(
            ui,
            "Relay name in this community",
            "Linking is optional. Your local account and private chat keys stay separate.",
        );
        if let Some(link) = &self.account_link_ui.linked {
            ui.label(RichText::new(format!("Linked as @{}", link.handle)).strong());
            if !self.account_link_ui.editing
                && ui.add(secondary("Remove community link…")).clicked()
            {
                self.account_link_ui.editing = true;
            }
        } else if let Some(provider) = &self.account_link_ui.provider {
            let compatible = self
                .central_account
                .provider
                .as_ref()
                .is_some_and(|p| p.matches(provider));
            let same_address = self
                .command_origin()
                .is_some_and(|o| canonical(&o.url) == canonical(&provider.community_url));
            if self.central_account.provider.is_none() {
                ui.label("Relay accounts are not enabled in this app.");
            } else if !compatible {
                ui.label("This community uses a different Relay account service.");
            } else if !same_address {
                ui.label(format!(
                    "Connect using {} to link your Relay name.",
                    provider.community_url
                ));
            } else if self.central_account.account.is_none() {
                ui.label("Sign in to your Relay account above to show its unique name here.");
            } else if !self.account_link_ui.editing
                && ui.add(secondary("Show my Relay name here…")).clicked()
            {
                self.account_link_ui.editing = true;
            }
        }
        let browser_waiting = self
            .account_link_ui
            .browser
            .as_ref()
            .is_some_and(|b| b.waiting());
        let pending = self.account_link_ui.pending.is_some();
        if browser_waiting {
            ui.label("Confirm the same Relay account in your browser to finish linking.");
        } else if self.account_link_ui.editing && !pending {
            authenticator_ui::secret_field(
                ui,
                "Community password",
                &mut self.account_link_ui.password,
                256,
            );
            egui::CollapsingHeader::new("Authenticator or recovery code")
                .id_source(("link-factor", self.account_link_ui.need_factor))
                .default_open(self.account_link_ui.need_factor)
                .show(ui, |ui| {
                    authenticator_ui::secret_field(
                        ui,
                        "Only if enabled on this community",
                        &mut self.account_link_ui.code,
                        64,
                    )
                });
            let unlink = self.account_link_ui.linked.is_some();
            if ui
                .add_enabled(
                    !self.account_link_ui.password.is_empty(),
                    primary(if unlink {
                        "Remove link"
                    } else {
                        "Confirm and open browser"
                    }),
                )
                .clicked()
            {
                self.account_link_request(if unlink { "unlink" } else { "begin" }, None);
            }
        } else if pending {
            ui.label("Checking account link…");
        }
        if (self.account_link_ui.editing || browser_waiting)
            && ui
                .add_enabled(!pending, secondary("Cancel linking"))
                .clicked()
        {
            let origin = self.account_link_ui.origin.clone();
            let provider = self.account_link_ui.provider.clone();
            let linked = self.account_link_ui.linked.clone();
            self.account_link_ui = State {
                origin,
                provider,
                linked,
                tried: true,
                ..Default::default()
            };
        }
        if let Some(error) = &self.account_link_ui.error {
            ui.label(RichText::new(error).color(c_bad()));
        }
        if ui
            .add_enabled(!pending && !browser_waiting, secondary("Refresh link"))
            .clicked()
        {
            self.account_link_request("status", None);
        }
        ui.add_space(18.0);
    }

    pub(super) fn linked_account_label(&self, ui: &mut egui::Ui, user: &str) {
        let Some(b) = self.bootstrap.as_ref() else {
            return;
        };
        if let Some(link) = linked_name(b, self.central_account.provider.as_ref(), user) {
            ui.label(RichText::new(format!("Linked Relay name · @{}",link.handle)).color(c_accent()))
                .on_hover_text("Account ownership confirmed by this community. Private chat device verification is separate.");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identity_challenge_is_bound_to_exact_actual_community_and_account() {
        let origin = CommandOrigin {
            url: "https://community.example.test".into(),
            server_id: "server-a".into(),
            user_id: "user-a".into(),
        };
        let hash = format!("{:x}", Sha256::digest(origin.url.as_bytes()));
        let nonce = format!("relay-link-v1:server-a:user-a:{hash}:{}", "a".repeat(43));
        assert!(challenge_matches(&nonce, &origin));
        for other in [
            CommandOrigin {
                url: "https://elsewhere.example.test".into(),
                ..origin.clone()
            },
            CommandOrigin {
                user_id: "other".into(),
                ..origin.clone()
            },
            CommandOrigin {
                server_id: "other".into(),
                ..origin.clone()
            },
        ] {
            assert!(!challenge_matches(&nonce, &other));
        }
        assert!(!challenge_matches(&(nonce + "/"), &origin));
    }
}
