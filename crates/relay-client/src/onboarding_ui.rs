//! Short, progressive entry points for creating a server and joining friends.
use super::*;
use crate::connection_setup::FlowKind;
use host_setup_ui::LoginEntry;

pub(super) fn join_friends_window<'a>(ctx: &egui::Context, open: &'a mut bool) -> egui::Window<'a> {
    desktop_ui::dialog(ctx, "Join friends")
        .open(open)
        .collapsible(false)
        .resizable(false)
        .default_width(460.0)
}

impl RelayApp {
    pub(super) fn login_view(&mut self, ctx: &egui::Context) {
        entry_surface::page(ctx, "simple-login-scroll", |ui| {
            if self.needs_profile() {
                self.profile_step(ui);
            } else {
                match self.hosting_setup.entry {
                    LoginEntry::Welcome => self.welcome_choices(ui),
                    LoginEntry::Join => self.join_friend_form(ui),
                    LoginEntry::Account => {
                        if ui.add_enabled(!self.busy, secondary("← Back")).clicked() {
                            self.hosting_setup.entry = LoginEntry::Welcome;
                            self.account_start.home = self.central_account.account.is_some();
                            self.account_start.pending = None;
                            self.error = None;
                        }
                        ui.add_space(16.0);
                        if self.hosting_setup.new_host && !self.hosting_setup.local_ready {
                            ui.heading("Starting your server…");
                            ui.spinner();
                            if !self.busy && ui.add(primary("Try again")).clicked() {
                                self.host_local();
                            }
                        } else {
                            self.simple_account_form(ui);
                        }
                    }
                    LoginEntry::Advanced => {
                        if ui.add_enabled(!self.busy, secondary("← Back")).clicked() {
                            self.hosting_setup.entry = LoginEntry::Welcome;
                            self.account_start.home = self.central_account.account.is_some();
                            self.account_start.pending = None;
                            self.error = None;
                        }
                        ui.add_space(12.0);
                        self.advanced_login_form(ui);
                        ui.collapsing("Connection help", |ui| self.login_guide(ui));
                    }
                }
            }
            if let Some(error) = self
                .error
                .as_ref()
                .filter(|_| !self.account_feedback.drew_general(ui.ctx()))
            {
                ui.add_space(12.0);
                ui.label(RichText::new(error).color(c_bad()));
            }
            ui.add_space(24.0);
            ui.separator();
            ui.add_space(14.0);
            ui.label(
                RichText::new(if self.hosting_setup.entry == LoginEntry::Join {
                    "Your friend hosts the server. You just connect."
                } else {
                    "Your server, on your PC. Keep Relay open while friends use it."
                })
                .size(font_size(12.0))
                .color(c_muted()),
            );
            ui.add_space(8.0);
            ui.label(
                RichText::new(crate::build_metadata::current().label())
                    .size(font_size(12.0))
                    .color(c_muted()),
            );
        });
    }

    /// First start only: a person who has no saved server and no earlier account
    /// is asked for a name before anything else. Accounts live on servers, so
    /// this is not a login; it is the name every server will suggest.
    fn needs_profile(&self) -> bool {
        self.prefs.profile_name.is_empty()
            && self.prefs.username.is_empty()
            && self.prefs.communities.is_empty()
    }

    fn profile_step(&mut self, ui: &mut egui::Ui) {
        ui.heading("Welcome to Relay");
        ui.label("Choose a name. Friends see it on every server you join, and you can change it on a server later.");
        ui.add_space(12.0);
        let name = field(ui, "YOUR NAME", &mut self.login.username, false);
        let checked = profile_name(&self.login.username);
        if let (Err(problem), false) = (&checked, self.login.username.trim().is_empty()) {
            ui.label(RichText::new(*problem).color(c_bad()));
        }
        ui.add_space(12.0);
        let submit = ui
            .add_enabled(
                checked.is_ok(),
                primary("Continue").min_size(Vec2::new(ui.available_width(), 42.0)),
            )
            .clicked()
            || (checked.is_ok()
                && name.lost_focus()
                && ui.input(|i| i.key_pressed(egui::Key::Enter)));
        if let (true, Ok(chosen)) = (submit, checked) {
            self.prefs.profile_name = chosen.clone();
            self.login.username = chosen.clone();
            self.add_community.username = chosen;
            self.save_preferences();
        }
        ui.add_space(8.0);
        ui.label(
            RichText::new(
                "Your password is chosen per server, because each server keeps its own accounts.",
            )
            .size(font_size(12.0))
            .color(c_muted()),
        );
    }

