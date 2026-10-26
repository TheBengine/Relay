//! Choices made once, when a new server is created: how its messages are
//! protected, and how friends will reach it.
//!
//! They are two separate choices. Server encryption decides what the server can
//! read; the connection decides how friends reach it. A Relay account is needed
//! for either, and neither changes the Relay account, other servers or
//! standalone Friends. Nothing here starts or changes anything until "Create
//! server" is pressed. The words and the decisions live in plain functions so
//! they can be tested.
//!
//! The chosen connection is a *plan* that outlives the window: once the first
//! account exists it is saved with the exact new server's id, resumed only for
//! that server, and removed when the way of sharing is actually in place or the
//! person changes or drops it.
use super::*;
use crate::appearance::{TEXT_BODY, TEXT_CAPTION};
use host_setup_ui::{Choice, Sharing};
use relay_protocol::ProtectionLevel;

#[cfg(all(test, windows))]
#[path = "creation_native_fixture.rs"]
mod native_fixture;

pub(super) const SCOPE: &str =
    "These choices are for this server only. They do not change your Relay account, your other servers or Friends.";

pub(super) const IROH_UNAVAILABLE: &str =
    "This copy of Relay does not include iroh. Install the complete Relay package, or choose Tailscale explicitly.";

/// What is published when a server is shared without accounts. Precise, not a
/// promise about what network services can observe.
pub(super) const IROH_DISCLOSURE: &str =
    "To let friends find your server, Relay publishes its endpoint key and the address of the relay it uses to a public directory (iroh.link). Your messages are not published. Network services along the way can still see connection details such as the addresses involved.";

/// What each person needs for a server shared through Tailscale. This is about
/// reaching this server only; signing in to a Relay account needs no Tailscale.
pub(super) const TAILSCALE_REQUIREMENTS: [&str; 5] = [
    "Everyone who connects to this server, including you, needs Tailscale: the connection built into Relay, or the Tailscale app.",
    "Everyone signs in to their own Tailscale account. The accounts do not have to belong to the same network.",
    "Everyone's Tailscale account needs access to this PC, for example through Tailscale device sharing. A Relay invitation alone does not give that access.",
    "Tailscale encrypts the connection. Server encryption is a separate choice.",
    "Signing in to your Relay account does not need Tailscale.",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Choices {
    pub protection: ProtectionLevel,
    pub route: Choice,
}

impl Choices {
    /// Missing support must not silently choose another transport. Confirmation
    /// checks availability and leaves an explicit alternative to the user.
    pub(super) fn recommended(_iroh_available: bool) -> Self {
        Self {
            protection: ProtectionLevel::Protected,
            route: Choice::Iroh,
        }
    }
}

pub(super) fn protection_title(level: ProtectionLevel) -> &'static str {
    match level {
        ProtectionLevel::Protected => "End-to-end encryption on (recommended)",
        ProtectionLevel::Open => "End-to-end encryption off",
    }
}

/// Who can read what, in plain words. The host may also be a participant on the
/// same PC, so nothing here says "the host's computer cannot read".
pub(super) fn protection_reads(level: ProtectionLevel) -> &'static str {
    match level {
        ProtectionLevel::Protected => {
            "The server stores messages and files encrypted. Only the people in a conversation can decrypt them or hear its calls. The server still knows who is in a conversation, when messages are sent and how large they are."
        }
        ProtectionLevel::Open => {
            "The person who runs the server can read all messages and files and can listen to calls. The connection to the server is still encrypted in transit. Every friend is asked to agree before sending messages or calling."
        }
    }
}

pub(super) fn route_title(route: Choice) -> &'static str {
    match route {
        Choice::Iroh => "Without accounts (iroh)",
        Choice::Tailscale => "With Tailscale",
    }
}

pub(super) fn route_about(route: Choice) -> &'static str {
    match route {
        Choice::Iroh => {
            "Friends join with your Relay invitation alone. No extra network account or app is needed; everyone still signs in to this Relay server as usual. The connection is encrypted whether or not server encryption is on."
        }
        Choice::Tailscale => {
            "Chat, voice, video and screen sharing over a private Tailscale network. Everyone needs the following."
        }
    }
}

/// A note about what the way of connecting reveals, shown with the choice.
pub(super) fn route_note(route: Choice) -> Option<&'static str> {
    match route {
        Choice::Iroh => Some(IROH_DISCLOSURE),
        Choice::Tailscale => None,
    }
}

fn route_summary(route: Choice) -> &'static str {
    match route {
        Choice::Iroh => "connecting without accounts (iroh)",
        Choice::Tailscale => "connecting with Tailscale",
    }
}

/// One line for the account step, so the choice is not forgotten.
pub(super) fn summary(choices: Choices) -> String {
    format!(
        "Chosen for this server: {} · {}",
        match choices.protection {
            ProtectionLevel::Protected => "end-to-end encryption on",
            ProtectionLevel::Open => "end-to-end encryption off",
        },
        route_summary(choices.route)
    )
}

// ---------------------------------------------------------------------------
// The saved plan

const PLAN_LIFETIME_SECS: u64 = 30 * 24 * 60 * 60;

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// The connection chosen for one exact new server, kept until it is in place.
/// No password, key or address is stored.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Pending {
    pub server_id: String,
    pub route: Choice,
    pub created: u64,
}

