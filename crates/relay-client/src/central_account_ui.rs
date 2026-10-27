//! Shared-account sign-in and the explicit choice to keep or forget sessions.
use super::*;

impl RelayApp {
    pub(super) fn begin_account_cleanup(&mut self) {
        self.staged_uploads.clear();
        self.account_start.pending = None;
        self.account_link_ui = Default::default();
        self.community_friends.clear();
        self.people_ui = Default::default();
        self.stop_media();
        let request_id = self.next_request();
        self.account_start = Default::default();
        self.account_start.clearing_previous_account = Some(request_id);
        self.send(Command::ForgetAccountSessions {
            urls: self.account_session_urls(),
            request_id,
        });
    }

    pub(super) fn account_sessions_forgotten(
        &mut self,
        request_id: u64,
        result: Result<(), String>,
    ) {
        let identity = self
            .central_account
            .account
            .as_ref()
            .map(|account| (account.issuer.clone(), account.subject.clone()));
        if self.account_start.finish_account_cleanup(
            request_id,
            result,
            identity,
            &mut self.prefs.last_relay_identity,
        ) {
            self.save_preferences();
        }
    }

    pub(super) fn account_session_urls(&self) -> Vec<String> {
        let mut urls: Vec<_> = self
            .prefs
            .communities
            .iter()
            .map(|saved| saved.url.clone())
            .chain(self.communities.iter().map(|live| live.url.clone()))
            .chain(self.people_url())
            .collect();
        if self.session.is_some() {
            urls.push(self.login.url.clone());
        }
        urls.sort();
        urls.dedup();
        urls
    }

    pub(super) fn stay_signed_in_control(&mut self, ui: &mut egui::Ui) {
        if ui
            .checkbox(&mut self.prefs.stay_signed_in, "Stay signed in on this PC")
            .changed()
        {
            if let Err(error) = self.central_account.set_remember(self.prefs.stay_signed_in) {
                self.central_account.warning = Some(error);
            }
            self.send(Command::RememberAccountSessions {
                enabled: self.prefs.stay_signed_in,
                urls: self.account_session_urls(),
            });
            self.save_preferences();
        }
        crate::settings_ui::description(ui,
            "Remember your Relay account and connected servers in Windows' protected credential storage. Sign out to remove saved logins.");
    }