    fn welcome_choices(&mut self, ui: &mut egui::Ui) {
        if self.hosting_setup.choosing_creation {
            self.creation_choices(ui);
            return;
        }
        if self.prefs.profile_name.is_empty() {
            ui.heading("A place for you and your friends");
        } else {
            ui.heading(format!("Hi, {}", self.prefs.profile_name));
        }
        ui.label("Add a server to get started. You can add more later with the plus button.");
        ui.add_space(20.0);
        if ui
            .add_enabled(
                !self.busy,
                primary("Create a server").min_size(Vec2::new(ui.available_width(), 48.0)),
            )
            .clicked()
        {
            self.hosting_setup
                .begin_creation(crate::iroh_route::available());
            self.error = None;
        }
        ui.label(
            RichText::new("You host it. Choose its protection and connection, then Relay helps you invite friends.")
                .color(c_muted()),
        );
        ui.add_space(16.0);
        if ui
            .add_enabled(
                !self.busy,
                secondary("Join friends").min_size(Vec2::new(ui.available_width(), 48.0)),
            )
            .clicked()
        {
            self.hosting_setup.entry = LoginEntry::Join;
            self.hosting_setup.new_host = false;
            self.hosting_setup.resume_share = false;
            self.login.setup.clipboard.rearm();
            self.error = None;
        }
        ui.label(
            RichText::new("Paste their invitation. You do not need to start a server.")
                .color(c_muted()),
        );
        ui.add_space(16.0);
        if ui
            .add_enabled(
                !self.busy,
                secondary("Connect with a server address")
                    .min_size(Vec2::new(ui.available_width(), 48.0)),
            )
            .clicked()
        {
            self.hosting_setup.entry = LoginEntry::Advanced;
            self.hosting_setup.new_host = false;
        }
        ui.label(
            RichText::new(
                "For a server on your home network, for example one you run on another computer.",
            )
            .color(c_muted()),
        );
        if !self.prefs.username.is_empty() {
            ui.add_space(20.0);
            if ui
                .add_enabled(!self.busy, secondary("Sign in again"))
                .clicked()
            {
                self.hosting_setup.entry = LoginEntry::Account;
                self.hosting_setup.new_host = false;
                self.login.registering = false;
                self.error = None;
            }
        }
    }

    fn join_friend_form(&mut self, ui: &mut egui::Ui) {
        if ui.add_enabled(!self.busy, secondary("← Back")).clicked() {
            self.hosting_setup.entry = LoginEntry::Welcome;
            self.login.setup.clipboard.settle();
            self.error = None;
        }
        ui.add_space(16.0);
        ui.heading("Join your friends");
        self.join_steps(ui, FlowKind::Login);
        if !self.friend_invitation_entry(ui, FlowKind::Login) {
            return;
        }
        if !self.friend_server_step(ui, FlowKind::Login) {
            return;
        }
        self.simple_account_form(ui);
    }

    /// "1 Invitation → 2 Connect → 3 Account" above the join form.
    fn join_steps(&self, ui: &mut egui::Ui, kind: FlowKind) {
        let (setup, address) = if kind == FlowKind::AddCommunity {
            (&self.add_community.setup, &self.add_community.url)
        } else {
            (&self.login.setup, &self.login.url)
        };
        let step = host_setup_ui::join_step(
            setup.invitation.is_some(),
            setup.routing.is_some() || setup.refused.is_some(),
            setup.invitation.is_some() && setup.verified_for(address),
        );
        ui.add_space(4.0);
        host_setup_ui::step_line(ui, &["Invitation", "Connect", "Account"], step);
        ui.add_space(10.0);
    }