fn valid_server_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

impl Pending {
    pub(super) fn new(server_id: &str, route: Choice, now: u64) -> Option<Self> {
        valid_server_id(server_id).then(|| Self {
            server_id: server_id.into(),
            route,
            created: now,
        })
    }

    /// Well formed and not older than 30 days (or from a clock a day ahead).
    pub(super) fn valid(&self, now: u64) -> bool {
        valid_server_id(&self.server_id)
            && self.created <= now.saturating_add(24 * 60 * 60)
            && now.saturating_sub(self.created) <= PLAN_LIFETIME_SECS
    }

    /// The route, only for exactly this server.
    pub(super) fn route_for(&self, server_id: &str, now: u64) -> Option<Choice> {
        (self.valid(now) && self.server_id == server_id).then_some(self.route)
    }
}

/// The plan for the server that is signed in now: it must be this server's own
/// id and a server on this PC. A plan for another server is never replayed.
pub(super) fn plan_for(
    pending: Option<&Pending>,
    server_id: &str,
    local: bool,
    now: u64,
) -> Option<Choice> {
    if !local {
        return None;
    }
    pending?.route_for(server_id, now)
}

/// Read the saved plan leniently: anything unreadable becomes "no plan", so a
/// damaged value can never cost the person their other preferences.
pub(super) fn lenient_pending<'de, D>(deserializer: D) -> Result<Option<Pending>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value.and_then(|value| serde_json::from_value(value).ok()))
}

/// Remove the saved plan only if it belongs to this server. A plan made for
/// another server is left exactly as it is, and so is any plan when nobody is
/// signed in. Returns whether something was removed.
pub(super) fn clear_pending_for(pending: &mut Option<Pending>, server_id: Option<&str>) -> bool {
    let belongs = matches!(
        (pending.as_ref(), server_id),
        (Some(plan), Some(id)) if plan.server_id == id
    );
    if belongs {
        *pending = None;
    }
    belongs
}

/// Point this server's saved plan at another way. A plan for another server is
/// left alone. Returns whether the plan was changed.
pub(super) fn retarget_pending(
    pending: &mut Option<Pending>,
    server_id: Option<&str>,
    route: Choice,
) -> bool {
    match pending.as_mut() {
        Some(plan) if server_id == Some(plan.server_id.as_str()) => {
            plan.route = route;
            true
        }
        _ => false,
    }
}

/// A record that is expired, from the future or malformed is useless to every
/// server, so it is removed whichever server is signed in. A valid record is
/// never touched here. Returns whether something was removed.
pub(super) fn drop_invalid(pending: &mut Option<Pending>, now: u64) -> bool {
    if pending.as_ref().is_some_and(|plan| !plan.valid(now)) {
        *pending = None;
        true
    } else {
        false
    }
}

/// The first account exists: bind the choice made before it to this server.
pub(super) fn commit_plan(
    chosen: &mut Option<Choice>,
    server_id: &str,
    now: u64,
) -> Option<Pending> {
    let pending = Pending::new(server_id, (*chosen)?, now)?;
    *chosen = None;
    Some(pending)
}

