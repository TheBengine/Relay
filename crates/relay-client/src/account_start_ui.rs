//! One Relay account for friends and community sign-in.
use super::*;
use sha2::{Digest, Sha256};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
#[cfg(all(test, windows))]
#[path = "account_entry_native_fixture.rs"]
mod native_entry_fixture;

pub(super) struct Attempt {
    url: String,
    expected: Option<String>,
    request: u64,
    other: bool,
    people: bool,
    account: crate::central_account::Account,
    cancel: Arc<AtomicBool>,
    browser: Option<crate::central_account::Controller>,
    challenge: Option<String>,
}
impl Attempt {
    pub(super) fn is_people(&self) -> bool {
        self.people
    }
}
impl Drop for Attempt {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
    }
}

pub(super) struct State {
    pub(super) pending: Option<Attempt>,
    pub(super) home: bool,
    pub(super) bypass: bool,
}
impl Default for State {
    fn default() -> Self {
        Self {
            pending: None,
            home: true,
            bypass: false,
        }
    }
}

impl State {
    pub(super) fn waiting(&self) -> bool {
        self.pending
            .as_ref()
            .and_then(|attempt| attempt.browser.as_ref())
            .is_some_and(|browser| browser.waiting())
    }
}

fn challenge_matches(nonce: &str, url: &str, server: &str) -> bool {
    let prefix = format!(
        "relay-login-v1:{server}:{:x}:",
        Sha256::digest(url.trim_end_matches('/').as_bytes())
    );
    nonce.strip_prefix(&prefix).is_some_and(|r| {
        r.len() == 43
            && r.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    })
}