    /// Shared by first-time login and joining another server while signed in.
    fn friend_invitation_entry(&mut self, ui: &mut egui::Ui, kind: FlowKind) -> bool {
        // Measuring the routes of a pasted invitation, or a refused route.
        if self.routing_state(ui, kind) {
            return false;
        }
        let state = if kind == FlowKind::AddCommunity {
            &mut self.add_community.setup
        } else {
            &mut self.login.setup
        };
        if state.invitation.is_none() {
            state.clipboard.check(
                &state.invitation_text,
                &mut crate::invitation_clipboard::SystemClipboard,
            );
            let mut submitted = None;
            if state.clipboard.offered() && clipboard_offer(ui, !self.busy) {
                // Reuse the text read when the form opened; never read again here.
                submitted = state.clipboard.take();
            }
            ui.label("Paste the invitation your friend sent you.");
            field(ui, "INVITATION", &mut state.invitation_text, false);
            ui.label(
                RichText::new("Relay checks the connection before you sign in.").color(c_muted()),
            );
            if ui
                .add_enabled(
                    !self.busy && !state.invitation_text.trim().is_empty(),
                    primary("Join server"),
                )
                .clicked()
            {
                submitted = Some(state.invitation_text.clone());
            }
            if let Some(text) = submitted {
                match crate::invitation::Invitation::parse(&text) {
                    Ok(invitation) => self.start_joining(ui.ctx(), kind, invitation),
                    Err(error) => self.error = Some(error),
                }
            }
            return false;
        }
        // An invitation is in use; a later "Use another invitation" must not
        // turn into a fresh clipboard check.
        state.clipboard.settle();
        if ui
            .add_enabled(!self.busy, secondary("Use another invitation"))
            .clicked()
        {
            if kind == FlowKind::AddCommunity {
                self.add_community
                    .setup
                    .edited(&mut self.add_community.invite);
                self.add_community.password.clear();
                self.add_community.url.clear();
            } else {
                self.login.setup.edited(&mut self.login.invite);
                self.login.password.clear();
            }
            self.hosting_setup.access_opened = false;
            self.error = None;
            return false;
        }
        true
    }

    fn check_friend_server(&mut self, kind: FlowKind, retry: bool) {
        let request_id = self.next_request();
        self.error = None;
        let (address, state) = if kind == FlowKind::AddCommunity {
            (&self.add_community.url, &mut self.add_community.setup)
        } else {
            (&self.login.url, &mut self.login.setup)
        };
        match state.begin_check(address, request_id, retry) {
            Ok(Some(url)) => {
                if let Err(error) = self.network.send(Command::CheckServer {
                    cancel: state.check_cancellation(),
                    authority: state.invitation.as_ref().and_then(|i|i.authority.clone()),
                        locator_invite: state.invitation.as_ref().filter(|i|i.authority.is_some()).map(|i|i.code.clone()),
                    url: url.clone(),
                    request_id,
                }) {
                    state.accept(address, &url, request_id, Err(error));
                }
            }
            Ok(None) => {}
            Err(error) => state.result = Some(Err(error)),
        }
    }