    pub(super) fn central_account_tick(&mut self, ctx: &egui::Context) {
        let was_signed_in = self.central_account.account.is_some();
        if let Some(url) = self.central_account.poll() {
            #[cfg(windows)]
            let in_app = self
                .account_webview
                .as_mut()
                .filter(|surface| !surface.browser_only);
            #[cfg(windows)]
            if let Some(surface) = in_app {
                if let Err(error) = surface.open(url.clone(), ctx) {
                    surface.hide();
                    self.central_account.disconnect();
                    self.central_account.error = Some(error);
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
        let changed = self.central_account.take_account_change();
        if self
            .central_account
            .account
            .as_ref()
            .is_some_and(|account| {
                changed
                    || (!was_signed_in
                        && account_start_ui::identity_needs_cleanup(
                            &self.prefs.last_relay_identity,
                            account,
                        ))
            })
        {
            self.begin_account_cleanup();
        }
        if !was_signed_in {
            if let Some(account) = &self.central_account.account {
                self.prefs.profile_name = account.handle.clone();
                self.account_start.home = true;
                self.account_start.bypass = false;
                self.save_preferences();
            }
        }
        // Refresh results and expiry remain responsive while the rest of the
        // application is idle. No repaint-driven HTTP request is created.
        if self.central_account.waiting() {
            ctx.request_repaint_after(Duration::from_millis(100));
        } else if self.central_account.account.is_some() {
            ctx.request_repaint_after(Duration::from_secs(1));
        }
    }

    #[cfg(windows)]
    pub(super) fn account_surface_tick(&mut self, ctx: &egui::Context) {
        let waiting = self.central_account.waiting()
            || self.account_start.waiting()
            || self.account_link_ui.waiting();
        let Some(surface) = &mut self.account_webview else {
            return;
        };
        if !waiting {
            surface.hide();
        }
        match surface.take_request() {
            Some(crate::account_webview::Request::Browser(url)) if waiting => {
                surface.hide();
                ctx.open_url(egui::OpenUrl {
                    url: url.into(),
                    new_tab: true,
                });
            }
            Some(crate::account_webview::Request::Blocked) if waiting => {
                let shared_signin = self.central_account.waiting();
                self.cancel_account_surface();
                let error = "That page cannot open inside Relay. Try signing in with your browser.";
                if shared_signin {
                    self.central_account.error = Some(error);
                } else {
                    self.error = Some(error.into());
                }
            }
            _ => {}
        }
    }

    #[cfg(windows)]
    pub(super) fn cancel_account_surface(&mut self) {
        if self.central_account.waiting() {
            self.central_account.disconnect();
        }
        self.account_start.pending = None;
        self.account_link_ui.cancel_signin();
        if let Some(surface) = &mut self.account_webview {
            surface.reset();
        }
    }

    #[cfg(windows)]
    pub(super) fn account_surface_view(&mut self, ctx: &egui::Context) -> bool {
        if !self
            .account_webview
            .as_ref()
            .is_some_and(|surface| surface.visible())
        {
            return false;
        }
        let mut cancel = false;
        let mut browser = false;
        egui::TopBottomPanel::bottom("account-page-actions")
            .frame(
                egui::Frame::none()
                    .fill(c_rail())
                    .inner_margin(Margin::symmetric(16.0, 8.0)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    cancel = ui.add(secondary("← Back")).clicked();
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        browser = entry_surface::text_action(ui, "Sign in with browser instead")
                            .clicked();
                    });
                });
                self.stay_signed_in_control(ui);
            });
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(c_rail()))
            .show(ctx, |ui| {
                if let Some(surface) = &mut self.account_webview {
                    surface.resize(ui.max_rect(), ctx.pixels_per_point());
                }
            });
        if cancel {
            self.cancel_account_surface();
        } else if browser {
            if let Some(surface) = &mut self.account_webview {
                if let Some(url) = surface.browser_url() {
                    surface.hide();
                    ctx.open_url(egui::OpenUrl {
                        url: url.into(),
                        new_tab: true,
                    });
                }
            }
        }
        true
    }

    pub(super) fn central_account_settings(&mut self, ui: &mut egui::Ui) {
        let Some(provider) = self.central_account.provider.clone() else {
            if let Some(error) = self.central_account.error {
                settings_heading(ui, "Relay account", "Optional shared account");
                ui.label(RichText::new(error).color(c_bad()));
                ui.add_space(18.0);
            }
            return;
        };
        settings_heading(
            ui,
            "Relay account",
            "Your unique Relay name at the shared account service.",
        );
        ui.label(
            RichText::new(format!("Account service: {}", provider.service())).color(c_muted()),
        );
        self.stay_signed_in_control(ui);
        if let Some(account) = &self.central_account.account {
            let handle = format!("@{}", account.handle);
            ui.label(RichText::new(&handle).size(font_size(22.0)).strong());
            ui.hyperlink_to(
                "Manage password, authenticator and recovery",
                provider.management_url(),
            );
            ui.horizontal_wrapped(|ui| {
                if ui.button("Copy my Relay name").clicked() {
                    ui.output_mut(|out| out.copied_text = handle);
                }
                if ui.add(secondary("Sign out of Relay")).clicked() {
                    self.sign_out_relay();
                }
            });
        } else if self.central_account.waiting() {
            ui.label(if self.central_account.browser_ready() {
                "Continue in your browser. Relay will show your account here when sign-in finishes."
            } else if self.central_account.resuming() {
                "Restoring your saved login…"
            } else {
                "Contacting the Relay account service…"
            });
            if ui.add(secondary("Cancel sign-in")).clicked() {
                self.central_account.disconnect();
            }
        } else if ui.add(primary("Sign in to Relay account")).clicked() {
            self.central_account
                .start_remembered(ui.ctx().clone(), self.prefs.stay_signed_in);
        }
        if let Some(error) = self.central_account.error {
            ui.label(RichText::new(error).color(c_bad()));
        }
        if let Some(warning) = self.central_account.warning {
            ui.add(egui::Label::new(RichText::new(warning).color(c_muted())).wrap(true));
        }
        crate::settings_ui::description(ui, "Use this account to join participating servers. Existing server accounts can be linked explicitly; their messages and private chat keys stay with them.");
        ui.add_space(22.0);
        ui.separator();
        ui.add_space(12.0);
    }
}