impl RelayApp {
    pub(super) fn account_front(&mut self, ctx: &egui::Context) -> bool {
        if self.capture_path.is_some() {
            return false;
        }
        let gate = self.central_account.account.is_none()
            && !self.account_start.bypass
            && (self.central_account.provider.is_some() || self.session.is_none());
        let home = self.central_account.account.is_some() && self.account_start.home;
        if !gate && !home {
            return false;
        }
        #[cfg(windows)]
        if gate
            && self.central_account.provider.is_some()
            && self.central_account.error.is_none()
            && !self.central_account.waiting()
        {
            if let Some(surface) = self
                .account_webview
                .as_mut()
                .filter(|surface| !surface.started)
            {
                surface.started = true;
                self.central_account.start(ctx.clone());
            }
        }
        self.people_call_notice(ctx);
        entry_surface::page(ctx, "relay-account-entry", |ui| {
            let width = ui.available_width();
            if gate {
                ui.heading("Welcome back");
                ui.add_space(8.0);
                ui.label(
                    RichText::new("One account for your friends and servers.").color(c_muted()),
                );
                ui.add_space(28.0);
                if self.central_account.provider.is_some() {
                    if self.central_account.waiting() {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(if self.central_account.browser_ready() {
                                "Continue in your browser, then come back to Relay."
                            } else {
                                "Getting sign-in ready…"
                            });
                        });
                        ui.add_space(20.0);
                        if ui
                            .add_sized([width, 44.0], secondary("Cancel sign-in"))
                            .clicked()
                        {
                            self.central_account.disconnect();
                        }
                    } else {
                        ui.label(RichText::new("Sign in to your Relay account.").color(c_muted()));
                        ui.add_space(16.0);
                        if ui.add_sized([width, 44.0], primary("Sign in")).clicked() {
                            #[cfg(windows)]
                            if let Some(surface) = &mut self.account_webview {
                                surface.browser_only = false;
                            }
                            self.central_account.start(ctx.clone());
                        }
                        ui.add_space(24.0);
                        ui.horizontal_wrapped(|ui| {
                            ui.label(RichText::new("New to Relay?").color(c_muted()));
                            if entry_surface::text_action(ui, "Create account").clicked() {
                                #[cfg(windows)]
                                if let Some(surface) = &mut self.account_webview {
                                    surface.browser_only = false;
                                }
                                self.central_account.start_registration(ctx.clone());
                            }
                        });
                    }
                    #[cfg(windows)]
                    if !self.central_account.waiting()
                        && entry_surface::text_action(ui, "Sign in with browser instead").clicked()
                    {
                        if let Some(surface) = &mut self.account_webview {
                            surface.browser_only = true;
                        }
                        self.central_account.start(ctx.clone());
                    }
                } else {
                    ui.label(RichText::new("Relay account sign-in is not configured in this copy. Install the latest version of Relay.").color(c_muted()));
                }
                if let Some(error) = self.central_account.error {
                    ui.add_space(16.0);
                    egui::Frame::none()
                        .fill(c_rail())
                        .stroke(Stroke::new(1.0_f32, c_bad()))
                        .rounding(7.0)
                        .inner_margin(Margin::same(12.0))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.add(
                                egui::Label::new(RichText::new(error).color(c_bad())).wrap(true),
                            );
                        });
                }
            } else {
                let handle = self
                    .central_account
                    .account
                    .as_ref()
                    .map(|a| a.handle.clone())
                    .unwrap_or_default();
                ui.heading(format!("Welcome, @{handle}"));
                ui.add_space(8.0);
                ui.label(
                    RichText::new(
                        "Your account is ready. Choose a server or make a place for your friends.",
                    )
                    .color(c_muted()),
                );
                ui.add_space(24.0);
                if ui
                    .add(primary("Friends and messages").min_size(Vec2::new(width, 44.0)))
                    .clicked()
                {
                    self.open_people();
                }
                ui.label(RichText::new("Message and call friends with your Relay account. No shared server needed.").color(c_muted()));
                if self.account_start.pending.is_some() {
                    ui.spinner();
                    ui.label("Connecting your friends…");
                    if ui.button("Cancel").clicked() {
                        self.account_start.pending = None;
                    }
                }
                if let Some(error) = &self.error {
                    ui.label(RichText::new(error).color(c_bad()));
                }
                ui.add_space(16.0);
                let people_url = self.people_url();
                for saved in self
                    .prefs
                    .communities
                    .clone()
                    .into_iter()
                    .filter(|s| Some(&s.url) != people_url.as_ref())
                {
                    if ui
                        .add(
                            secondary(if saved.name.is_empty() {
                                &saved.url
                            } else {
                                &saved.name
                            })
                            .min_size(Vec2::new(width, 44.0)),
                        )
                        .clicked()
                    {
                        self.login.url = saved.url;
                        self.login.registering = false;
                        self.login.setup = Default::default();
                        self.hosting_setup.entry = host_setup_ui::LoginEntry::Account;
                        self.hosting_setup.new_host = false;
                        self.account_start.home = false;
                        self.begin_account_login(false);
                    }
                }
                ui.add_space(16.0);
                if ui
                    .add(primary("Create a server").min_size(Vec2::new(width, 44.0)))
                    .clicked()
                {
                    self.account_start.home = false;
                    self.hosting_setup.new_host = true;
                    self.hosting_setup.local_ready = false;
                    self.hosting_setup.resume_share = true;
                    self.hosting_setup.entry = host_setup_ui::LoginEntry::Account;
                    self.host_local();
                }
                if ui
                    .add(secondary("Join with an invitation").min_size(Vec2::new(width, 44.0)))
                    .clicked()
                {
                    self.account_start.home = false;
                    self.hosting_setup.entry = host_setup_ui::LoginEntry::Join;
                }
                if self.session.is_some() && ui.button("Return to my server").clicked() {
                    self.account_start.home = false;
                }
                if self.session.is_some() && ui.button("Friends on my servers").clicked() {
                    self.admin_ui.friend_all_servers = true;
                    self.selected_nav = false;
                    self.home_page = Some(SettingsTab::Friends);
                    self.account_start.home = false;
                }
                ui.add_space(24.0);
                ui.separator();
                ui.add_space(18.0);
                if let Some(service) = &self.central_account.provider {
                    ui.hyperlink_to("Manage my Relay account", service.management_url());
                }
                if entry_surface::text_action(ui, "Sign out of Relay").clicked() {
                    self.sign_out_relay();
                }
            }
        });
        true
    }

    pub(super) fn sign_out_relay(&mut self) {
        self.community_friends.clear();
        self.people_ui = Default::default();
        #[cfg(windows)]
        if let Some(surface) = &mut self.account_webview {
            surface.reset();
        }
        self.account_start.pending = None;
        self.save_preferences();
        self.stop_media();
        let mut urls: Vec<_> = self.communities.iter().map(|c| c.url.clone()).collect();
        if self.session.is_some() {
            urls.push(self.login.url.clone());
        }
        urls.sort();
        urls.dedup();
        for url in urls {
            self.send(Command::ForgetCommunity { url });
        }
        self.central_account.disconnect();
        self.account_start = Default::default();
        self.account_link_ui = Default::default();
        self.settings_open = false;
        self.add_community_open = false;
    }

    pub(super) fn relay_server_account_form(&mut self, ui: &mut egui::Ui, other: bool) -> bool {
        let Some(account) = self.central_account.account.as_ref() else {
            return false;
        };
        if other {
            ui.heading("Join your friends");
        }
        ui.label(format!(
            "Continue as @{}. No new server password needed.",
            account.handle
        ));
        let destination = crate::community::account_destination(if other {
            &self.add_community.url
        } else {
            &self.login.url
        });
        if let Err(message) = &destination {
            ui.label(RichText::new(message).color(c_muted()));
        }
        if let Some(error) = &self.error {
            ui.label(RichText::new(error).color(c_bad()));
        }
        if self.account_start.pending.is_some() {
            ui.spinner();
            ui.label("Confirm your Relay account to open this server.");
            if ui.button("Cancel").clicked() {
                self.account_start.pending = None;
                self.busy = false;
            }
        } else if ui
            .add_enabled(
                !self.busy && destination.is_ok(),
                primary("Continue with my Relay account"),
            )
            .clicked()
        {
            self.begin_account_login(other);
        }
        true
    }

    pub(super) fn people_url(&self) -> Option<String> {
        self.central_account.provider.as_ref()?.people_url().ok()
    }

    pub(super) fn in_people(&self) -> bool {
        self.bootstrap
            .as_ref()
            .is_some_and(|b| b.capabilities.iter().any(|c| c == "people-v1"))
    }

    pub(super) fn open_people(&mut self) {
        if self.central_account.account.is_none() {
            self.account_start.home = true;
            return;
        }
        if self.voice_channel.is_some() || self.joining_voice.is_some() {
            self.notice = Some((
                "Finish your current call before opening Friends.".into(),
                Instant::now(),
            ));
            return;
        }
        if self.account_start.pending.is_some() {
            return;
        }
        let Some(url) = self.people_url() else {
            self.error = Some("The Friends service address has not been configured.".into());
            return;
        };
        if self.in_people() && self.login.url == url {
            self.account_start.home = false;
            self.selected_nav = false;
            self.home_page = Some(SettingsTab::Friends);
            self.admin_ui.friend_all_servers = false;
            return;
        }
        self.account_start.home = true;
        let previous_login = std::mem::take(&mut self.login);
        self.login.url = url;
        self.login.registering = true;
        self.login.setup = Default::default();
        self.hosting_setup.new_host = false;
        self.begin_account_login(false);
        if let Some(attempt) = &mut self.account_start.pending {
            attempt.people = true;
        }
        self.login = previous_login;
    }

    fn begin_account_login(&mut self, other: bool) {
        if self.account_start.pending.is_some() {
            return;
        }
        let Some(account) = self.central_account.account.clone() else {
            return;
        };
        let flow = if other {
            &self.add_community
        } else {
            &self.login
        };
        let url = match crate::community::normalize_url(&flow.url) {
            Ok(u) => u,
            Err(e) => {
                self.error = Some(e);
                return;
            }
        };
        let expected = match flow
            .setup
            .expected_id_with_saved(&url, &self.prefs.communities)
        {
            Ok(e) => e,
            Err(e) => {
                self.error = Some(e);
                return;
            }
        };
        let invite = zeroize::Zeroizing::new(flow.invite.clone());
        let register = flow.registering;
        let initial_protection = (!other && self.hosting_setup.new_host && register)
            .then_some(self.hosting_setup.creation_protection);
        let request = self.next_request();
        self.error = None;
        self.account_start.pending = Some(Attempt {
            url: url.clone(),
            expected: expected.clone(),
            request,
            other,
            people: false,
            account,
            cancel: Arc::new(AtomicBool::new(false)),
            browser: None,
            challenge: None,
        });
        self.send(Command::RememberSession {
            enabled: self.prefs.remember_session,
        });
        self.send(Command::AccountLoginBegin {
            url,
            expected_server_id: expected,
            invite,
            register,
            initial_protection,
            request_id: request,
        });
    }

    pub(super) fn account_login_failed(&mut self, request: u64, error: &str) -> bool {
        if self
            .account_start
            .pending
            .as_ref()
            .is_some_and(|a| a.request == request)
        {
            self.account_start.pending = None;
            self.error = Some(error.into());
            self.busy = false;
            true
        } else {
            false
        }
    }

    pub(super) fn account_login_current(&self, request: u64, url: &str) -> bool {
        self.account_start.pending.as_ref().is_some_and(|a| {
            a.request == request
                && a.url == url
                && !a.cancel.load(Ordering::Acquire)
                && self
                    .central_account
                    .account
                    .as_ref()
                    .is_some_and(|current| {
                        current.issuer == a.account.issuer && current.subject == a.account.subject
                    })
        })
    }

    pub(super) fn account_login_challenge(
        &mut self,
        ctx: &egui::Context,
        url: &str,
        data: serde_json::Value,
        request: u64,
    ) {
        let Some(attempt) = self
            .account_start
            .pending
            .as_mut()
            .filter(|a| a.request == request && a.url == url)
        else {
            return;
        };
        let provider =
            serde_json::from_value::<relay_protocol::AccountProvider>(data["provider"].clone())
                .ok();
        let server = data["server_id"]
            .as_str()
            .filter(|s| uuid::Uuid::parse_str(s).is_ok());
        let nonce = data["nonce"].as_str();
        let challenge = data["challenge_id"]
            .as_str()
            .filter(|s| uuid::Uuid::parse_str(s).is_ok());
        if let (Some(provider), Some(server), Some(nonce), Some(challenge), Some(service)) = (
            provider,
            server,
            nonce,
            challenge,
            self.central_account.provider.clone(),
        ) {
            if (!attempt.people || data["people_service"].as_bool() == Some(true))
                && service.matches(&provider)
                && attempt.expected.as_ref().is_none_or(|e| e == server)
                && challenge_matches(nonce, url, server)
            {
                let mut browser = crate::central_account::Controller::default();
                browser.provider = Some(service);
                browser.start_community(ctx.clone(), nonce.into(), attempt.account.clone());
                attempt.expected = Some(server.into());
                attempt.challenge = Some(challenge.into());
                attempt.browser = Some(browser);
                return;
            }
        }
        self.account_start.pending = None;
        self.error = Some(
            "The account service or server identity changed. Check the invitation and try again."
                .into(),
        );
    }

    pub(super) fn account_login_tick(&mut self, ctx: &egui::Context) {
        let Some(a) = self.account_start.pending.as_mut() else {
            return;
        };
        let flow = if a.other {
            &self.add_community
        } else {
            &self.login
        };
        if (a.other && !self.add_community_open)
            || (!a.people
                && crate::community::normalize_url(&flow.url).as_deref() != Ok(a.url.as_str()))
            || self.central_account.account.as_ref().is_none_or(|current| {
                current.issuer != a.account.issuer || current.subject != a.account.subject
            })
        {
            self.account_start.pending = None;
            return;
        }
        let Some(browser) = a.browser.as_mut() else {
            return;
        };
        if let Some(url) = browser.poll() {
            #[cfg(windows)]
            if let Some(surface) = self
                .account_webview
                .as_mut()
                .filter(|surface| !surface.browser_only)
            {
                if let Err(error) = surface.open(url.clone(), ctx) {
                    surface.hide();
                    self.error = Some(error.into());
                    self.account_start.pending = None;
                    return;
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
        if let Some(error) = browser.error {
            self.error = Some(error.into());
            self.account_start.pending = None;
            return;
        }
        if let Some(proof) = browser.take_link_proof() {
            let command = Command::AccountLoginFinish {
                url: a.url.clone(),
                expected_server_id: a.expected.clone().unwrap_or_default(),
                challenge_id: a.challenge.take().unwrap_or_default(),
                proof: proof.id_token,
                cancel: a.cancel.clone(),
                request_id: a.request,
            };
            a.browser = None;
            self.send(command);
        } else {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn server_login_challenge_binds_destination_and_server() {
        let url = "https://community.invalid";
        let nonce = format!(
            "relay-login-v1:owned:{:x}:{}",
            Sha256::digest(url.as_bytes()),
            "a".repeat(43)
        );
        assert!(challenge_matches(&nonce, url, "owned"));
        assert!(!challenge_matches(&nonce, "https://other.invalid", "owned"));
        assert!(!challenge_matches(&nonce, url, "replaced"));
        assert!(!challenge_matches(&(nonce + "x"), url, "owned"));
    }
}