// ---------------------------------------------------------------------------
// Carrying the plan out

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Attempt {
    #[default]
    Idle,
    /// The account-free sharing was asked to start.
    Starting,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RouteFailure {
    /// This copy of Relay cannot do it. Retrying does not help.
    Unavailable,
    /// It was asked to start and did not come up.
    DidNotStart,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum RouteStep {
    /// Something else is running; look again next frame.
    Wait,
    /// Start the account-free sharing now.
    Start,
    /// Open the Tailscale step; its own button starts the sign-in.
    ShowTailscale,
    /// The chosen way is in place. The plan is done.
    Established,
    /// The server is shared another way. Leave it; the plan is done.
    SharedElsewhere,
    /// The chosen way is not available or did not start. Keep the plan.
    Failed(RouteFailure),
}

pub(super) fn route_step(
    route: Choice,
    sharing: Option<Sharing>,
    iroh_available: bool,
    busy: bool,
    attempt: Attempt,
) -> RouteStep {
    match (route, sharing) {
        (Choice::Iroh, Some(Sharing::Iroh | Sharing::Both))
        | (Choice::Tailscale, Some(Sharing::Tailscale | Sharing::Both)) => RouteStep::Established,
        (_, Some(_)) => RouteStep::SharedElsewhere,
        (Choice::Tailscale, None) => RouteStep::ShowTailscale,
        (Choice::Iroh, None) => {
            if !iroh_available {
                RouteStep::Failed(RouteFailure::Unavailable)
            } else if busy {
                RouteStep::Wait
            } else if attempt == Attempt::Starting {
                RouteStep::Failed(RouteFailure::DidNotStart)
            } else {
                RouteStep::Start
            }
        }
    }
}

/// The requirements as a short list, wherever Tailscale is explained.
pub(super) fn tailscale_requirements(ui: &mut egui::Ui) {
    for line in TAILSCALE_REQUIREMENTS {
        ui.add(
            egui::Label::new(
                RichText::new(format!("• {line}"))
                    .size(font_size(TEXT_BODY))
                    .color(c_muted()),
            )
            .wrap(true),
        );
    }
}

enum CardAction {
    TryAgain,
    UseTailscale,
    DecideLater,
}

impl RelayApp {
    fn server_creation_has_call(&self) -> bool {
        self.voice_channel.is_some()
            || self.joining_voice.is_some()
            || self.audio_connecting
            || self.private_calls.intent.is_some()
            || self.rtc_request.is_some()
    }

    /// Begin the real thing: the same steps the three "Create a server"
    /// buttons took before the choices were asked.
    pub(super) fn start_new_server(&mut self) {
        if self.call_switch_pending() {
            self.error = Some("Wait for your call to finish switching before creating this server.".into());
            return;
        }
        let Some(server) = crate::local_host::server_binary() else {
            self.account_start.hosting_error =
                Some("The bundled server is missing. Reinstall Relay to restore it.".into());
            return;
        };
        if let Err(error) = self.remember_current_local() {
            self.error = Some(error);
            return;
        }
        // The final button names the call interruption. Merely opening or
        // cancelling the choices must leave the current call running.
        if self.server_creation_has_call() {
            self.leave_voice();
        }
        self.account_start.opening_local = None;
        self.account_start.home = false;
        self.hosting_setup.new_host = true;
        self.hosting_setup.local_ready = false;
        self.hosting_setup.resume_share = true;
        self.hosting_setup.entry = host_setup_ui::LoginEntry::Account;
        self.error = None;
        self.account_start.hosting_error = None;
        let request = uuid::Uuid::new_v4().to_string();
        self.account_start.creation_request = Some(request.clone());
        self.busy = true;
        self.send(Command::CreateLocal { server, request });
    }

    /// The choices for a new server. Returns true when the server was started.
    /// `new_host` already set means a half-created server is waiting for its
    /// first account: the choices are still open, so "Continue" replaces
    /// "Create server" and nothing is started again.
    pub(super) fn creation_choices(&mut self, ui: &mut egui::Ui) -> bool {
        let iroh_available = crate::iroh_route::available();
        let waiting_server = self.hosting_setup.new_host;
        let mut started = false;
        if !waiting_server && self.other_local_servers(ui) { return true; }
        ui.heading("Create your server");
        ui.label(
            RichText::new(
                "Your existing servers are kept. One local server runs at a time on this PC.",
            )
            .color(c_muted()),
        );
        ui.add(egui::Label::new(RichText::new(SCOPE).color(c_muted())).wrap(true));
        ui.add_space(14.0);

        ui.label(RichText::new("Server encryption").strong());
        for level in [ProtectionLevel::Protected, ProtectionLevel::Open] {
            ui.radio_value(
                &mut self.hosting_setup.creation_protection,
                level,
                protection_title(level),
            );
        }
        ui.add(
            egui::Label::new(
                RichText::new(protection_reads(self.hosting_setup.creation_protection))
                    .size(font_size(TEXT_BODY))
                    .color(c_muted()),
            )
            .wrap(true),
        );
        ui.add_space(14.0);

        ui.label(RichText::new("How friends connect").strong());
        for route in [Choice::Iroh, Choice::Tailscale] {
            let usable = route != Choice::Iroh || iroh_available;
            egui::Frame::none()
                .fill(c_rail())
                .rounding(6.0)
                .inner_margin(Margin::same(12.0))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.add_enabled_ui(usable, |ui| {
                        ui.radio_value(
                            &mut self.hosting_setup.creation_route,
                            route,
                            RichText::new(route_title(route)).strong(),
                        );
                    });
                    ui.add(
                        egui::Label::new(
                            RichText::new(route_about(route))
                                .size(font_size(TEXT_BODY))
                                .color(c_muted()),
                        )
                        .wrap(true),
                    );
                    if route == Choice::Tailscale {
                        tailscale_requirements(ui);
                    }
                    if let Some(note) = route_note(route) {
                        ui.add(
                            egui::Label::new(
                                RichText::new(note)
                                    .size(font_size(TEXT_CAPTION))
                                    .color(c_muted()),
                            )
                            .wrap(true),
                        );
                    }
                    if !usable {
                        ui.add(
                            egui::Label::new(RichText::new(IROH_UNAVAILABLE).color(c_muted()))
                                .wrap(true),
                        );
                    }
                });
            ui.add_space(8.0);
        }
        ui.add_space(6.0);

        let in_call = !waiting_server && self.server_creation_has_call();
        let switching_call = self.call_switch_pending();
        if in_call {
            ui.add(egui::Label::new(RichText::new(
                "You can choose the settings while staying in your call. Creating the server will leave the call and switch to your new server. Cancel keeps your call running."
            ).color(c_muted())).wrap(true));
            ui.add_space(8.0);
        }
        if switching_call {
            ui.label("Wait for your call to finish switching before creating this server.");
        }
        ui.horizontal(|ui| {
            let label = if waiting_server {
                "Continue"
            } else if in_call {
                "Leave call and create server"
            } else {
                "Create server"
            };
            if ui
                .add_enabled(!self.busy && !switching_call, primary(label).min_size(Vec2::new(180.0, 42.0)))
                .clicked()
            {
                if self.hosting_setup.confirm_creation(iroh_available) {
                    self.error = None;
                    if !waiting_server {
                        self.start_new_server();
                        started = true;
                    }
                } else {
                    self.error = Some(IROH_UNAVAILABLE.into());
                }
            }
            if !waiting_server && ui.add_enabled(!self.busy, secondary("Cancel")).clicked() {
                self.hosting_setup.cancel_creation();
                self.error = None;
            }
        });
        if let Some(error) = &self.error {
            ui.add_space(8.0);
            ui.add(egui::Label::new(RichText::new(error).color(c_bad())).wrap(true));
        }
        started
    }

    pub(super) fn creation_summary(&self, ui: &mut egui::Ui) {
        ui.add(
            egui::Label::new(
                RichText::new(summary(Choices {
                    protection: self.hosting_setup.creation_protection,
                    route: self.hosting_setup.creation_route,
                }))
                .size(font_size(TEXT_CAPTION))
                .color(c_muted()),
            )
            .wrap(true),
        );
    }

    /// The first account was just created on this PC's own server: save the
    /// connection chosen before it, tied to this exact server.
    pub(super) fn remember_creation_route(&mut self, server_id: &str) {
        if let Some(pending) = commit_plan(
            &mut self.hosting_setup.route_to_apply,
            server_id,
            now_secs(),
        ) {
            self.prefs.pending_creation = Some(pending);
            self.hosting_setup.route_attempt = Attempt::Idle;
            self.hosting_setup.route_failure = None;
            self.defer_preferences();
        }
    }

    /// The plan for the server signed in now, if there is one.
    fn creation_plan(&self) -> Option<Choice> {
        let local = self.session.is_some() && crate::local_host::is_local_url(&self.login.url);
        let server_id = &self.bootstrap.as_ref()?.server_id;
        plan_for(
            self.prefs.pending_creation.as_ref(),
            server_id,
            local,
            now_secs(),
        )
    }

    /// The server whose plan an explicit action may change: the one signed in.
    fn signed_in_server_id(&self) -> Option<String> {
        self.bootstrap.as_ref().map(|b| b.server_id.clone())
    }

    /// The person dropped the plan, changed the way, or it came true. Only the
    /// signed-in server's own plan is removed: Back and "Use Tailscale instead"
    /// on one host must never erase the saved choice of another server.
    pub(super) fn clear_creation_plan(&mut self) {
        self.hosting_setup.route_attempt = Attempt::Idle;
        self.hosting_setup.route_failure = None;
        let server = self.signed_in_server_id();
        if clear_pending_for(&mut self.prefs.pending_creation, server.as_deref()) {
            self.defer_preferences();
        }
    }

    /// Choose another way for the same server. It stays saved until in place.
    fn change_creation_plan(&mut self, route: Choice) {
        let server = self.signed_in_server_id();
        if retarget_pending(&mut self.prefs.pending_creation, server.as_deref(), route) {
            self.defer_preferences();
        }
        self.hosting_setup.route_attempt = Attempt::Idle;
        self.hosting_setup.route_failure = None;
        self.hosting_setup.copy_when_shared = false;
        self.error = None;
    }

    /// Carry out the saved plan through the ordinary start paths, for exactly
    /// the server it was made for. Opens the Invite dialog once per run so a
    /// restart resumes it. Never acts on another server or on a remote one.
    pub(super) fn apply_creation_route(&mut self) {
        let now = now_secs();
        if drop_invalid(&mut self.prefs.pending_creation, now) {
            // Expired or unreadable: removed, never replayed.
            self.hosting_setup.route_attempt = Attempt::Idle;
            self.hosting_setup.route_failure = None;
            self.defer_preferences();
            return;
        }
        let Some(route) = self.creation_plan() else {
            self.hosting_setup.route_failure = None;
            return;
        };
        if self.capture_path.is_some() {
            return;
        }
        let server_id = self
            .bootstrap
            .as_ref()
            .map(|b| b.server_id.clone())
            .unwrap_or_default();
        if !self.hosting_setup.open
            && self.hosting_setup.resumed_server.as_deref() != Some(server_id.as_str())
        {
            self.hosting_setup.resumed_server = Some(server_id.clone());
            self.hosting_setup.open = true;
        }
        if !self.hosting_setup.open {
            return;
        }
        let sharing = self
            .hosting_setup
            .sharing(crate::iroh_route::hosted_status(&server_id).is_some());
        let busy = self.busy
            || self.voice_channel.is_some()
            || self.joining_voice.is_some()
            || !self.pending_sends.is_empty();
        let step = route_step(
            route,
            sharing,
            crate::iroh_route::available(),
            busy,
            self.hosting_setup.route_attempt,
        );
        self.hosting_setup.route_failure = match step {
            RouteStep::Failed(reason) => Some(reason),
            _ => None,
        };
        match step {
            RouteStep::Wait | RouteStep::Failed(_) => {}
            RouteStep::Established | RouteStep::SharedElsewhere => self.clear_creation_plan(),
            RouteStep::Start => {
                self.hosting_setup.route_attempt = Attempt::Starting;
                self.hosting_setup.copy_when_shared = true;
                self.error = None;
                self.start_iroh_host();
            }
            RouteStep::ShowTailscale => {
                self.hosting_setup.copy_when_shared = false;
                self.hosting_setup.tailscale_step = true;
            }
        }
    }

    /// In the Connect step: what was chosen, and, when it did not start, what
    /// to do next. Existing invitations and routes are never touched here.
    pub(super) fn creation_route_card(&mut self, ui: &mut egui::Ui) {
        let Some(route) = self.creation_plan() else {
            return;
        };
        let failure = self.hosting_setup.route_failure;
        let mut action = None;
        egui::Frame::none()
            .fill(c_rail())
            .rounding(6.0)
            .inner_margin(Margin::same(12.0))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                match failure {
                    None => {
                        ui.add(
                            egui::Label::new(
                                RichText::new(format!(
                                    "Chosen when you created this server: {}.",
                                    route_summary(route)
                                ))
                                .size(font_size(TEXT_BODY))
                                .color(c_muted()),
                            )
                            .wrap(true),
                        );
                    }
                    Some(reason) => {
                        ui.label(
                            RichText::new("The connection you chose did not start")
                                .color(c_bad())
                                .strong(),
                        );
                        ui.add(
                            egui::Label::new(
                                RichText::new(match reason {
                                    RouteFailure::Unavailable => IROH_UNAVAILABLE,
                                    RouteFailure::DidNotStart => {
                                        "Relay could not start it. The reason is shown below. Your server, account and invitations are unchanged."
                                    }
                                })
                                .size(font_size(TEXT_BODY))
                                .color(c_muted()),
                            )
                            .wrap(true),
                        );
                        ui.horizontal_wrapped(|ui| {
                            if reason == RouteFailure::DidNotStart
                                && ui.add_enabled(!self.busy, primary("Try again")).clicked()
                            {
                                action = Some(CardAction::TryAgain);
                            }
                            if route == Choice::Iroh
                                && ui
                                    .add_enabled(!self.busy, secondary("Use Tailscale instead"))
                                    .clicked()
                            {
                                action = Some(CardAction::UseTailscale);
                            }
                            if ui
                                .add_enabled(!self.busy, secondary("Decide later"))
                                .clicked()
                            {
                                action = Some(CardAction::DecideLater);
                            }
                        });
                    }
                }
            });
        ui.add_space(10.0);
        match action {
            Some(CardAction::TryAgain) => {
                self.hosting_setup.route_attempt = Attempt::Idle;
                self.hosting_setup.route_failure = None;
                self.error = None;
            }
            Some(CardAction::UseTailscale) => self.change_creation_plan(Choice::Tailscale),
            Some(CardAction::DecideLater) => {
                self.clear_creation_plan();
                self.error = None;
            }
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use host_setup_ui::HostSetup;

    const SERVER: &str = "4be44aef-8c98-4db6-a9d5-81c2eb311330";
    const NOW: u64 = 1_800_000_000;

    fn joined(lines: &[&str]) -> String {
        lines.join(" ")
    }

    #[test]
    fn every_new_server_starts_from_the_recommended_choices() {
        assert_eq!(
            Choices::recommended(true),
            Choices {
                protection: ProtectionLevel::Protected,
                route: Choice::Iroh
            }
        );
        // A missing helper never turns an unchosen alternative into the default.
        assert_eq!(Choices::recommended(false).route, Choice::Iroh);
        assert_eq!(
            Choices::recommended(false).protection,
            ProtectionLevel::Protected
        );
    }

    #[test]
    fn begin_resets_to_defaults_and_touches_nothing_else() {
        let mut state = HostSetup::default();
        state.creation_protection = ProtectionLevel::Open;
        state.creation_route = Choice::Tailscale;
        state.route_to_apply = Some(Choice::Tailscale);
        state.hosted_address = Some("http://100.64.0.1:4580".into());
        state.iroh_only = true;
        state.share_url = "kept".into();
        state.local_ready = true;
        state.new_host = false;
        state.begin_creation(true);
        assert!(state.choosing_creation && !state.creation_confirmed);
        assert_eq!(state.creation_protection, ProtectionLevel::Protected);
        assert_eq!(state.creation_route, Choice::Iroh);
        assert_eq!(state.route_to_apply, None);
        // Visiting the choices changes no existing server and starts nothing.
        assert_eq!(
            state.hosted_address.as_deref(),
            Some("http://100.64.0.1:4580")
        );
        assert!(state.iroh_only && state.local_ready && !state.new_host);
        assert_eq!(state.share_url, "kept");
        assert!(!state.resume_share);
    }

    #[test]
    fn cancel_leaves_no_plan_and_no_server() {
        let mut state = HostSetup::default();
        state.begin_creation(true);
        state.creation_route = Choice::Tailscale;
        state.cancel_creation();
        assert!(!state.choosing_creation && !state.creation_confirmed);
        assert_eq!(state.route_to_apply, None);
        assert!(!state.new_host && !state.resume_share);
    }

    #[test]
    fn confirming_remembers_exactly_the_chosen_way_until_the_account_exists() {
        for route in [Choice::Iroh, Choice::Tailscale] {
            let mut state = HostSetup::default();
            state.begin_creation(true);
            state.creation_route = route;
            state.creation_protection = ProtectionLevel::Open;
            assert!(state.confirm_creation(true));
            assert!(!state.choosing_creation && state.creation_confirmed);
            assert_eq!(state.route_to_apply, Some(route));
            assert_eq!(state.creation_protection, ProtectionLevel::Open);
        }
    }

    #[test]
    fn the_account_free_way_cannot_be_confirmed_when_this_copy_lacks_it() {
        let mut state = HostSetup::default();
        state.begin_creation(false);
        assert_eq!(state.creation_route, Choice::Iroh);
        assert!(!state.confirm_creation(false));
        assert!(state.choosing_creation && !state.creation_confirmed);
        assert_eq!(state.route_to_apply, None);
        // Tailscale remains available only after the person actually chooses it.
        state.creation_route = Choice::Tailscale;
        assert!(state.confirm_creation(false));
        assert_eq!(state.route_to_apply, Some(Choice::Tailscale));
    }

    #[test]
    fn a_half_created_server_offers_the_choices_again_but_a_chosen_one_does_not() {
        let mut login = crate::connection_setup::JoinFlow::default();
        let mut state = HostSetup::default();
        state.creation_protection = ProtectionLevel::Open;
        state.creation_route = Choice::Tailscale;
        state.local_account_ready(&mut login, Some(true));
        assert!(state.new_host && !state.creation_confirmed);
        assert_eq!(state.creation_protection, ProtectionLevel::Protected);
        assert_eq!(
            state.creation_route,
            Choices::recommended(crate::iroh_route::available()).route
        );
        let mut chosen = HostSetup::default();
        chosen.begin_creation(true);
        chosen.creation_protection = ProtectionLevel::Open;
        chosen.creation_route = Choice::Tailscale;
        assert!(chosen.confirm_creation(true));
        chosen.local_account_ready(&mut login, Some(true));
        assert_eq!(chosen.creation_protection, ProtectionLevel::Open);
        assert_eq!(chosen.creation_route, Choice::Tailscale);
        assert_eq!(chosen.route_to_apply, Some(Choice::Tailscale));
        chosen.local_account_ready(&mut login, Some(false));
        assert_eq!(chosen.creation_protection, ProtectionLevel::Open);
    }

    // ---- the saved plan: round trip, restart and mismatch -----------------

    #[test]
    fn a_plan_is_tied_to_the_exact_server_it_was_made_for() {
        let pending = Pending::new(SERVER, Choice::Tailscale, NOW).unwrap();
        assert_eq!(pending.route_for(SERVER, NOW), Some(Choice::Tailscale));
        for other in [
            "4be44aef-8c98-4db6-a9d5-81c2eb311331",
            "",
            "OTHER",
            &SERVER.to_uppercase(),
        ] {
            assert_eq!(pending.route_for(other, NOW), None, "{other}");
        }
        // Only a server on this PC: is_local_url alone is never enough.
        assert_eq!(
            plan_for(Some(&pending), SERVER, true, NOW),
            Some(Choice::Tailscale)
        );
        assert_eq!(plan_for(Some(&pending), SERVER, false, NOW), None);
        assert_eq!(plan_for(Some(&pending), "another", true, NOW), None);
        assert_eq!(plan_for(None, SERVER, true, NOW), None);
    }

    #[test]
    fn a_plan_is_bounded_and_unreadable_ones_are_refused() {
        assert!(Pending::new("", Choice::Iroh, NOW).is_none());
        assert!(Pending::new(&"a".repeat(65), Choice::Iroh, NOW).is_none());
        assert!(Pending::new("bad id with spaces", Choice::Iroh, NOW).is_none());
        assert!(Pending::new("id/../x", Choice::Iroh, NOW).is_none());
        let pending = Pending::new(SERVER, Choice::Iroh, NOW).unwrap();
        assert!(pending.valid(NOW));
        assert!(pending.valid(NOW + PLAN_LIFETIME_SECS));
        assert!(!pending.valid(NOW + PLAN_LIFETIME_SECS + 1), "expires");
        assert!(pending.valid(NOW - 3600), "a clock a little behind is fine");
        assert!(
            !pending.valid(NOW - 3 * 24 * 3600),
            "a plan from the future is refused"
        );
        assert_eq!(
            pending.route_for(SERVER, NOW + PLAN_LIFETIME_SECS + 1),
            None
        );
        let junk = Pending {
            server_id: "x y".into(),
            route: Choice::Iroh,
            created: NOW,
        };
        assert!(!junk.valid(NOW));
    }

    #[test]
    fn the_plan_survives_a_restart_as_saved_preferences() {
        let mut prefs = Preferences::default();
        assert!(prefs.pending_creation.is_none());
        prefs.pending_creation = Pending::new(SERVER, Choice::Tailscale, NOW);
        let bytes = serde_json::to_vec(&prefs).unwrap();
        let text = String::from_utf8(bytes.clone()).unwrap();
        // Nothing but the id, the route and a time is saved.
        assert!(
            text.contains(r#""pending_creation":{"server_id":"4be44aef-8c98-4db6-a9d5-81c2eb311330","route":"tailscale","created":1800000000}"#),
            "{text}"
        );
        let loaded: Preferences = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(loaded.pending_creation, prefs.pending_creation);
        // The restarted app resumes it for that server only.
        let plan = |server: &str, local| {
            plan_for(loaded.pending_creation.as_ref(), server, local, NOW + 600)
        };
        assert_eq!(plan(SERVER, true), Some(Choice::Tailscale));
        assert_eq!(plan("different-server", true), None);
        assert_eq!(plan(SERVER, false), None);
        // Preferences written before this feature load with no plan.
        let mut old: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        old.as_object_mut().unwrap().remove("pending_creation");
        let old: Preferences = serde_json::from_value(old).unwrap();
        assert!(old.pending_creation.is_none());
        // A damaged plan is dropped; the other preferences still load.
        for damage in [
            serde_json::json!({"server_id": SERVER, "route": "carrier-pigeon", "created": 1}),
            serde_json::json!({"server_id": SERVER, "route": "iroh", "created": 1, "extra": 1}),
            serde_json::json!("text"),
            serde_json::json!(7),
        ] {
            let mut damaged: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            damaged["pending_creation"] = damage;
            damaged["username"] = serde_json::json!("kept");
            let loaded: Preferences = serde_json::from_value(damaged).unwrap();
            assert!(loaded.pending_creation.is_none());
            assert_eq!(loaded.username, "kept");
        }
    }

    #[test]
    fn explicit_changes_on_one_server_never_touch_the_plan_of_another() {
        let other = Pending::new(SERVER, Choice::Tailscale, NOW).unwrap();
        let here = "11111111-2222-3333-4444-555555555555";
        // Back, "Use Tailscale instead", "Decide later" and the card's change,
        // all on a server that has no plan of its own:
        let mut pending = Some(other.clone());
        assert!(!clear_pending_for(&mut pending, Some(here)));
        assert_eq!(pending, Some(other.clone()));
        assert!(
            !clear_pending_for(&mut pending, None),
            "nobody is signed in"
        );
        assert_eq!(pending, Some(other.clone()));
        assert!(!retarget_pending(&mut pending, Some(here), Choice::Iroh));
        assert!(!retarget_pending(&mut pending, None, Choice::Iroh));
        assert_eq!(pending, Some(other.clone()));
        // Near misses do not match either.
        for near in [
            SERVER.to_uppercase(),
            format!("{SERVER}0"),
            SERVER[..SERVER.len() - 1].to_string(),
            String::new(),
        ] {
            assert!(
                !clear_pending_for(&mut pending, Some(near.as_str())),
                "{near}"
            );
            assert_eq!(pending, Some(other.clone()), "{near}");
        }
        // On its own server the same actions work.
        assert!(retarget_pending(&mut pending, Some(SERVER), Choice::Iroh));
        let changed = pending.as_ref().unwrap();
        assert_eq!(
            (changed.route, changed.server_id.as_str()),
            (Choice::Iroh, SERVER)
        );
        assert_eq!(changed.created, NOW, "only the route changes");
        assert!(clear_pending_for(&mut pending, Some(SERVER)));
        assert!(pending.is_none());
        assert!(
            !clear_pending_for(&mut pending, Some(SERVER)),
            "nothing left"
        );
        assert!(!retarget_pending(&mut pending, Some(SERVER), Choice::Iroh));
    }

    #[test]
    fn only_a_useless_record_is_removed_whatever_server_is_signed_in() {
        let mut valid = Pending::new(SERVER, Choice::Iroh, NOW);
        assert!(!drop_invalid(&mut valid, NOW));
        assert!(valid.is_some());
        assert!(!drop_invalid(&mut None, NOW));
        // Expired, from the future and malformed records go.
        let mut expired = Pending::new(SERVER, Choice::Iroh, NOW);
        assert!(drop_invalid(&mut expired, NOW + PLAN_LIFETIME_SECS + 1));
        assert!(expired.is_none());
        let mut future = Pending::new(SERVER, Choice::Iroh, NOW + 10 * 24 * 3600);
        assert!(drop_invalid(&mut future, NOW));
        assert!(future.is_none());
        let mut junk = Some(Pending {
            server_id: "x y".into(),
            route: Choice::Tailscale,
            created: NOW,
        });
        assert!(drop_invalid(&mut junk, NOW));
        assert!(junk.is_none());
    }

    #[test]
    fn the_choice_made_before_the_account_is_bound_when_the_account_exists() {
        let mut chosen = Some(Choice::Tailscale);
        let pending = commit_plan(&mut chosen, SERVER, NOW).unwrap();
        assert_eq!(pending.route_for(SERVER, NOW), Some(Choice::Tailscale));
        assert_eq!(chosen, None, "the in-memory choice is consumed once");
        // Nothing chosen, or an unusable id: nothing is saved and nothing is lost.
        let mut none = None;
        assert!(commit_plan(&mut none, SERVER, NOW).is_none());
        let mut kept = Some(Choice::Iroh);
        assert!(commit_plan(&mut kept, "", NOW).is_none());
        assert_eq!(kept, Some(Choice::Iroh));
    }

    // ---- carrying the plan out --------------------------------------------

    #[test]
    fn the_account_free_plan_starts_once_and_failure_keeps_it_with_a_way_forward() {
        use Attempt::*;
        use RouteFailure::*;
        use RouteStep::*;
        let step = |sharing, available, busy, attempt| {
            route_step(Choice::Iroh, sharing, available, busy, attempt)
        };
        assert_eq!(step(None, true, false, Idle), Start);
        // While it starts, wait; if it ends without sharing, that is a failure to retry.
        assert_eq!(step(None, true, true, Starting), Wait);
        assert_eq!(step(None, true, false, Starting), Failed(DidNotStart));
        // After "Try again" the attempt is idle and it starts again.
        assert_eq!(step(None, true, false, Idle), Start);
        // Unavailable is its own state, not "already shared" and not a silent drop.
        assert_eq!(step(None, false, false, Idle), Failed(Unavailable));
        assert_eq!(step(None, false, true, Idle), Failed(Unavailable));
        // It came up.
        assert_eq!(
            step(Some(Sharing::Iroh), true, false, Starting),
            Established
        );
        assert_eq!(step(Some(Sharing::Both), true, false, Idle), Established);
        // The server is shared another way already: never replaced.
        assert_eq!(
            step(Some(Sharing::Tailscale), true, false, Idle),
            SharedElsewhere
        );
        assert_eq!(
            step(Some(Sharing::Tailscale), false, false, Idle),
            SharedElsewhere
        );
    }

    #[test]
    fn the_tailscale_plan_opens_its_step_and_never_starts_a_sign_in_by_itself() {
        use RouteStep::*;
        let step =
            |sharing, busy| route_step(Choice::Tailscale, sharing, true, busy, Attempt::Idle);
        assert_eq!(step(None, false), ShowTailscale);
        assert_eq!(step(None, true), ShowTailscale);
        assert_eq!(step(Some(Sharing::Tailscale), false), Established);
        assert_eq!(step(Some(Sharing::Both), false), Established);
        assert_eq!(step(Some(Sharing::Iroh), false), SharedElsewhere);
        // It works without the account-free connection too.
        assert_eq!(
            route_step(Choice::Tailscale, None, false, false, Attempt::Idle),
            ShowTailscale
        );
    }

    // ---- wording ----------------------------------------------------------

    #[test]
    fn encryption_wording_says_who_can_decrypt_and_makes_no_absolute_promise() {
        let on = protection_reads(ProtectionLevel::Protected);
        let off = protection_reads(ProtectionLevel::Open);
        assert!(on.contains("stores messages and files encrypted"));
        assert!(on.contains("Only the people in a conversation can decrypt"));
        assert!(on.contains("who is in a conversation, when messages are sent and how large"));
        // The host can also be a participant on the same PC.
        assert!(!on.contains("host's computer"));
        assert!(!on.contains("cannot read"));
        assert!(off.contains("can read all messages and files"));
        assert!(off.contains("listen to calls"));
        assert!(off.contains("encrypted in transit"));
        assert!(!off.contains("people outside cannot"));
        assert!(off.contains("agree before"));
        for text in [
            on,
            off,
            protection_title(ProtectionLevel::Protected),
            protection_title(ProtectionLevel::Open),
        ] {
            assert!(!text.contains("Tailscale"), "{text}");
        }
        assert!(protection_title(ProtectionLevel::Protected).contains("recommended"));
        assert!(!protection_title(ProtectionLevel::Open).contains("recommended"));
        assert!(
            SCOPE.contains("Relay account")
                && SCOPE.contains("other servers")
                && SCOPE.contains("Friends")
        );
    }

    #[test]
    fn the_account_free_wording_is_precise_about_what_is_published() {
        let about = route_about(Choice::Iroh);
        assert!(about.contains("No extra network account or app"));
        assert!(about.contains("signs in to this Relay server"));
        assert!(about.contains("encrypted whether or not server encryption is on"));
        assert!(!about.contains("Tailscale"));
        let note = route_note(Choice::Iroh).unwrap();
        assert!(note.contains("endpoint key"));
        assert!(note.contains("relay"));
        assert!(note.contains("iroh.link"));
        assert!(note.contains("Your messages are not published"));
        assert!(note.contains("connection details such as the addresses involved"));
        // No promise that nothing can be learned.
        for absolute in [
            "never learns",
            "your address",
            "anonymous",
            "nobody can see",
        ] {
            assert!(!note.contains(absolute), "{absolute}");
        }
        assert!(route_note(Choice::Tailscale).is_none());
        assert_eq!(route_title(Choice::Iroh), "Without accounts (iroh)");
    }

    #[test]
    fn tailscale_wording_names_every_requirement_without_widening_it() {
        let all = joined(&TAILSCALE_REQUIREMENTS);
        assert!(all.contains("Everyone who connects to this server, including you"));
        assert!(all.contains("needs Tailscale"));
        assert!(all.contains("the connection built into Relay, or the Tailscale app"));
        assert!(all.contains("their own Tailscale account"));
        assert!(all.contains("do not have to belong to the same network"));
        assert!(all.contains("access to this PC"));
        assert!(all.contains("device sharing"));
        assert!(all.contains("A Relay invitation alone does not give that access"));
        assert!(all.contains("Server encryption is a separate choice"));
        // Not applied to ordinary Relay login, and no claim about public access.
        assert!(all.contains("Signing in to your Relay account does not need Tailscale"));
        assert!(!all.contains("requires Tailscale"));
        assert!(!all.to_lowercase().contains("funnel"));
        assert_eq!(route_title(Choice::Tailscale), "With Tailscale");
        assert!(route_about(Choice::Tailscale).contains("Everyone needs"));
    }

    #[test]
    fn the_summary_names_both_choices() {
        let line = summary(Choices {
            protection: ProtectionLevel::Open,
            route: Choice::Tailscale,
        });
        assert_eq!(
            line,
            "Chosen for this server: end-to-end encryption off · connecting with Tailscale"
        );
        assert!(summary(Choices::recommended(true)).contains("encryption on"));
    }
}