    fn friend_server_step(&mut self, ui: &mut egui::Ui, kind: FlowKind) -> bool {
        let address = if kind == FlowKind::AddCommunity {
            self.add_community.url.clone()
        } else {
            self.login.url.clone()
        };
        let private = crate::invitation::private_address(&address);
        let state = if kind == FlowKind::AddCommunity {
            &mut self.add_community.setup
        } else {
            &mut self.login.setup
        };
        if private && !state.private_connection_started && !self.busy {
            state.private_connection_started = true;
            self.begin_friend_connection(ui.ctx());
        }
        if private && !self.friend_connection_step(ui) {
            return false;
        }
        let state = if kind == FlowKind::AddCommunity {
            &self.add_community.setup
        } else {
            &self.login.setup
        };
        if state.verified_for(&address) {
            return true;
        }
        if !self.busy && state.needs_automatic_check() {
            self.check_friend_server(kind, false);
        }
        let state = if kind == FlowKind::AddCommunity {
            &self.add_community.setup
        } else {
            &self.login.setup
        };
        if state.pending.is_some() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Connecting to your friend's server…");
            });
            return false;
        }
        let access = state
            .invitation
            .as_ref()
            .and_then(|i| i.network_share.clone());
        if let Some(Err(error)) = &state.result {
            let unreachable =
                error.contains("Cannot reach") || error == crate::network::SERVER_TIMEOUT;
            routing_ui::status_card(ui, |ui| {
                ui.label(
                    RichText::new(if unreachable {
                        "Your friend's server didn't respond"
                    } else {
                        "Relay could not connect"
                    })
                    .strong()
                    .color(c_bad()),
                );
                ui.add_space(4.0);
                ui.add(
                    egui::Label::new(if unreachable {
                        "Ask your friend to open Relay. Under their server's name it should say \"Friends can join\"."
                    } else {
                        error.as_str()
                    })
                    .wrap(true),
                );
            });
            ui.add_space(8.0);
            if private {
                if let Some(access) = access {
                    ui.label(
                        "The first time, accept your friend's access invitation in your browser.",
                    );
                    if ui
                        .hyperlink_to(
                            if self.hosting_setup.access_opened {
                                "Open access invitation again"
                            } else {
                                "Accept access from my friend"
                            },
                            access,
                        )
                        .clicked()
                    {
                        // Opening the page is not proof of approval. Only a
                        // successful identity check below unlocks the account form.
                        self.hosting_setup.access_opened = true;
                        self.check_friend_server(kind, true);
                    }
                    if self.hosting_setup.access_opened {
                        ui.label("After accepting in your browser, return here and try again.");
                    }
                } else {
                    ui.label("Ask your friend to share access with your Tailscale account.");
                }
            }
            if ui.add_enabled(!self.busy, primary("Try again")).clicked() {
                self.check_friend_server(kind, true);
            }
        }
        false
    }

    fn simple_account_form(&mut self, ui: &mut egui::Ui) {
        let shared_account = self.central_account.account.is_some();
        if self.login.username.is_empty() && !self.prefs.profile_name.is_empty() {
            self.login.username = self.prefs.profile_name.clone();
        }
        let server_name = self
            .login
            .setup
            .result
            .as_ref()
            .and_then(|r| r.as_ref().ok())
            .map(|s| s.server_name.as_str())
            .or_else(|| {
                self.prefs
                    .communities
                    .iter()
                    .find(|c| c.url == self.login.url)
                    .map(|c| c.name.as_str())
            });
        ui.heading(if shared_account && self.hosting_setup.new_host {
            "Create your server"
        } else if shared_account {
            "Open this server"
        } else if self.login.registering {
            "Create your account"
        } else {
            "Welcome back"
        });
        if let Some(name) = server_name {
            ui.label(format!("For {name}"));
        }
        if let Some(note) = &self.login.setup.route_note {
            ui.label(RichText::new(format!("Connected · {note}")).color(c_good()));
        }
        ui.label(if shared_account {
            "Use your Relay account to continue."
        } else if self.hosting_setup.new_host {
            "This account manages your server."
        } else {
            "Your account belongs to this server."
        });
        if self.hosting_setup.new_host && self.login.registering {
            ui.add_space(12.0);
            if !self.hosting_setup.creation_confirmed {
                // A server left without accounts by an earlier start: its choices
                // were never made, and the first account is what commits them.
                self.creation_choices(ui);
                return;
            }
            self.creation_summary(ui);
        }
        ui.add_space(12.0);
        account_feedback_ui::form_status(
            ui,
            &mut self.account_feedback,
            self.error.as_deref(),
            self.hosting_setup.new_host && self.hosting_setup.local_ready && self.login.registering,
        );
        self.local_host_problem(ui);
        if !self.hosting_setup.local_ready && self.account_start.hosting_error.is_none()
            && crate::local_host::is_local_url(&self.login.url)
            && crate::local_host::data_directory()
                .ok()
                .is_some_and(|data| data.join("relay.sqlite3").is_file())
        {
            if ui
                .add_enabled(!self.busy, secondary("Start my server"))
                .clicked()
            {
                self.error = None;
                self.host_local();
            }
            ui.label(
                RichText::new(
                    "Starts the existing server on this PC. Your accounts and messages stay.",
                )
                .color(c_muted()),
            );
            ui.add_space(12.0);
        }
        if self.relay_server_account_form(ui, false) {
            return;
        }
        let attempt = account_feedback_ui::AccountAttempt::new(
            account_feedback_ui::AccountFlow::SignIn,
            &self.login.url,
            self.login.registering,
        );
        account_feedback_ui::account_field(
            ui,
            &mut self.account_feedback,
            &attempt,
            account_feedback_ui::AccountField::Username,
            &mut self.login.username,
        );
        let password = account_feedback_ui::account_field(
            ui,
            &mut self.account_feedback,
            &attempt,
            account_feedback_ui::AccountField::Password,
            &mut self.login.password,
        );
        let factor_enter =
            authenticator_ui::sign_in_field(ui, &mut self.login, self.error.as_deref());
        if self.login.registering {
            ui.label(
                RichText::new("Use at least 10 characters.")
                    .size(font_size(12.0))
                    .color(c_muted()),
            );
        }
        if ui
            .checkbox(&mut self.prefs.remember_session, "Keep me signed in")
            .changed()
        {
            self.save_preferences();
        }
        ui.add_space(12.0);
        let label = if self.busy {
            "Connecting…"
        } else if self.login.registering {
            "Create account and continue"
        } else {
            "Sign in"
        };
        if ui
            .add_enabled(
                !self.busy,
                primary(label).min_size(Vec2::new(ui.available_width(), 42.0)),
            )
            .clicked()
            || (!self.busy
                && (password.lost_focus() || factor_enter)
                && ui.input(|i| i.key_pressed(egui::Key::Enter)))
        {
            self.authenticate();
        }
        if ui
            .add_enabled(
                !self.busy,
                secondary(if self.login.registering {
                    "I already have an account"
                } else {
                    "Create an account instead"
                }),
            )
            .clicked()
        {
            self.login.registering = !self.login.registering;
            self.error = None;
        }
        if self.hosting_setup.entry == LoginEntry::Account && !self.hosting_setup.new_host {
            ui.collapsing("Use a different server", |ui| {
                self.setup_controls(ui, FlowKind::Login);
            });
        }
    }

    pub(super) fn friend_community_dialog(&mut self, ctx: &egui::Context) {
        let mut open = true;
        let mut create = false;
        let window = join_friends_window(ctx, &mut open);
        let close = host_setup_ui::show_with_header(ctx, window, "Join friends", |ui| {
            let saved = self
                .prefs
                .communities
                .iter()
                .find(|saved| saved.url == self.add_community.url)
                .map(|saved| (saved.name.clone(), saved.server_id.clone()));
            if saved.is_none() || self.add_community.setup.invitation.is_some() {
                self.join_steps(ui, FlowKind::AddCommunity);
            }
            if self.add_community.setup.invitation.is_none() {
                if let Some((name, id)) = saved {
                    ui.heading(format!("Sign in to {name}"));
                    self.add_community.setup.saved_server_id = (!id.is_empty()).then_some(id);
                    if ui
                        .add_enabled(!self.busy, secondary("Use an invitation instead"))
                        .clicked()
                    {
                        self.add_community
                            .setup
                            .edited(&mut self.add_community.invite);
                        self.add_community.url.clear();
                        self.add_community.password.clear();
                        self.hosting_setup.access_opened = false;
                        self.error = None;
                        return;
                    }
                } else if !self.friend_invitation_entry(ui, FlowKind::AddCommunity) {
                    // Signing in opens Relay directly, so the plus button is
                    // where a new server is made as well as joined.
                    ui.add_space(16.0);
                    ui.separator();
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new("Or make a new place for you and your friends.")
                            .color(c_muted()),
                    );
                    if ui
                        .add_enabled(!self.busy && self.voice_channel.is_none(), secondary("Create a server"))
                        .clicked()
                    {
                        create = true;
                    }
                    ui.add_space(16.0);
                    ui.collapsing("Or connect with a server address", |ui| {
                        self.setup_controls(ui, FlowKind::AddCommunity);
                        // Advanced connections retain explicit address/code
                        // entry. The normal invitation path never exposes them.
                        if self.add_community.setup.invitation.is_none() {
                            self.friend_community_account_form(ui, false);
                        }
                    });
                    if let Some(error) = self
                        .error
                        .as_ref()
                        .filter(|_| !self.account_feedback.drew_general(ui.ctx()))
                    {
                        ui.label(RichText::new(error).color(c_bad()));
                    }
                    return;
                }
            } else if !self.friend_invitation_entry(ui, FlowKind::AddCommunity) {
                return;
            }
            ui.add_space(12.0);
            if self.friend_server_step(ui, FlowKind::AddCommunity) {
                self.friend_community_account_form(ui, true);
            }
            ui.add_space(16.0);
            ui.collapsing("Advanced", |ui| {
                ui.label(format!("Server address: {}", self.add_community.url));
                if !self.add_community.invite.is_empty() {
                    ui.label(format!("Invitation code: {}", self.add_community.invite));
                }
            });
            if let Some(error) = self
                .error
                .as_ref()
                .filter(|_| !self.account_feedback.drew_general(ui.ctx()))
            {
                ui.add_space(8.0);
                ui.label(RichText::new(error).color(c_bad()));
            }
            ui.add_space(12.0);
            ui.label(
                RichText::new("Your friend hosts the server. You just connect.").color(c_muted()),
            );
        });
        open &= !close && !create;
        self.add_community_open &= open;
        if create {
            self.account_start.home = true;
            self.hosting_setup
                .begin_creation(crate::iroh_route::available());
            self.error = None;
        }
        if !open {
            self.add_community.password.clear();
            self.add_community
                .setup
                .edited(&mut self.add_community.invite);
            self.add_community.setup.invitation_text.clear();
            self.add_community.setup.clipboard.settle();
            self.hosting_setup.access_opened = false;
        }
    }

    fn friend_community_account_form(&mut self, ui: &mut egui::Ui, require_check: bool) {
        if self.relay_server_account_form(ui, true) {
            return;
        }
        if self.add_community.username.is_empty() && !self.prefs.profile_name.is_empty() {
            self.add_community.username = self.prefs.profile_name.clone();
        }
        ui.heading(if self.add_community.registering {
            "Create your account"
        } else {
            "Welcome back"
        });
        if let Some(Ok(server)) = &self.add_community.setup.result {
            ui.label(format!("For {}", server.server_name));
        }
        ui.label("Use your own account for this server.");
        ui.add_space(8.0);
        account_feedback_ui::form_status(
            ui,
            &mut self.account_feedback,
            self.error.as_deref(),
            false,
        );
        let attempt = account_feedback_ui::AccountAttempt::new(
            account_feedback_ui::AccountFlow::AnotherServer,
            &self.add_community.url,
            self.add_community.registering,
        );
        account_feedback_ui::account_field(
            ui,
            &mut self.account_feedback,
            &attempt,
            account_feedback_ui::AccountField::Username,
            &mut self.add_community.username,
        );
        let password = account_feedback_ui::account_field(
            ui,
            &mut self.account_feedback,
            &attempt,
            account_feedback_ui::AccountField::Password,
            &mut self.add_community.password,
        );
        let factor_enter =
            authenticator_ui::sign_in_field(ui, &mut self.add_community, self.error.as_deref());
        if self.add_community.registering {
            ui.label(
                RichText::new("Use at least 10 characters.")
                    .size(font_size(12.0))
                    .color(c_muted()),
            );
        }
        if ui
            .checkbox(&mut self.prefs.remember_session, "Keep me signed in")
            .changed()
        {
            self.save_preferences();
        }
        if !require_check && self.add_community.registering {
            field(
                ui,
                "INVITE CODE (IF REQUIRED)",
                &mut self.add_community.invite,
                false,
            );
        }
        ui.add_space(12.0);
        let can_connect = !self.busy
            && (!require_check
                || self
                    .add_community
                    .setup
                    .verified_for(&self.add_community.url))
            && !self.add_community.url.trim().is_empty()
            && !self.add_community.username.trim().is_empty()
            && !self.add_community.password.is_empty();
        let clicked = ui
            .add_enabled(
                can_connect,
                primary(if self.busy {
                    "Connecting…"
                } else if self.add_community.registering {
                    "Create account and join"
                } else {
                    "Sign in"
                })
                .min_size(Vec2::new(ui.available_width(), 42.0)),
            )
            .clicked();
        let enter = (password.lost_focus() || factor_enter)
            && ui.input(|input| input.key_pressed(egui::Key::Enter));
        if can_connect && (clicked || enter) {
            match self
                .add_community
                .setup
                .expected_id_with_saved(&self.add_community.url, &self.prefs.communities)
            {
                Ok(expected_server_id) => {
                    self.error = None;
                    let request_id = self.next_request();
                    if !self.account_feedback.submit(
                        attempt,
                        request_id,
                        &self.add_community.username,
                        &self.add_community.password,
                    ) {
                        return;
                    }
                    self.busy = true;
                    self.send(Command::RememberNextLogins {
                        enabled: self.prefs.remember_session,
                    });
                    self.send(Command::Auth {
                        authority: self.add_community.setup.invitation.as_ref().and_then(|i|i.authority.clone()),
                        url: self.add_community.url.clone(),
                        expected_server_id,
                        request: AuthRequest {
                            second_factor: (!self.add_community.registering
                                && !self.add_community.second_factor.trim().is_empty())
                            .then(|| self.add_community.second_factor.trim().to_owned()),
                            initial_protection: None,
                            username: self.add_community.username.trim().into(),
                            password: self.add_community.password.clone(),
                            invite: self.add_community.invite.trim().into(),
                        },
                        register: self.add_community.registering,
                        request_id,
                    });
                }
                Err(error) => self.error = Some(error),
            }
        }
        if ui
            .add_enabled(
                !self.busy,
                secondary(if self.add_community.registering {
                    "I already have an account"
                } else {
                    "Create an account instead"
                }),
            )
            .clicked()
        {
            self.add_community.registering = !self.add_community.registering;
            self.error = None;
        }
    }
}

