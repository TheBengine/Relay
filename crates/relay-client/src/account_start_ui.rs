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
    /// Editing/replacing this join form invalidates its previous login even
    /// when the replacement invitation points at exactly the same address.
    form_cancel: Arc<AtomicBool>,
    browser: Option<crate::central_account::Controller>,
    challenge: Option<String>,
    authority: Option<relay_protocol::handover::RoutePin>,
    /// An automatic reconnect of a saved server after sign-in: it may not
    /// match the visible login form, and its failure is not an error page.
    background: bool,
    /// Started for another server than the visible login form (an automatic
    /// reconnect, or the first account of a server created while Relay is open).
    detached: bool,
}
impl Attempt {
    pub(super) fn is_people(&self) -> bool {
        self.people
    }

    fn form_current(
        &self,
        login: &crate::connection_setup::JoinFlow,
        other: &crate::connection_setup::JoinFlow,
        other_open: bool,
    ) -> bool {
        if self.other && !other_open {
            return false;
        }
        // Friends and saved-server reconnects deliberately use a temporary
        // form. Their existing request/account binding remains authoritative.
        if self.people || self.detached {
            return true;
        }
        let flow = if self.other { other } else { login };
        !self.form_cancel.load(Ordering::Acquire)
            && crate::community::normalize_url(&flow.url).as_deref() == Ok(self.url.as_str())
    }
}

/// Signing in opens Relay itself: saved servers reconnect one at a time with
/// the Relay account (each earlier one stays connected in the background),
/// then Friends opens last, so the user lands on the Friends home with every
/// saved server live. Nothing here asks for a password.
pub(super) struct AutoEntry {
    queue: std::collections::VecDeque<String>,
    friends: bool,
    started: Instant,
    /// A remembered login being resumed; the next step waits for it.
    resuming: Option<(String, Instant)>,
}

/// How long automatic entry waits for one remembered login to reconnect.
const RESUME_WAIT: Duration = Duration::from_secs(20);

/// The app's own server restarts in the background after launch; reconnecting
/// to it waits for that, but not forever.
const LOCAL_HOST_WAIT: Duration = Duration::from_secs(45);
impl Drop for Attempt {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
    }
}

pub(super) struct State {
    pub(super) pending: Option<Attempt>,
    pub(super) home: bool,
    pub(super) bypass: bool,
    /// Signing in opens Relay itself once; the start page below is only
    /// shown when that cannot happen or fails.
    pub(super) entered: bool,
    pub(super) auto: Option<AutoEntry>,
    pub(super) hosting_error: Option<String>,
    pub(super) creation_request: Option<String>,
    pub(super) opening_local: Option<String>,
    pub(super) local_choices: Option<Result<Vec<SavedCommunity>, String>>,
    pub(super) clearing_previous_account: Option<u64>,
    pub(super) account_cleanup_error: Option<String>,
    pub(super) friends_requested: bool,
    pub(super) friends_error: Option<String>,
    friends_resume: Option<(String, Instant)>,
    cancelled_navigation: std::collections::HashSet<String>,
    navigation_restore: Option<String>,
}
impl Default for State {
    fn default() -> Self {
        Self {
            pending: None,
            home: true,
            bypass: false,
            entered: false,
            auto: None,
            hosting_error: None,
            creation_request: None,
            opening_local: None,
            local_choices: None,
            clearing_previous_account: None,
            account_cleanup_error: None,
            friends_requested: false,
            friends_error: None,
            friends_resume: None,
            cancelled_navigation: Default::default(),
            navigation_restore: None,
        }
    }
}

impl State {
    fn cancel_navigation(&mut self, destination: Option<&str>) {
        self.navigation_restore = destination.map(str::to_owned);
        if let Some(auto) = self.auto.take() {
            if let Some((url, _)) = auto.resuming { self.cancelled_navigation.insert(url); }
        }
        if let Some((url, _)) = self.friends_resume.take() { self.cancelled_navigation.insert(url); }
        if self.pending.as_ref().is_some_and(|a| a.people || a.background) { self.pending = None; }
        if let Some(url) = destination { self.cancelled_navigation.remove(url); }
        self.friends_requested = false;
        self.home = false;
    }
    pub(super) fn finish_account_cleanup(&mut self, request_id: u64, result: Result<(), String>,
        verified_identity: Option<(String, String)>, last_identity: &mut Option<(String, String)>) -> bool {
        if self.clearing_previous_account != Some(request_id) { return false; }
        match result {
            Ok(()) => {
                *last_identity = verified_identity;
                self.clearing_previous_account = None;
                self.account_cleanup_error = None;
                true
            }
            Err(_) => {
                self.account_cleanup_error = Some("Windows could not remove the previous account's saved logins. Try again before opening this account.".into());
                false
            }
        }
    }

    pub(super) fn waiting(&self) -> bool {
        self.pending
            .as_ref()
            .and_then(|attempt| attempt.browser.as_ref())
            .is_some_and(|browser| browser.waiting())
    }
}

/// The saved identity is a cleanup marker, never proof of authentication.
pub(super) fn identity_needs_cleanup(previous: &Option<(String, String)>, verified: &crate::central_account::Account) -> bool {
    previous.as_ref().is_none_or(|(issuer, subject)| issuer != &verified.issuer || subject != &verified.subject)
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

fn signed_shell_allowed(verified: bool, cleaning: bool, creating: bool, explicit_join: bool) -> bool {
    verified && !cleaning && !creating && !explicit_join
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum FriendsConnectionAction { Cancel, Retry }
fn friends_connection_contents(ui: &mut egui::Ui, error: Option<&str>, connecting: bool) -> Option<FriendsConnectionAction> {
    ui.heading("Friends");
    ui.label(RichText::new("Messages and calls with your Relay friends.").color(c_muted()));
    ui.add_space(16.0);
    if let Some(error) = error {
        ui.add(egui::Label::new(RichText::new(error).color(c_bad())).wrap(true));
    } else if connecting {
        ui.horizontal(|ui| { ui.spinner(); ui.label("Connecting to Friends…"); });
    } else {
        ui.label("Friends is not connected. Your servers and settings are still available.");
    }
    ui.add_space(12.0);
    if connecting {
        ui.add(secondary("Cancel connection")).clicked().then_some(FriendsConnectionAction::Cancel)
    } else {
        ui.add(primary("Retry Friends")).clicked().then_some(FriendsConnectionAction::Retry)
    }
}

impl RelayApp {
    /// The verified global account can show the shell before Friends connects.
    pub(super) fn signed_in_shell(&self) -> bool {
        signed_shell_allowed(self.central_account.account.is_some(),
            self.account_start.clearing_previous_account.is_some(), self.hosting_setup.choosing_creation,
            self.session.is_none() && (self.hosting_setup.entry == host_setup_ui::LoginEntry::Join
                || self.login.has_invitation_input()))
    }
    pub(super) fn friends_shell_selected(&self) -> bool {
        self.central_account.account.is_some() && self.account_start.friends_requested
            && !self.selected_nav && self.home_page == Some(SettingsTab::Friends)
    }
    pub(super) fn friends_shell_pending(&self) -> bool {
        self.friends_shell_selected() && !(self.session.is_some() && self.in_people()
            && self.people_url().as_deref() == Some(self.login.url.as_str())
            && self.connection_status == ConnectionStatus::Connected)
    }
    pub(super) fn cancel_friends_navigation(&mut self, destination: Option<&str>) {
        self.account_start.cancel_navigation(destination);
    }
    pub(super) fn friends_connection_failed(&mut self, error: &str) {
        if self.account_start.friends_resume.is_some()
            || self.account_start.pending.as_ref().is_some_and(|a| a.people) {
            self.account_start.friends_error = Some(error.into());
            self.account_start.friends_resume = None;
            self.account_start.pending = None;
        }
    }
    pub(super) fn friends_connection_adopted(&mut self, url: &str, people: bool) {
        if self.people_url().as_deref() == Some(url) {
            self.account_start.friends_resume = None;
            self.account_start.friends_error = (!people).then(|| "This address is not the Relay Friends service.".into());
        }
    }
    pub(super) fn friends_connection_ui(&mut self, ui: &mut egui::Ui) {
        let connecting = self.account_start.friends_resume.is_some()
            || self.account_start.pending.as_ref().is_some_and(|a| a.people)
            || self.account_start.auto.is_some();
        match friends_connection_contents(ui, self.account_start.friends_error.as_deref(), connecting) {
            Some(FriendsConnectionAction::Cancel) => {
                self.cancel_friends_navigation(None);
                self.account_start.friends_requested = true;
                self.account_start.friends_error = Some("Connection cancelled.".into());
            }
            Some(FriendsConnectionAction::Retry) => self.open_people(),
            None => {}
        }
    }

    pub(super) fn cancelled_navigation_restore(&self) -> Option<String> {
        self.account_start.navigation_restore.clone().or_else(|| self.session.as_ref().map(|_| self.login.url.clone()))
    }
    pub(super) fn reject_cancelled_navigation(&self, url: &str) -> bool {
        self.account_start.cancelled_navigation.contains(url)
    }

    pub(super) fn account_front(&mut self, ctx: &egui::Context) -> bool {
        if self.capture_path.is_some() {
            return false;
        }
        if self.account_start.clearing_previous_account.is_some() {
            entry_surface::page(ctx, "relay-account-switch", |ui| {
                if let Some(error) = &self.account_start.account_cleanup_error {
                    ui.add(egui::Label::new(RichText::new(error).color(c_bad())).wrap(true));
                    if ui.add(primary("Try again")).clicked() {
                        self.begin_account_cleanup();
                    }
                } else {
                    ui.spinner();
                    ui.label("Updating saved logins…");
                }
                if ui.add(secondary("Sign out of Relay")).clicked() { self.sign_out_relay(); }
            });
            ctx.request_repaint_after(Duration::from_millis(100));
            return true;
        }
        let gate = self.central_account.account.is_none()
            && !self.account_start.bypass
            && (self.central_account.provider.is_some() || self.session.is_none());
        let invitation_form = (self.session.is_none()
            && self.hosting_setup.entry == host_setup_ui::LoginEntry::Join
            && self.login.has_invitation_input())
            || (self.add_community_open && self.add_community.has_invitation_input());
        if !gate && invitation_form && self.account_start.auto.is_none() {
            // Sign-in opens the invitation the user followed, not Friends or a
            // remembered server whose Connected response would clear this form.
            // The normal Join/Connect/Account screen still requires consent.
            self.account_start.entered = true;
            self.account_start.home = false;
            return false;
        }
        if !gate && self.signed_in_shell() {
            if !self.account_start.entered {
                self.account_start.entered = true;
                self.account_start.home = false;
                self.account_start.friends_requested = true;
                self.selected_nav = false;
                self.home_page = Some(SettingsTab::Friends);
                self.account_start.auto = Some(AutoEntry {
                    queue: self.saved_servers_to_reconnect().into(), friends: false,
                    started: Instant::now(), resuming: None,
                });
            }
            if self.account_start.friends_resume.as_ref().is_some_and(|(_, since)| since.elapsed() >= RESUME_WAIT) {
                self.account_start.friends_resume = None;
                self.account_start.friends_error = Some("Friends could not connect. Try again when your connection is available.".into());
            }
            self.auto_entry_tick(ctx);
            return false;
        }
        let home = self.central_account.account.is_some()
            && (self.account_start.home || self.hosting_setup.choosing_creation);
        if !gate && !home { return false; }
        // Silent restoration starts at launch. Interactive sign-in is an
        // explicit action, after the user can see the saved-login choice.
        self.people_call_notice(ctx);
        entry_surface::page(ctx, "relay-account-entry", |ui| {
            let width = ui.available_width();
            if gate {
                let invitation_waiting = invitation_form || self.external_invitation.is_pending();
                ui.heading(if invitation_waiting { "Sign in to join your friends" } else { "Welcome back" });
                ui.add_space(8.0);
                ui.label(
                    RichText::new(if invitation_waiting {
                        "Your invitation is waiting here. Sign in or create an account, then continue to your friend's server."
                    } else {
                        "One account for your friends and servers."
                    }).color(c_muted()),
                );
                ui.add_space(28.0);
                if self.central_account.provider.is_some() {
                    self.stay_signed_in_control(ui);
                    ui.add_space(12.0);
                    if self.central_account.waiting() {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(if self.central_account.browser_ready() {
                                "Continue in your browser, then come back to Relay."
                            } else if self.central_account.resuming() {
                                "Restoring your saved login…"
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
                            self.central_account.start_remembered(ctx.clone(), self.prefs.stay_signed_in);
                        }
                        ui.add_space(24.0);
                        ui.vertical(|ui| {
                            ui.spacing_mut().interact_size.y = 20.0;
                            ui.spacing_mut().button_padding = Vec2::ZERO;
                            ui.spacing_mut().item_spacing.y = 4.0;
                            ui.label(RichText::new("New to Relay?").color(c_muted()));
                            let create_account = egui::Button::new(
                                RichText::new("Create account")
                                    .text_style(egui::TextStyle::Body)
                                    .color(ui.visuals().hyperlink_color),
                            )
                            .frame(false);
                            if ui.add(create_account).clicked() {
                                #[cfg(windows)]
                                if let Some(surface) = &mut self.account_webview {
                                    surface.browser_only = false;
                                }
                                self.central_account.start_registration_remembered(ctx.clone(), self.prefs.stay_signed_in);
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
                        self.central_account.start_remembered(ctx.clone(), self.prefs.stay_signed_in);
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
                if let Some(warning) = self.central_account.warning {
                    ui.add(egui::Label::new(RichText::new(warning).color(c_muted())).wrap(true));
                }
            } else if self.hosting_setup.choosing_creation {
                self.creation_choices(ui);
                self.local_host_problem(ui);
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
                    RichText::new("Start a conversation or choose a server.").color(c_muted()),
                );
                ui.add_space(24.0);

                home_section(ui, "Friends", |ui| {
                    ui.label(
                        RichText::new("Messages and calls. No shared server needed.")
                            .color(c_muted()),
                    );
                    ui.add_space(10.0);
                    if ui
                        .add_sized([ui.available_width(), 44.0], primary("Open Friends"))
                        .clicked()
                    {
                        self.open_people();
                    }
                    if self.account_start.pending.is_some() {
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Connecting…");
                            if entry_surface::text_action(ui, "Cancel").clicked() {
                                self.account_start.pending = None;
                            }
                        });
                    }
                    if let Some(error) = &self.error {
                        ui.add_space(8.0);
                        ui.add(egui::Label::new(RichText::new(error).color(c_bad())).wrap(true));
                    }
                });
                ui.add_space(16.0);
                home_section(ui, "Your servers", |ui| {
                    let people_url = self.people_url();
                    let saved: Vec<_> = self
                        .prefs
                        .communities
                        .clone()
                        .into_iter()
                        .filter(|s| Some(&s.url) != people_url.as_ref())
                        .collect();
                    if saved.is_empty() {
                        ui.label(
                            RichText::new("Join your friends, or make a server of your own.")
                                .color(c_muted()),
                        );
                    }
                    for saved in saved {
                        if ui
                            .add_sized(
                                [ui.available_width(), 44.0],
                                secondary(if saved.name.is_empty() {
                                    &saved.url
                                } else {
                                    &saved.name
                                }),
                            )
                            .clicked()
                        {
                            if crate::local_host::is_local_url(&saved.url)
                                && crate::local_host::community_directory(&saved.server_id).is_ok()
                            {
                                self.open_local_server(saved.server_id);
                                continue;
                            }
                            self.login.url = saved.url;
                            self.login.registering = false;
                            self.login.setup = Default::default();
                            self.hosting_setup.entry = host_setup_ui::LoginEntry::Account;
                            self.hosting_setup.new_host = false;
                            self.account_start.home = false;
                            self.begin_account_login(false);
                        }
                    }
                    self.other_local_servers(ui);
                    self.local_host_problem(ui);
                    ui.add_space(12.0);
                    ui.columns(2, |columns| {
                        let left_width = columns[0].available_width();
                        let right_width = columns[1].available_width();
                        if columns[0]
                            .add_sized([left_width, 44.0], secondary("Create a server"))
                            .clicked()
                        {
                            self.hosting_setup
                                .begin_creation(crate::iroh_route::available());
                            self.error = None;
                        }
                        if columns[1]
                            .add_sized([right_width, 44.0], secondary("Join a server"))
                            .clicked()
                        {
                            self.account_start.home = false;
                            self.hosting_setup.entry = host_setup_ui::LoginEntry::Join;
                        }
                    });
                    if self.session.is_some()
                        && entry_surface::text_action(ui, "Return to my server").clicked()
                    {
                        self.account_start.home = false;
                    }
                });
                ui.add_space(24.0);
                ui.separator();
                ui.add_space(14.0);
                ui.label(RichText::new("Account").small().color(c_muted()));
                ui.add_space(6.0);
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

    pub(super) fn local_host_problem(&mut self, ui: &mut egui::Ui) {
        let Some(details) = self.account_start.hosting_error.clone() else {
            return;
        };
        ui.add_space(12.0);
        egui::Frame::none().fill(c_bg()).rounding(6.0)
            .stroke(Stroke::new(1.0_f32, c_line())).inner_margin(Margin::same(12.0))
            .show(ui, |ui| {
                ui.label(RichText::new("Your local server couldn't start").strong());
                ui.add_space(4.0);
                ui.add(egui::Label::new(RichText::new(
                    "Try again to reopen it. Your server data is kept, and Friends works separately."
                ).color(c_muted())).wrap(true));
                ui.add_space(8.0);
                if ui.add_enabled(!self.busy, secondary("Retry server")).clicked() {
                    if let Some(id) = self.account_start.opening_local.clone() {
                        self.open_local_server(id);
                    } else if let (Some(request), Some(server)) = (self.account_start.creation_request.clone(), crate::local_host::server_binary()) {
                        self.account_start.hosting_error = None;
                        self.busy = true;
                        self.send(Command::CreateLocal { server, request });
                    } else {
                        self.host_local();
                    }
                }
                egui::CollapsingHeader::new("Technical details").id_source("local-host-failure-details")
                    .show(ui, |ui| { ui.add(egui::Label::new(RichText::new(details).small().color(c_muted())).wrap(true)); });
            });
    }

    pub(super) fn remember_current_local(&mut self) -> Result<(), String> {
        self.save_preferences();
        self.account_start.local_choices = None;
        for saved in &self.prefs.communities {
            if saved.url == "http://127.0.0.1:4580"
                && crate::local_host::community_directory(&saved.server_id).is_ok()
            {
                crate::local_host::remember_community(saved)?;
            }
        }
        Ok(())
    }

    pub(super) fn other_local_servers(&mut self, ui: &mut egui::Ui) -> bool {
        let loaded = self.account_start.local_choices
            .get_or_insert_with(crate::local_host::saved_communities).clone();
        let servers = match loaded {
            Ok(servers) => servers,
            Err(error) => {
                ui.add_space(12.0);
                ui.label(RichText::new("Your saved local servers couldn't be loaded").color(c_bad()));
                ui.add(egui::Label::new("Your server data is kept. Retry to refresh this list.").wrap(true));
                if ui.add_enabled(!self.busy, secondary("Retry server list")).clicked() {
                    self.account_start.local_choices = None;
                }
                egui::CollapsingHeader::new("Details").id_source("local-server-list-error")
                    .show(ui, |ui| { ui.add(egui::Label::new(error).wrap(true)); });
                ui.add_space(12.0);
                return false;
            }
        };
        let current = self.prefs.communities.iter()
            .find(|saved| saved.url == "http://127.0.0.1:4580").map(|saved| saved.server_id.clone());
        let others: Vec<_> = servers.iter().filter(|saved| Some(&saved.server_id) != current.as_ref()).collect();
        if others.is_empty() { return false; }
        ui.add_space(12.0);
        ui.label(RichText::new("Other servers on this PC").strong());
        for saved in others {
            let duplicate = servers.iter().filter(|other| other.name.eq_ignore_ascii_case(&saved.name)).count() > 1;
            let label = if duplicate {
                format!("Open {} · {}", saved.name, saved.server_id.get(..8).unwrap_or(&saved.server_id))
            } else {
                format!("Open {}", saved.name)
            };
            let clicked = ui.push_id(&saved.server_id, |ui| {
                ui.add_enabled(!self.busy, secondary(&label)).clicked()
            }).inner;
            if clicked {
                self.open_local_server(saved.server_id.clone());
                return true;
            }
        }
        ui.add_space(12.0);
        false
    }

    pub(super) fn open_local_server(&mut self, server_id: String) {
        if let Err(error) = self.remember_current_local() {
            self.account_start.hosting_error = Some(error);
            return;
        }
        let Some(server) = crate::local_host::server_binary() else {
            self.account_start.hosting_error = Some("The bundled server is missing. Reinstall Relay to restore it.".into());
            return;
        };
        self.account_start.pending = None;
        self.account_start.auto = None;
        self.account_start.creation_request = None;
        self.account_start.opening_local = Some(server_id.clone());
        self.account_start.hosting_error = None;
        self.hosting_setup.cancel_creation();
        self.hosting_setup.new_host = false;
        self.hosting_setup.local_ready = false;
        self.error = None;
        self.busy = true;
        self.send(Command::OpenLocal { server, server_id });
    }

    pub(super) fn local_server_opened(&mut self, saved: SavedCommunity) {
        if self.account_start.opening_local.as_deref() != Some(saved.server_id.as_str()) { return; }
        self.account_start.opening_local = None;
        self.account_start.local_choices = None;
        self.account_start.hosting_error = None;
        self.hosting_setup.local_ready = true;
        self.hosting_setup.entry = host_setup_ui::LoginEntry::Account;
        self.account_start.home = false;
        // The inactive host has its own consent and account metadata. Replacing
        // the loopback slot never carries those choices between communities.
        self.prefs.communities.retain(|entry| entry.url != saved.url);
        self.prefs.communities.push(saved.clone());
        self.community_statuses.remove(&saved.url);
        self.community_reads.remove(&saved.url);
        self.community_social.remove(&saved.url);
        self.community_friends.remove(&saved.url);
        self.save_preferences();
        let previous = std::mem::take(&mut self.login);
        self.login.url = saved.url;
        self.login.username = saved.username;
        self.login.registering = false;
        self.login.setup.saved_server_id = Some(saved.server_id);
        self.begin_account_login(false);
        if self.session.is_some() {
            if let Some(attempt) = &mut self.account_start.pending { attempt.detached = true; }
            self.login = previous;
        }
    }

    /// Saved servers the Relay account can reopen, excluding Friends and any
    /// server that is already connected.
    fn saved_servers_to_reconnect(&self) -> Vec<String> {
        let people = self.people_url();
        self.prefs
            .communities
            .iter()
            .map(|saved| saved.url.clone())
            .filter(|url| Some(url) != people.as_ref())
            .filter(|url| !(self.session.is_some() && *url == self.login.url))
            .filter(|url| !self.communities.iter().any(|live| live.url == *url))
            .filter(|url| crate::community::account_destination(url).is_ok())
            .collect()
    }

    fn auto_entry_tick(&mut self, ctx: &egui::Context) {
        let Some(auto) = self.account_start.auto.as_mut() else {
            return;
        };
        if self.account_start.pending.is_some() {
            return;
        }
        if let Some((url, since)) = &auto.resuming {
            // A parked connection is not proof that switching to it completed.
            let live = self.session.is_some() && self.login.url == *url;
            if !live && since.elapsed() < RESUME_WAIT {
                ctx.request_repaint_after(Duration::from_millis(250));
                return;
            }
            auto.resuming = None;
        }
        if let Some(url) = auto.queue.front().cloned() {
            if crate::local_host::is_local_url(&url) && self.account_start.hosting_error.is_some() {
                auto.queue.pop_front();
                ctx.request_repaint();
                return;
            }
            if crate::local_host::is_local_url(&url)
                && !self.hosting_setup.local_ready
                && auto.started.elapsed() < LOCAL_HOST_WAIT
            {
                ctx.request_repaint_after(Duration::from_millis(250));
                return;
            }
            auto.queue.pop_front();
            // A login kept for a move (or by the user's choice) resumes without
            // a new session, which also works while that host is paused.
            if self.prefs.stay_signed_in && crate::credential_vault::load(&url).ok().flatten().is_some() {
                let expected_server_id = self
                    .prefs
                    .communities
                    .iter()
                    .find(|saved| saved.url == url)
                    .map(|saved| saved.server_id.clone())
                    .filter(|id| !id.is_empty());
                auto.resuming = Some((url.clone(), Instant::now()));
                self.send(Command::Resume {
                    url,
                    expected_server_id,
                });
                return;
            }
            self.reconnect_saved_server(url);
            return;
        }
        if !auto.friends {
            auto.friends = true;
            if self.people_url().is_some() {
                self.open_people();
                if self.account_start.pending.is_some()
                    || self.account_start.auto.as_ref().is_some_and(|auto| auto.resuming.is_some()) {
                    return;
                }
            }
        }
        // Friends could not open (or is not configured): land on whatever
        // server is connected; with nothing connected the start page remains.
        self.account_start.auto = None;
        if self.session.is_some() && self.error.is_none() {
            self.account_start.home = false;
        }
    }

    fn reconnect_saved_server(&mut self, url: String) {
        let previous_login = std::mem::take(&mut self.login);
        self.login.url = url;
        self.login.registering = false;
        self.login.setup = Default::default();
        self.hosting_setup.new_host = false;
        self.begin_account_login(false);
        if let Some(attempt) = &mut self.account_start.pending {
            attempt.background = true;
            attempt.detached = true;
        }
        self.login = previous_login;
    }

    /// A server created while Relay is already open (on Friends or another
    /// server) gets its first account from the Relay account at once, as the
    /// start page's "Continue with my Relay account" did. Errors are shown.
    pub(super) fn claim_new_server(&mut self, setup_required: Option<bool>) {
        if self.central_account.account.is_none() || self.account_start.pending.is_some() {
            return;
        }
        let previous_login = std::mem::take(&mut self.login);
        self.login.url = "http://127.0.0.1:4580".into();
        self.login.registering = setup_required != Some(false);
        self.login.setup = Default::default();
        self.begin_account_login(false);
        if let Some(attempt) = &mut self.account_start.pending {
            attempt.detached = true;
        }
        self.login = previous_login;
    }

    pub(super) fn sign_out_relay(&mut self) {
        self.staged_uploads.clear();
        #[cfg(windows)]
        if let Some(surface) = &mut self.account_webview {
            surface.reset();
        }
        self.begin_account_cleanup();
        if let Err(error) = self.central_account.forget() {
            self.central_account.error = Some(error);
        }
        self.settings_open = false;
        self.add_community_open = false;
        self.save_preferences();
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
        if other {
            if let Some(error) = &self.error {
                ui.add(egui::Label::new(RichText::new(error).color(c_bad())).wrap(true));
            }
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
        if self.central_account.account.is_none() { self.account_start.home = true; return; }
        if self.voice_channel.is_some() || self.joining_voice.is_some() {
            self.notice = Some(("Finish your current call before opening Friends.".into(), Instant::now()));
            return;
        }
        self.cancel_friends_navigation(None);
        self.account_start.entered = true;
        self.account_start.friends_requested = true;
        self.account_start.friends_error = None;
        self.selected_nav = false;
        self.home_page = Some(SettingsTab::Friends);
        self.admin_ui.friend_all_servers = false;
        self.error = None;
        let Some(url) = self.people_url() else {
            self.account_start.friends_error = Some("The Friends service address has not been configured.".into());
            return;
        };
        self.account_start.cancelled_navigation.remove(&url);
        let sign_in_required = self.community_statuses.get(&url) == Some(&ConnectionStatus::SignInRequired)
            || (self.login.url == url && self.connection_status == ConnectionStatus::SignInRequired);
        if self.in_people() && self.login.url == url && !sign_in_required {
            self.account_start.home = false;
            self.selected_nav = false;
            self.home_page = Some(SettingsTab::Friends);
            self.admin_ui.friend_all_servers = false;
            return;
        }
        self.account_start.home = false;
        if !sign_in_required && self.communities.iter().any(|community| community.url == url) {
            self.account_start.friends_resume = Some((url.clone(), Instant::now()));
            self.send(Command::SwitchCommunity { url });
            return;
        }
        if !sign_in_required && self.prefs.stay_signed_in && crate::credential_vault::load(&url).ok().flatten().is_some() {
            let expected_server_id = self.prefs.communities.iter()
                .find(|saved| saved.url == url).map(|saved| saved.server_id.clone())
                .filter(|id| !id.is_empty());
            self.account_start.friends_resume = Some((url.clone(), Instant::now()));
            self.send(Command::Resume { url, expected_server_id });
            return;
        }
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
        let deliberate_new_local = !other
            && self.hosting_setup.new_host
            && flow.registering
            && self.hosting_setup.creation_confirmed
            && crate::local_host::is_local_url(&url);
        let expected = match if deliberate_new_local {
            Ok(None)
        } else {
            flow.setup
                .expected_id_with_saved(&url, &self.prefs.communities)
        } {
            Ok(e) => e,
            Err(e) => {
                self.error = Some(e);
                return;
            }
        };
        let authority = flow
            .setup
            .invitation
            .as_ref()
            .and_then(|i| i.authority.clone());
        let invite = zeroize::Zeroizing::new(flow.invite.clone());
        let register = flow.registering;
        let form_cancel = flow.setup.check_cancellation();
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
            form_cancel,
            browser: None,
            challenge: None,
            authority: authority.clone(),
            background: false,
            detached: false,
        });
        self.send(Command::RememberNextLogins {
            enabled: self.prefs.stay_signed_in,
        });
        self.send(Command::AccountLoginBegin {
            authority,
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
            let current = self
                .account_start
                .pending
                .as_ref()
                .is_some_and(|attempt| self.account_login_current(request, &attempt.url));
            let attempt = self.account_start.pending.take();
            let people = attempt.as_ref().is_some_and(|a| a.people);
            let background = attempt.as_ref().is_some_and(|a| a.background);
            if people && current { self.account_start.friends_error = Some(error.into()); }
            // A saved server that cannot be reopened automatically stays in the
            // side rail with its sign-in state; it does not stop opening Relay.
            if !background && current {
                self.error = Some(error.into());
            }
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
                && a.form_current(&self.login, &self.add_community, self.add_community_open)
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
        if !self.account_login_current(request, url) {
            return;
        }
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
        if attempt.people {
            self.account_start.friends_error = Some("The Friends service identity changed. Check your connection and try again.".into());
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
        if !a.form_current(&self.login, &self.add_community, self.add_community_open)
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
            if a.people { self.account_start.friends_error = Some(error.into()); }
            self.error = Some(error.into());
            self.account_start.pending = None;
            return;
        }
        if let Some(proof) = browser.take_link_proof() {
            let command = Command::AccountLoginFinish {
                authority: a.authority.clone(),
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
    fn account_cleanup_survives_restart_and_only_matching_success_ack_advances_identity() {
        let account = crate::central_account::Account {
            issuer: "https://owned.invalid/realm".into(), subject: "account-b".into(),
            handle: "same_handle".into(), expires_at: u64::MAX,
        };
        let a = Some((account.issuer.clone(), "account-a".into()));
        let b = Some((account.issuer.clone(), account.subject.clone()));
        assert!(identity_needs_cleanup(&None, &account));
        assert!(identity_needs_cleanup(&a, &account));
        assert!(!identity_needs_cleanup(&b, &account));
        let mut prefs = Preferences { last_relay_identity: a.clone(), ..Default::default() };
        let mut state = State { clearing_previous_account: Some(17), ..Default::default() };
        assert!(!state.finish_account_cleanup(16, Ok(()), b.clone(), &mut prefs.last_relay_identity));
        assert_eq!(prefs.last_relay_identity, a);
        assert!(!state.finish_account_cleanup(17, Err("owned vault failure".into()), b.clone(), &mut prefs.last_relay_identity));
        assert_eq!(state.clearing_previous_account, Some(17));
        assert!(state.account_cleanup_error.is_some());
        // A crash before successful cleanup retains the old marker, including
        // when a new refresh credential was already saved by the worker.
        let restarted: Preferences = serde_json::from_slice(&serde_json::to_vec(&prefs).unwrap()).unwrap();
        assert!(identity_needs_cleanup(&restarted.last_relay_identity, &account));
        assert!(state.finish_account_cleanup(17, Ok(()), b.clone(), &mut prefs.last_relay_identity));
        assert_eq!(prefs.last_relay_identity, b);
        assert!(state.clearing_previous_account.is_none() && state.account_cleanup_error.is_none());
        assert!(!state.finish_account_cleanup(17, Ok(()), a, &mut prefs.last_relay_identity));
        assert_eq!(prefs.last_relay_identity, b);
    }

    #[test]
    fn account_attempt_rejects_replaced_same_address_forms_and_closed_join_dialogs() {
        let mut login = crate::connection_setup::JoinFlow {
            url: "https://community.invalid".into(),
            ..Default::default()
        };
        let make_attempt = |flow: &crate::connection_setup::JoinFlow, other| Attempt {
            url: flow.url.clone(),
            expected: None,
            request: 1,
            other,
            people: false,
            account: crate::central_account::Account {
                issuer: "https://accounts.invalid".into(),
                subject: "fixture-account".into(),
                handle: "fixture".into(),
                expires_at: u64::MAX,
            },
            cancel: Arc::new(AtomicBool::new(false)),
            form_cancel: flow.setup.check_cancellation(),
            browser: None,
            challenge: None,
            authority: None,
            background: false,
            detached: false,
        };
        let mut other = crate::connection_setup::JoinFlow {
            url: login.url.clone(),
            ..Default::default()
        };
        let old = make_attempt(&login, false);
        assert!(old.form_current(&login, &other, false));
        login.setup.edited(&mut login.invite);
        assert_eq!(login.url, old.url);
        assert!(!old.form_current(&login, &other, false));
        let replacement = make_attempt(&login, false);
        assert!(replacement.form_current(&login, &other, false));

        let dialog = make_attempt(&other, true);
        assert!(dialog.form_current(&login, &other, true));
        assert!(!dialog.form_current(&login, &other, false));
        other.setup = Default::default();
        assert!(!dialog.form_current(&login, &other, true));

        // Automatic reopen uses a deliberately temporary form, not the active
        // Join dialog. It still has its separate account/request/cancel checks.
        let mut automatic = make_attempt(&login, false);
        automatic.detached = true;
        login.setup = Default::default();
        assert!(automatic.form_current(&login, &other, false));
    }

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

/// Group related choices into a single clearly named area of the welcome card.
fn home_section(ui: &mut egui::Ui, title: &str, body: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::none()
        .fill(c_rail())
        .rounding(8.0)
        .stroke(Stroke::new(1.0_f32, c_line()))
        .inner_margin(Margin::same(16.0))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(title).strong().size(font_size(16.0)));
            ui.add_space(10.0);
            body(ui);
        });
}

#[cfg(test)]
#[path = "friends_shell_tests.rs"]
mod friends_shell_tests;