/// The first-start name must already be a valid username on every server, so it
/// follows the server's own rule: 2 to 32 bytes of letters, numbers, dots,
/// dashes and underscores.
pub(super) fn profile_name(input: &str) -> Result<String, &'static str> {
    let name = input.trim();
    if name.len() < 2 || name.len() > 32 {
        return Err("Use 2 to 32 characters.");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        return Err("Use a–z, 0–9, dots, dashes or underscores. No spaces or special letters.");
    }
    Ok(name.to_owned())
}

/// R2.3: a short suggestion above the invitation field. Returns true on the
/// active click; it never shows the invitation itself.
fn clipboard_offer(ui: &mut egui::Ui, enabled: bool) -> bool {
    let mut clicked = false;
    egui::Frame::none()
        .fill(c_panel())
        .rounding(8.0)
        .inner_margin(Margin::same(12.0))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.add(egui::Label::new("Invitation found on your clipboard.").wrap(true));
            ui.add_space(6.0);
            clicked = ui.add_enabled(enabled, primary("Use invitation")).clicked();
        });
    ui.add_space(10.0);
    clicked
}

#[cfg(test)]
mod profile_name_tests {
    use super::profile_name;

    #[test]
    fn a_chosen_name_is_always_a_username_every_server_accepts() {
        for ok in ["ab", "Maria_k", "anna.s-1", "  anna  ", &"a".repeat(32)] {
            assert_eq!(profile_name(ok).as_deref(), Ok(ok.trim()), "{ok:?}");
        }
        for bad in [
            "",
            " ",
            "a",
            "maria k",
            "maria@x",
            "bo/b",
            "a\u{0}b",
            // Plain names only, so none can look like another person's name.
            "æøå",
            "\u{410}lex",
            &"a".repeat(33),
        ] {
            assert!(profile_name(bad).is_err(), "{bad:?}");
        }
    }
}
