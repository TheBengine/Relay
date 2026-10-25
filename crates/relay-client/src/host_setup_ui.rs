use super::*;
use std::{io::Read, net::Ipv4Addr, process::Stdio};

#[derive(Clone, Deserialize)]
pub(super) struct HostReport {
    schema_version: u8,
    lan_ipv4: Option<String>,
    default_gateway: Option<String>,
    public_ipv4: Option<String>,
    router_wan_ipv4: Option<String>,
    upnp_available: bool,
    wan_status: String,
    cgnat_status: String,
}

impl HostReport {
    fn validate(&self) -> Result<(), String> {
        if self.schema_version != 1
            || [
                &self.lan_ipv4,
                &self.default_gateway,
                &self.public_ipv4,
                &self.router_wan_ipv4,
            ]
            .into_iter()
            .flatten()
            .any(|ip| ip.parse::<Ipv4Addr>().is_err())
            || !matches!(
                self.wan_status.as_str(),
                "unverified"
                    | "upstream_nat"
                    | "double_nat_possible"
                    | "public_address_matches"
                    | "mismatch_unverified"
            )
            || !matches!(self.cgnat_status.as_str(), "unverified" | "shared_address")
        {
            return Err("The network check returned an invalid result. Try again.".into());
        }
        if self.cgnat_status == "shared_address"
            && !self.router_wan_ipv4.as_ref().is_some_and(|value| {
                let b = value.parse::<Ipv4Addr>().unwrap().octets();
                b[0] == 100 && (64..=127).contains(&b[1])
            })
        {
            return Err(
                "The router address could not be confirmed. Check it and try again.".into(),
            );
        }
        Ok(())
    }
}

#[derive(Clone, PartialEq, Eq)]
struct SharingDraft {
    origin: CommandOrigin,
    address: String,
    share_url: Option<String>,
}

struct PendingInvitationCopy {
    request_id: u64,
    draft: SharingDraft,
    started: Instant,
}

fn sharing_page_url(address: &str) -> Option<String> {
    let host = url::Url::parse(address).ok()?;
    if host.scheme() != "http"
        || host.port_or_known_default() != Some(4580)
        || !host.username().is_empty()
        || host.password().is_some()
        || host.path() != "/"
        || host.query().is_some()
        || host.fragment().is_some()
        || !crate::tailscale_connect::valid_address(host.host_str()?)
    {
        return None;
    }
    let mut page = url::Url::parse("https://console.tailscale.com/admin/machines").ok()?;
    page.query_pairs_mut().append_pair("q", host.host_str()?);
    Some(page.into())
}

/// How your own community is shared right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Sharing {
    Tailscale,
    /// Without accounts (iroh).
    Iroh,
    /// An older setup which had both. Choosing either way below makes it one.
    Both,
}

impl Sharing {
    pub(super) fn current(tailscale: bool, iroh: bool) -> Option<Self> {
        match (tailscale, iroh) {
            (true, true) => Some(Self::Both),
            (true, false) => Some(Self::Tailscale),
            (false, true) => Some(Self::Iroh),
            (false, false) => None,
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Tailscale => "With Tailscale",
            Self::Iroh => "Without accounts (iroh)",
            Self::Both => "Tailscale and without accounts",
        }
    }
}

/// "1 Invitation → 2 Connect → 3 Account": where you are in a short flow.
/// `current` counts from 1.
pub(super) fn step_line(ui: &mut egui::Ui, steps: &[&str], current: usize) {
    ui.horizontal_wrapped(|ui| {
        for (index, label) in steps.iter().enumerate() {
            let n = index + 1;
            ui.label(
                RichText::new(format!("{n}  {label}")).color(if n == current {
                    c_accent()
                } else {
                    c_muted()
                }),
            );
            if n < steps.len() {
                ui.label(RichText::new("→").color(c_muted()));
            }
        }
    });
}

/// Which step of joining a friend you are on: 1 paste the invitation,
/// 2 connect (measuring, refused, or checking the server), 3 your account.
pub(super) fn join_step(has_invitation: bool, routing: bool, verified: bool) -> usize {
    if verified {
        3
    } else if has_invitation || routing {
        2
    } else {
        1
    }
}

/// The line under your own server's name: who can join, in a few words.
/// `true` when friends can join.
pub(super) fn sharing_status(sharing: Option<Sharing>) -> (&'static str, bool) {
    match sharing {
        None => ("Only this PC can join · Invite friends", false),
        Some(Sharing::Iroh) => ("Friends can join · without accounts", true),
        Some(Sharing::Tailscale) => ("Friends can join · Tailscale", true),
        Some(Sharing::Both) => ("Friends can join · Tailscale and without accounts", true),
    }
}

pub(super) fn local_invitation_notice(ui: &mut egui::Ui) {
    ui.label(RichText::new("Local network invitation").strong());
    ui.label("Same network or configured VPN only. For friends in other homes, enable internet sharing on the computer hosting this server.");
    ui.add_space(8.0);
}

/// What the host picked under "Change connection", or when creating a server.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Choice {
    #[default]
    Iroh,
    Tailscale,
}

/// One choice as it is shown: the words and whether it can be pressed.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct ChoiceView {
    pub choice: Choice,
    pub title: &'static str,
    pub about: &'static str,
    pub button: &'static str,
    pub enabled: bool,
    /// Why this choice cannot be used, when it cannot.
    pub unavailable: Option<&'static str>,
}

/// Why changing the connection cannot start now. The change restarts the
/// server, which would end a call and lose a message being sent.
pub(super) fn change_blocker(in_call: bool, sending: bool) -> Option<&'static str> {
    if in_call {
        Some("Leave the voice call first. Changing the connection restarts your server and would end it.")
    } else if sending {
        Some("Wait for your messages to finish sending, then change the connection.")
    } else {
        None
    }
}

pub(super) fn connection_choices(
    current: Sharing,
    iroh_available: bool,
    blocker: Option<&'static str>,
) -> [ChoiceView; 2] {
    let iroh_in_use = current == Sharing::Iroh;
    let tailscale_in_use = current == Sharing::Tailscale;
    [
        ChoiceView {
            choice: Choice::Iroh,
            title: "Without accounts (iroh)",
            about: "Chat, voice, video and screen sharing. No extra network account.",
            button: if iroh_in_use {
                "In use"
            } else {
                "Use without accounts"
            },
            enabled: !iroh_in_use && iroh_available && blocker.is_none(),
            unavailable: (!iroh_available && !iroh_in_use)
                .then_some("This copy of Relay does not include the account-free connection."),
        },
        ChoiceView {
            choice: Choice::Tailscale,
            title: "With Tailscale",
            about:
                "Chat, voice, video and screen sharing. You and your friends sign in to Tailscale.",
            button: if tailscale_in_use {
                "In use"
            } else {
                "Use Tailscale"
            },
            enabled: !tailscale_in_use && blocker.is_none(),
            unavailable: None,
        },
    ]
}

/// The two ways to share, one card each. Returns what was pressed.
pub(super) fn connection_chooser(
    ui: &mut egui::Ui,
    current: Sharing,
    iroh_available: bool,
    blocker: Option<&'static str>,
) -> Option<Choice> {
    let mut pressed = None;
    for view in connection_choices(current, iroh_available, blocker) {
        egui::Frame::none()
            .fill(c_rail())
            .rounding(6.0)
            .inner_margin(Margin::same(12.0))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new(view.title).strong());
                ui.add(egui::Label::new(RichText::new(view.about).color(c_muted())).wrap(true));
                if let Some(reason) = view.unavailable {
                    ui.add(egui::Label::new(RichText::new(reason).color(c_muted())).wrap(true));
                }
                ui.add_space(4.0);
                if ui
                    .add_enabled(view.enabled, secondary(view.button))
                    .clicked()
                {
                    pressed = Some(view.choice);
                }
            });
        ui.add_space(6.0);
    }
    if let Some(reason) = blocker {
        ui.add(egui::Label::new(RichText::new(reason).strong()).wrap(true));
        ui.add_space(4.0);
    }
    ui.add(
        egui::Label::new(
            RichText::new("Relay restarts your server for a few seconds when you change. Your community, accounts, history and encryption stay. Invitations you copied before stop working, so copy a new one afterwards.")
                .size(font_size(12.0))
                .color(c_muted()),
        )
        .wrap(true),
    );
    pressed
}

#[derive(Default)]
pub(super) struct HostSetup {
    pub open: bool,
    pub hosted_address: Option<String>,
    /// Address of a signed-in server on another computer; invitations use it as is.
    direct_address: Option<String>,
    /// Further addresses offered in new invitations (from Advanced), already valid.
    extra_addresses: Vec<String>,
    offer_text: String,
    offer_error: Option<String>,
    offer_synced: bool,
    /// Your own community is shared only through the account-free route.
    pub iroh_only: bool,
    /// The "Change connection" choices are open on the last step.
    pub change_open: bool,
    stop_invitations_confirm: bool,
    /// Moving from the account-free route to Tailscale: the Connect step is shown again.
    pub tailscale_step: bool,
    /// "Share and copy invitation" was pressed: copy the invitation as soon as
    /// the account-free route is up, without another click.
    pub copy_when_shared: bool,
    /// Windows Firewall and the account-free connection, checked once per visit.
    pub firewall: Option<crate::firewall_check::Firewall>,
    firewall_checked: bool,
    firewall_pending: Option<Receiver<Result<crate::firewall_check::Firewall, String>>>,
    firewall_fix: Option<Receiver<Result<(), String>>>,
    firewall_error: Option<String>,
    /// Display-only: a capture shows this way of sharing without running anything.
    pub capture_sharing: Option<Sharing>,
    pub entry: LoginEntry,
    pub new_host: bool,
    pub creation_protection: relay_protocol::ProtectionLevel,
    /// The choices for a new server are on screen. Nothing has been started.
    pub choosing_creation: bool,
    /// The choices were made for the server that is being created now.
    pub creation_confirmed: bool,
    pub creation_route: Choice,
    /// The way chosen at creation, before the first account exists. When the
    /// account is created it becomes the saved plan for that exact server.
    pub route_to_apply: Option<Choice>,
    /// Whether the account-free sharing was asked to start for the saved plan.
    pub route_attempt: super::server_creation::Attempt,
    /// Why the saved plan has not started, while it has not.
    pub route_failure: Option<super::server_creation::RouteFailure>,
    /// The server whose saved plan already reopened the Invite dialog this run.
    pub resumed_server: Option<String>,
    pub local_ready: bool,
    pub resume_share: bool,
    pub auto_share: bool,
    pub share_url: String,
    pub existing_access: bool,
    pub invitation: Option<crate::invitation::Invitation>,
    pending_invitation_copy: Option<PendingInvitationCopy>,
    pub sharing_identity: Option<(String, String)>,
    pub access_opened: bool,
    pub join_visible: bool,
    embedded_pending: Option<Receiver<Result<crate::embedded_network::Status, String>>>,
    embedded_result: Option<Result<crate::embedded_network::Status, String>>,
    embedded_check_at: Option<Instant>,
    router_wan: String,
    tailscale_pending: Option<Receiver<Result<crate::tailscale_connect::Status, String>>>,
    tailscale_result: Option<Result<crate::tailscale_connect::Status, String>>,
    pending: Option<(String, Receiver<Result<HostReport, String>>)>,
    result: Option<Result<HostReport, String>>,
}

impl HostSetup {
    /// Use the running server's account state, not the existence of its database.
    /// Older servers without the hint retain the current form choice.
    pub(super) fn local_account_ready(
        &mut self,
        login: &mut crate::connection_setup::JoinFlow,
        setup_required: Option<bool>,
    ) {
        self.local_ready = true;
        if let Some(required) = setup_required {
            self.new_host = required;
            login.registering = required;
            if required && !self.creation_confirmed {
                // A server left without accounts by an earlier start: its
                // choices were never made, so offer the recommended ones.
                self.reset_creation_choices(crate::iroh_route::available());
            }
        }
    }

    fn reset_creation_choices(&mut self, iroh_available: bool) {
        let recommended = super::server_creation::Choices::recommended(iroh_available);
        self.creation_protection = recommended.protection;
        self.creation_route = recommended.route;
        self.route_to_apply = None;
    }

    /// "Create a server" was pressed: show the choices with the recommended
    /// defaults, whatever an earlier server used. Starts nothing and leaves
    /// every existing server untouched.
    pub(super) fn begin_creation(&mut self, iroh_available: bool) {
        self.reset_creation_choices(iroh_available);
        self.choosing_creation = true;
        self.creation_confirmed = false;
    }

    pub(super) fn cancel_creation(&mut self) {
        self.choosing_creation = false;
        self.creation_confirmed = false;
        self.route_to_apply = None;
    }

    /// The choices are final for this server. Refuses a way this copy of Relay
    /// cannot use, so a choice is never accepted and then ignored.
    pub(super) fn confirm_creation(&mut self, iroh_available: bool) -> bool {
        if self.creation_route == Choice::Iroh && !iroh_available {
            return false;
        }
        self.choosing_creation = false;
        self.creation_confirmed = true;
        self.route_to_apply = Some(self.creation_route);
        true
    }

    /// The way friends reach this server has changed. Nothing prepared for the
    /// old way is kept: not the copied invitation, not the Tailscale access link.
    pub(super) fn connection_changed(&mut self) {
        self.invitation = None;
        self.share_url.clear();
        self.existing_access = false;
        self.cancel_invitation_copy();
        self.change_open = false;
        self.stop_invitations_confirm = false;
        self.tailscale_step = false;
    }

    /// Changing the iroh admission secret does not change Tailscale sharing.
    pub(super) fn invitations_stopped(&mut self) {
        self.invitation = None;
        self.cancel_invitation_copy();
        self.copy_when_shared = false;
        self.stop_invitations_confirm = false;
    }

    /// The server is now shared through Tailscale only.
    pub(super) fn tailscale_ready(&mut self, address: String) {
        self.hosted_address = Some(address);
        self.iroh_only = false;
        self.capture_sharing = None;
        self.connection_changed();
    }

    /// The server is shared through the account-free route. `only` means the
    /// server now runs on the placeholder address, so Tailscale is gone.
    pub(super) fn iroh_ready(&mut self, only: bool) {
        self.iroh_only = only;
        if only {
            self.hosted_address = None;
        }
        self.capture_sharing = None;
        self.connection_changed();
    }

    /// "Share and copy invitation" was pressed and the account-free route is up:
    /// the invitation can be copied now, once.
    pub(super) fn ready_to_auto_copy(&self, iroh_hosted: bool, busy: bool) -> bool {
        self.copy_when_shared
            && !busy
            && self.sharing(iroh_hosted) == Some(Sharing::Iroh)
            && self.invitation.is_none()
            && self.pending_invitation_copy.is_none()
    }

    /// Starts the firewall check once per visit, and collects its answers.
    fn poll_firewall(&mut self, ctx: &egui::Context) {
        if !self.firewall_checked && crate::iroh_route::available() {
            self.firewall_checked = true;
            self.firewall_check(ctx);
        }
        if let Some(result) = self
            .firewall_pending
            .as_ref()
            .and_then(|r| r.try_recv().ok())
        {
            self.firewall_pending = None;
            match result {
                Ok(found) => self.firewall = Some(found),
                // Not knowing is not a warning; the host is not bothered.
                Err(_) => self.firewall = None,
            }
        }
        if let Some(result) = self.firewall_fix.as_ref().and_then(|r| r.try_recv().ok()) {
            self.firewall_fix = None;
            match result {
                Ok(()) => {
                    self.firewall_error = None;
                    self.firewall_check(ctx);
                }
                Err(error) => self.firewall_error = Some(error),
            }
        }
    }

    fn firewall_check(&mut self, ctx: &egui::Context) {
        let (sender, receiver) = mpsc::channel();
        self.firewall_pending = Some(receiver);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = sender.send(crate::firewall_check::check());
            ctx.request_repaint();
        });
    }

    fn firewall_allow(&mut self, ctx: &egui::Context) {
        if self.firewall_fix.is_some() {
            return;
        }
        let (sender, receiver) = mpsc::channel();
        self.firewall_fix = Some(receiver);
        self.firewall_error = None;
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = sender.send(crate::firewall_check::allow());
            ctx.request_repaint();
        });
    }

    /// A short card when Windows blocks direct account-free connections.
    fn firewall_notice(&mut self, ui: &mut egui::Ui) {
        if self.firewall != Some(crate::firewall_check::Firewall::Blocked) {
            return;
        }
        let ctx = ui.ctx().clone();
        let mut allow = false;
        ui.add_space(10.0);
        super::routing_ui::status_card(ui, |ui| {
            ui.label(
                RichText::new("Windows blocks direct connections for Relay on this network")
                    .strong(),
            );
            ui.add(
                egui::Label::new(
                    RichText::new("Friends can still join, but calls may go through a relay server and be slower.")
                        .color(c_muted()),
                )
                .wrap(true),
            );
            ui.add_space(6.0);
            if self.firewall_fix.is_some() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Waiting for Windows…");
                });
            } else {
                allow = ui
                    .add(secondary("Allow in Windows Firewall"))
                    .on_hover_text("Windows asks for permission. Only Relay's account-free connection is allowed.")
                    .clicked();
            }
            if let Some(error) = &self.firewall_error {
                ui.label(RichText::new(error).color(c_bad()));
            }
        });
        if allow {
            self.firewall_allow(&ctx);
        }
    }

    /// How this community is shared right now, if it is.
    pub(super) fn sharing(&self, iroh_hosted: bool) -> Option<Sharing> {
        self.capture_sharing
            .or_else(|| Sharing::current(self.hosted_address.is_some(), iroh_hosted))
    }

    fn invitation_draft(&self, origin: &CommandOrigin) -> Option<SharingDraft> {
        if let Some(address) = &self.direct_address {
            return Some(SharingDraft {
                origin: origin.clone(),
                address: address.clone(),
                share_url: None,
            });
        }
        let address = match self.hosted_address.as_ref() {
            Some(address) => address,
            None => {
                // Shared only through the account-free route: no access link is needed.
                let route = crate::iroh_route::hosted(&origin.server_id)?;
                return Some(SharingDraft {
                    origin: origin.clone(),
                    address: route.address(),
                    share_url: None,
                });
            }
        };
        sharing_page_url(address)?;
        let share_url = if self.existing_access {
            None
        } else {
            crate::invitation::valid_share_url(self.share_url.trim())
                .then(|| self.share_url.trim().to_owned())
                .map(Some)?
        };
        Some(SharingDraft {
            origin: origin.clone(),
            address: address.clone(),
            share_url,
        })
    }

    fn begin_invitation_copy(&mut self, origin: &CommandOrigin, request_id: u64) -> bool {
        if !self.open || self.pending_invitation_copy.is_some() {
            return false;
        }
        let Some(draft) = self.invitation_draft(origin) else {
            return false;
        };
        self.pending_invitation_copy = Some(PendingInvitationCopy {
            request_id,
            draft,
            started: Instant::now(),
        });
        true
    }

    pub(super) fn cancel_invitation_copy(&mut self) {
        self.pending_invitation_copy = None;
    }

    pub(super) fn fail_invitation_copy(&mut self, request_id: u64) -> bool {
        if !self.open
            || self
                .pending_invitation_copy
                .as_ref()
                .is_none_or(|p| p.request_id != request_id)
        {
            return false;
        }
        self.pending_invitation_copy = None;
        true
    }

    fn expire_invitation_copy(&mut self, now: Instant) -> bool {
        if self
            .pending_invitation_copy
            .as_ref()
            .is_some_and(|p| now.saturating_duration_since(p.started) >= Duration::from_secs(30))
        {
            self.pending_invitation_copy = None;
            return true;
        }
        false
    }

    /// Without device verification; only the tests copy a link this way.
    #[cfg(test)]
    pub(super) fn finish_invitation_copy(
        &mut self,
        ctx: &egui::Context,
        request_id: u64,
        origin: &CommandOrigin,
        code: &str,
    ) -> Result<bool, String> {
        self.finish_invitation_copy_with_proof(ctx, request_id, origin, code, None, None)
    }

    /// As `finish_invitation_copy`; `proof` adds device verification to the link.
    pub(super) fn finish_invitation_copy_with_proof(
        &mut self,
        ctx: &egui::Context,
        request_id: u64,
        origin: &CommandOrigin,
        code: &str,
        proof: Option<crate::invite_proof::InviteProof>,
        authority: Option<relay_protocol::handover::RoutePin>,
    ) -> Result<bool, String> {
        if !self.open {
            self.cancel_invitation_copy();
            return Ok(false);
        }
        if self
            .pending_invitation_copy
            .as_ref()
            .is_none_or(|p| p.request_id != request_id)
        {
            return Ok(false);
        }
        let pending = self.pending_invitation_copy.take().unwrap();
        if pending.started.elapsed() >= Duration::from_secs(30)
            || self.invitation_draft(origin).as_ref() != Some(&pending.draft)
        {
            return Ok(false);
        }
        let mut invitation = crate::invitation::Invitation::new(
            &pending.draft.address,
            &pending.draft.origin.server_id,
            code,
        )?;
        if let Some(share) = &pending.draft.share_url {
            invitation = invitation.with_network_share(share)?;
        }
        if !self.extra_addresses.is_empty() {
            invitation = invitation.with_alternatives(&self.extra_addresses)?;
        }
        // Your own community may also be reachable without any account.
        if let Some(route) = crate::iroh_route::hosted(&pending.draft.origin.server_id) {
            invitation = invitation.with_iroh(route.to_invitation()?);
        }
        if let Some(proof) = proof {
            invitation = invitation.with_proof(proof);
        }
        let invitation=invitation.with_authority(authority)?.for_sharing();
        ctx.output_mut(|output| output.copied_text = invitation.browser_link());
        self.invitation = Some(invitation);
        Ok(true)
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum LoginEntry {
    #[default]
    Welcome,
    Join,
    Account,
    Advanced,
}

impl HostSetup {
    pub fn from_args(args: &[String]) -> Self {
        let router_wan = args
            .iter()
            .position(|arg| arg == "--router-wan")
            .and_then(|at| args.get(at + 1))
            .cloned()
            .unwrap_or_default();
        Self {
            open: args.iter().any(|arg| arg == "--hosting"),
            entry: if args
                .iter()
                .any(|arg| arg == "--server" || arg == "--username")
            {
                LoginEntry::Account
            } else {
                LoginEntry::Welcome
            },
            router_wan,
            ..Default::default()
        }
    }
    fn start(&mut self, ctx: &egui::Context) {
        if self.pending.is_some() {
            return;
        }
        let wan = self.router_wan.trim().to_string();
        if !wan.is_empty() && wan.parse::<Ipv4Addr>().is_err() {
            self.result = Some(Err(
                "Enter the WAN IPv4 address shown by your router, or leave it empty.".into(),
            ));
            return;
        }
        let (sender, receiver) = mpsc::channel();
        self.pending = Some((wan.clone(), receiver));
        self.result = None;
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = sender.send(run_probe(&wan));
            ctx.request_repaint();
        });
    }
    fn poll(&mut self, ctx: &egui::Context) {
        if let Some(receiver) = &self.embedded_pending {
            match receiver.try_recv() {
                Ok(result) => {
                    if result.as_ref().is_err()
                        || result
                            .as_ref()
                            .is_ok_and(|s| matches!(s.phase.as_str(), "error" | "stopped"))
                    {
                        self.auto_share = false;
                    }
                    self.embedded_pending = None;
                    self.embedded_result = Some(result);
                    self.embedded_check_at = Some(Instant::now());
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.embedded_pending = None;
                    self.auto_share = false;
                    self.embedded_result =
                        Some(Err("The connection check stopped. Try again.".into()));
                }
                Err(mpsc::TryRecvError::Empty) => {
                    ctx.request_repaint_after(Duration::from_millis(150))
                }
            }
        }
        let waiting = self.embedded_result.as_ref().is_some_and(|r| {
            r.as_ref()
                .is_ok_and(|s| matches!(s.phase.as_str(), "connecting" | "needs_login"))
        });
        if (self.open || self.entry == LoginEntry::Join || self.join_visible)
            && waiting
            && self.embedded_pending.is_none()
            && self
                .embedded_check_at
                .is_none_or(|t| t.elapsed() >= Duration::from_secs(2))
        {
            self.check_embedded(ctx, false);
        }
        if (self.open || self.entry == LoginEntry::Join || self.join_visible) && waiting {
            ctx.request_repaint_after(Duration::from_secs(2));
        }
        if let Some(receiver) = &self.tailscale_pending {
            match receiver.try_recv() {
                Ok(result) => {
                    self.tailscale_result = Some(result);
                    self.tailscale_pending = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.tailscale_result =
                        Some(Err("The Tailscale check stopped. Try again.".into()));
                    self.tailscale_pending = None;
                }
                Err(mpsc::TryRecvError::Empty) => {
                    ctx.request_repaint_after(Duration::from_millis(150))
                }
            }
        }
        if let Some((address, receiver)) = &self.pending {
            match receiver.try_recv() {
                Ok(result) => {
                    if address == self.router_wan.trim() {
                        self.result = Some(result);
                    }
                    self.pending = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.result = Some(Err("The network check stopped. Try again.".into()));
                    self.pending = None;
                }
                Err(mpsc::TryRecvError::Empty) => {
                    ctx.request_repaint_after(Duration::from_millis(150))
                }
            }
        }
    }
}

#[cfg(windows)]
fn run_probe(wan: &str) -> Result<HostReport, String> {
    use std::os::windows::process::CommandExt;
    let package = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent()?.parent().map(PathBuf::from))
        .ok_or("Cannot locate the Relay package.")?;
    let script = package.join("scripts/probe-host-network.ps1");
    if !script.is_file() {
        return Err(
            "The hosting helper is missing. Extract the complete latest Relay package.".into(),
        );
    }
    let mut command = std::process::Command::new("powershell.exe");
    command
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(script)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(0x08000000);
    if !wan.is_empty() {
        command.arg("-RouterWanIPv4").arg(wan);
    }
    let mut child = command
        .spawn()
        .map_err(|_| "Cannot start the network check.")?;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match child.try_wait() {
            Ok(Some(exit)) => {
                if !exit.success() {
                    return Err(
                        "The network check failed. Check your connection and try again.".into(),
                    );
                }
                let mut bytes = Vec::new();
                child
                    .stdout
                    .take()
                    .ok_or("The network check returned no result.")?
                    .take(16_385)
                    .read_to_end(&mut bytes)
                    .map_err(|_| "Cannot read the network result.")?;
                if bytes.len() > 16_384 {
                    return Err("The network result was too large.".into());
                }
                let text =
                    String::from_utf8(bytes).map_err(|_| "Cannot read the network result.")?;
                let report: HostReport =
                    serde_json::from_str(text.trim_start_matches('\u{feff}').trim())
                        .map_err(|_| "The network check returned an invalid result.")?;
                report.validate()?;
                return Ok(report);
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("Cannot check the network helper.".into());
            }
            Ok(None) => {}
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err("The network check timed out. Try again.".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(not(windows))]
fn run_probe(_wan: &str) -> Result<HostReport, String> {
    Err("Automatic hosting checks currently require the Windows package.".into())
}

/// Shows `window` with the shared dialog header (title, Close and Escape)
/// fixed above its scrolling content. Returns true when the header asks to
/// close the dialog.
pub(super) fn show_with_header(
    ctx: &egui::Context,
    window: egui::Window<'_>,
    title: &str,
    body: impl FnOnce(&mut egui::Ui),
) -> bool {
    let mut close = false;
    window.title_bar(false).vscroll(false).show(ctx, |ui| {
        close = profile_ui::dialog_header_ui::dialog_header(ui, title);
        egui::ScrollArea::vertical().show(ui, body);
    });
    close
}

fn invite_friends_window<'a>(ctx: &egui::Context, open: &'a mut bool) -> egui::Window<'a> {
    desktop_ui::dialog(ctx, "Invite friends")
        .open(open)
        .collapsible(false)
        .default_width(520.0)
        // Room for the connection choices on the last step; a shorter step
        // still sizes the dialog to its content.
        .default_height(640.0)
        .resizable(false)
}

impl RelayApp {
    pub(super) fn hosting_setup_dialog(&mut self, ctx: &egui::Context) {
        self.hosting_setup.join_visible = (self.add_community_open
            && ((self.add_community.setup.private_connection_started
                && crate::invitation::private_address(&self.add_community.url))
                || self
                    .add_community
                    .setup
                    .invitation
                    .as_ref()
                    .is_some_and(|i| crate::invitation::private_address(&i.server))))
            || (self.session.is_none()
                && self
                    .login
                    .setup
                    .invitation
                    .as_ref()
                    .is_some_and(|i| crate::invitation::private_address(&i.server)));
        self.hosting_setup.poll(ctx);
        // May reopen the dialog for the server a saved plan belongs to.
        self.apply_creation_route();
        if !self.hosting_setup.open {
            return;
        }
        if self.capture_path.is_none() {
            self.hosting_setup.poll_firewall(ctx);
        }
        if !self.hosting_setup.offer_synced {
            self.hosting_setup.offer_synced = true;
            self.hosting_setup.extra_addresses = self.prefs.connection.offer.clone();
            self.hosting_setup.offer_text = self.prefs.connection.offer.join("\n");
        }
        if self.hosting_setup.expire_invitation_copy(Instant::now()) {
            self.busy = false;
            self.error =
                Some("Creating the invitation took too long. Try copying it again.".into());
        }
        if self.hosting_setup.pending_invitation_copy.is_some() {
            ctx.request_repaint_after(Duration::from_secs(1));
        }
        let mut open = true;
        let mut go_to_account = false;
        let identity = self
            .bootstrap
            .as_ref()
            .map(|b| (b.server_id.clone(), b.user.id.clone()));
        if identity != self.hosting_setup.sharing_identity {
            self.hosting_setup.sharing_identity = identity;
            self.hosting_setup.share_url.clear();
            self.hosting_setup.existing_access = false;
            self.hosting_setup.invitation = None;
            self.hosting_setup.cancel_invitation_copy();
        }
        if self.hosting_setup.auto_share
            && self.hosting_setup.embedded_pending.is_none()
            && !self.busy
        {
            if let Some(address) = self
                .hosting_setup
                .embedded_result
                .as_ref()
                .and_then(|r| r.as_ref().ok())
                .filter(|s| s.phase == "ready")
                .and_then(|s| s.address.clone())
            {
                self.hosting_setup.auto_share = false;
                self.start_embedded_host(address);
            }
        }
        if self.hosting_setup.copy_when_shared {
            let own_id = self
                .bootstrap
                .as_ref()
                .map(|b| b.server_id.clone())
                .unwrap_or_default();
            if self
                .hosting_setup
                .ready_to_auto_copy(crate::iroh_route::hosted(&own_id).is_some(), self.busy)
            {
                self.hosting_setup.copy_when_shared = false;
                self.request_invitation_copy();
            }
        }
        let window = invite_friends_window(ctx, &mut open);
        let close = show_with_header(ctx, window, "Invite friends", |ui| {
            self.lan_pairing_host_controls(ui);
            if self.remote_invite_step(ui) {
                return;
            }
            let local = self.session.is_some()
                && (crate::local_host::is_local_url(&self.login.url) || self.capture_local_host);
            let own_id = self
                .bootstrap
                .as_ref()
                .map(|b| b.server_id.clone())
                .unwrap_or_default();
            let sharing = self
                .hosting_setup
                .sharing(crate::iroh_route::hosted(&own_id).is_some());
            let account_free = sharing == Some(Sharing::Iroh);
            let step = if !local {
                1
            } else if self.hosting_setup.tailscale_step || sharing.is_none() {
                2
            } else {
                3
            };
            step_line(ui, &["Your server", "Connect", "Invite"], step);
            ui.add_space(16.0);
            if step == 1 {
                if self.hosting_setup.choosing_creation {
                    if self.creation_choices(ui) {
                        go_to_account = true;
                    }
                } else {
                    ui.heading("Start with your own server");
                    ui.label("Choose how it is protected and how friends connect, create its first account, then Relay will help you invite friends.");
                    if ui
                        .add_enabled(
                            !self.busy && self.session.is_none(),
                            primary("Create my server"),
                        )
                        .clicked()
                    {
                        self.hosting_setup
                            .begin_creation(crate::iroh_route::available());
                        self.error = None;
                    }
                    if self.session.is_some() {
                        ui.label("You are visiting another server. Return to your own server to change its sharing.");
                    }
                }
            } else if step == 2 {
                self.creation_route_card(ui);
                if self.hosting_setup.tailscale_step
                    && ui.add_enabled(!self.busy, secondary("← Back")).clicked()
                {
                    // Going back is a deliberate change of mind about the saved choice.
                    self.clear_creation_plan();
                    self.hosting_setup.tailscale_step = false;
                    self.hosting_setup.auto_share = false;
                    self.error = None;
                }
                let available = !self.busy
                    && self.hosting_setup.embedded_pending.is_none()
                    && self.voice_channel.is_none()
                    && self.joining_voice.is_none()
                    && self.pending_sends.is_empty();
                if crate::iroh_route::available() && !self.hosting_setup.tailscale_step {
                    // The easy way first: one button shares and copies the invitation.
                    ui.heading("Invite your friends");
                    ui.label("Friends join with the invitation alone. Chat and voice work, and nobody needs an extra account.");
                    ui.add_space(12.0);
                    if ui
                        .add_enabled(
                            available && !self.hosting_setup.auto_share,
                            primary("Share and copy invitation")
                                .min_size(Vec2::new(ui.available_width(), 42.0)),
                        )
                        .clicked()
                    {
                        self.error = None;
                        self.hosting_setup.copy_when_shared = true;
                        self.start_iroh_host();
                    }
                    if self.busy || self.hosting_setup.copy_when_shared {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Getting your invitation ready…");
                        });
                    }
                    if self.voice_channel.is_some() || self.joining_voice.is_some() {
                        ui.label("Leave the call first, then share.");
                    }
                    if !self.pending_sends.is_empty() {
                        ui.label("Wait for your messages to finish sending.");
                    }
                    ui.add(egui::Label::new(RichText::new(super::server_creation::IROH_DISCLOSURE).size(font_size(12.0)).color(c_muted())).wrap(true));
                    self.hosting_setup.firewall_notice(ui);
                    ui.add_space(14.0);
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new("Other connection options").color(c_muted()));
                        if ui
                            .add_enabled(!self.busy, secondary("Use Tailscale instead"))
                            .clicked()
                        {
                            self.clear_creation_plan();
                            self.error = None;
                            self.hosting_setup.copy_when_shared = false;
                            self.hosting_setup.tailscale_step = true;
                        }
                    });
                } else {
                    ui.heading("Let friends connect from home");
                    ui.label("Relay uses Tailscale to connect you privately. First time, you and each friend sign in with your own Tailscale accounts in the browser. No separate app is needed.");
                    super::server_creation::tailscale_requirements(ui);
                    ui.add_space(12.0);
                    if !self.hosting_setup.auto_share
                        && ui
                            .add_enabled(available, primary("Enable sharing"))
                            .clicked()
                    {
                        self.error = None;
                        self.hosting_setup.auto_share = true;
                        self.hosting_setup.check_embedded(ctx, true);
                    }
                    if self.hosting_setup.auto_share || self.busy {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(if self.busy {
                                "Preparing your server…"
                            } else {
                                "Waiting for your connection…"
                            });
                        });
                    }
                    self.private_sign_in_status(ui);
                    if self.voice_channel.is_some() || self.joining_voice.is_some() {
                        ui.label("Leave the call first, then enable sharing.");
                    }
                    if !self.pending_sends.is_empty() {
                        ui.label("Wait for your messages to finish sending.");
                    }
                }
                ui.add_space(10.0);
                ui.label(RichText::new("Relay may briefly restart your server. Your account, channels, history and encryption keys stay.").color(c_muted()));
            } else {
                if self.hosting_setup.invitation.is_none() {
                    let ready = account_free
                        || self.hosting_setup.existing_access
                        || crate::invitation::valid_share_url(self.hosting_setup.share_url.trim());
                    ui.heading(if ready {
                        "Send your invitation"
                    } else {
                        "Give your friend access"
                    });
                    if !self.hosting_setup.existing_access && !account_free {
                        ui.add_enabled_ui(!self.busy && self.hosting_setup.pending_invitation_copy.is_none(), |ui| {
                                if !ready {
                                    ui.label("Open the page below, choose Relay → ⋯ → Share → Copy invite link → Copy share link, then paste the link here.");
                                    if let Some(page) = self.hosting_setup.hosted_address.as_deref().and_then(sharing_page_url) {
                                        if ui.add(primary("Give access to a friend")).clicked() {
                                            ui.output_mut(|output| output.open_url = Some(egui::OpenUrl { url: page, new_tab: true }));
                                        }
                                    }
                                    ui.collapsing("Can't find Relay?", |ui| {
                                        if let Some(address) = &self.hosting_setup.hosted_address { ui.label(RichText::new(format!("Choose Relay at {}", address.trim_start_matches("http://").trim_end_matches(":4580"))).color(c_muted())); }
                                    });
                                }
                                field(ui,"ACCESS LINK FROM THE SHARING PAGE",&mut self.hosting_setup.share_url,false);
                                if !self.hosting_setup.share_url.trim().is_empty() && !crate::invitation::valid_share_url(self.hosting_setup.share_url.trim()) { ui.label(RichText::new("Choose Share → Copy invite link → Copy share link, then paste the link here.").color(c_bad())); }
                            });
                    }
                    if ready
                        && ui
                            .add_enabled(
                                !self.busy && self.hosting_setup.pending_invitation_copy.is_none(),
                                primary("Copy invitation"),
                            )
                            .clicked()
                    {
                        self.request_invitation_copy();
                    }
                    if self.hosting_setup.pending_invitation_copy.is_some() {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Creating and copying your invitation…");
                        });
                    }
                } else {
                    ui.heading("Invitation copied");
                    ui.label("Send it to your friend in Discord or any chat.");
                    if ui.add(primary("Copy invitation")).clicked() {
                        let link = self
                            .hosting_setup
                            .invitation
                            .as_ref()
                            .unwrap()
                            .browser_link();
                        ui.output_mut(|o| o.copied_text = link);
                        self.notice = Some((
                            "Invitation copied — send it to your friend".into(),
                            Instant::now(),
                        ));
                    }
                    ui.label("Your friend opens the link in a browser and chooses Open Relay.");
                    if ui.add(secondary("Invite another friend")).clicked() {
                        self.hosting_setup.invitation = None;
                        self.hosting_setup.share_url.clear();
                        self.hosting_setup.existing_access = false;
                        self.hosting_setup.cancel_invitation_copy();
                    }
                }
                if matches!(sharing, Some(Sharing::Iroh | Sharing::Both)) {
                    self.hosting_setup.firewall_notice(ui);
                }
                self.connection_section(ui, sharing);
                ui.add_space(12.0);
                ui.label(
                    RichText::new("Keep this PC and Relay open while your friends use the server.")
                        .color(c_muted()),
                );
            }
            if let Some(error) = &self.error {
                ui.add_space(12.0);
                ui.label(RichText::new(error).color(c_bad()));
            }
            ui.add_space(16.0);
            ui.collapsing("Advanced", |ui| {
            if step == 3 && self.hosting_setup.invitation.is_none() {
                ui.add_enabled_ui(!self.busy && self.hosting_setup.pending_invitation_copy.is_none(), |ui| {
                    ui.checkbox(&mut self.hosting_setup.existing_access,"This friend already has access");
                });
                ui.separator();
            }
            ui.collapsing("Alternative: an existing Tailscale installation", |ui| {
            ui.label("Both people install Tailscale and sign in with their own accounts. Share host access with your friend, then send the Relay invitation. Your community stays on your PC.");
            ui.hyperlink_to("Install Tailscale", "https://tailscale.com/download/windows");
            ui.hyperlink_to("Share host access in Tailscale", "https://login.tailscale.com/admin/machines");
            ui.label(RichText::new("Tailscale sharing can also allow access to other services on the host. Its access rules determine what your friend can reach.").color(c_muted()));
            if ui.add_enabled(self.hosting_setup.tailscale_pending.is_none(), secondary(if self.hosting_setup.tailscale_pending.is_some() { "Checking Tailscale…" } else { "Check Tailscale" })).clicked() {
                let (sender, receiver) = mpsc::channel();
                self.hosting_setup.tailscale_pending = Some(receiver);
                self.hosting_setup.tailscale_result = None;
                let ctx = ctx.clone();
                std::thread::spawn(move || { let _ = sender.send(crate::tailscale_connect::check()); ctx.request_repaint(); });
            }
            if let Some(result) = self.hosting_setup.tailscale_result.clone() {
                match result {
                    Err(error) => { ui.label(RichText::new(error).color(c_bad())); }
                    Ok(status) => {
                        ui.label(status.message);
                        if let Some(address) = status.address {
                            let local = self.session.is_none() || crate::local_host::is_local_url(&self.login.url);
                            let available = local && !self.busy && self.voice_channel.is_none() && self.joining_voice.is_none() && self.pending_sends.is_empty();
                            if ui.add_enabled(available, primary("Start this community with Tailscale")).clicked() { self.start_tailscale_host(address); }
                            ui.label(RichText::new("Create the owner locally first. Stop this server in Server Settings before changing its sharing mode; the existing community and keys are kept.").color(c_muted()));
                        }
                    }
                }
            }
            if let Some(address) = &self.hosting_setup.hosted_address {
                ui.label(RichText::new("The server is listening on your Tailscale address.").color(c_good()));
                ui.label(address);
                ui.label("Friends can join after accepting your Tailscale sharing and your Relay invitation. Keep this PC and Tailscale connected.");
            }
            });
            ui.add_space(10.0);
            ui.label(RichText::new("Also offer these addresses").strong());
            ui.label(RichText::new("Friends try every address and use the fastest one that works. One address per line. Use https:// for anything on the internet.").color(c_muted()));
            if ui.add(egui::TextEdit::multiline(&mut self.hosting_setup.offer_text).desired_rows(3).desired_width(f32::INFINITY).hint_text("https://chat.example.org")).changed() {
                match crate::route_choice::parse_offer_lines(&self.hosting_setup.offer_text) {
                    Ok(list) => {
                        self.hosting_setup.offer_error = None;
                        self.hosting_setup.extra_addresses = list.clone();
                        self.prefs.connection.offer = list;
                        self.save_preferences();
                    }
                    Err(error) => self.hosting_setup.offer_error = Some(error),
                }
            }
            if let Some(error) = &self.hosting_setup.offer_error { ui.label(RichText::new(error).color(c_bad())); }
            ui.add_space(10.0);
            ui.label(RichText::new("Check internet access").strong());
            ui.label("This reads your network settings and looks up your public address. It does not open ports or publish your server.");
            if field(ui, "ROUTER WAN IPv4 (OPTIONAL)", &mut self.hosting_setup.router_wan, false).changed() { self.hosting_setup.result = None; }
            ui.label(RichText::new("Find WAN IPv4 on the router status page. Without it, shared internet access cannot be confirmed.").color(c_muted()));
            if ui.add_enabled(self.hosting_setup.pending.is_none(), secondary(if self.hosting_setup.pending.is_some() { "Checking network…" } else { "Check network" })).clicked() { self.hosting_setup.start(ctx); }
            if let Some(result) = &self.hosting_setup.result {
                ui.add_space(10.0);
                match result {
                    Err(error) => { ui.label(RichText::new(error).color(c_bad())); }
                    Ok(report) => {
                        if report.cgnat_status == "shared_address" {
                            ui.label(RichText::new("Your provider uses a shared address (CGNAT).").color(c_bad()).strong());
                            ui.label("Ask for a public IPv4 address to host directly. A dynamic public address is enough. Opening ports on this router alone will not solve it.");
                            if ui.button("Copy message for my internet provider").clicked() {
                                ui.output_mut(|output| output.copied_text = "Hej. Jeg vil gerne hoste en privat chat- og taleserver på min egen pc. Min router har en WAN-adresse i CGNAT-området 100.64.0.0/10. Kan I give min forbindelse en offentlig IPv4-adresse uden CGNAT og med mulighed for port forwarding? En dynamisk offentlig IPv4-adresse er tilstrækkelig. Oplys gerne, om det koster noget.".into());
                            }
                        } else if report.wan_status == "double_nat_possible" {
                            ui.label(RichText::new("There may be another router upstream.").color(c_bad()));
                            ui.label("Check the upstream router or ask your provider for direct public IPv4 access.");
                        } else {
                            ui.label(if report.wan_status == "public_address_matches" { "The router address matches the public address. Internet access still needs to be verified." } else { "Public access has not been confirmed. Check the router WAN address before sharing an invitation." });
                        }
                        if let Some(gateway) = &report.default_gateway { ui.hyperlink_to("Open my router", format!("http://{gateway}/")); }
                        if let Some(lan) = &report.lan_ipv4 { ui.label(format!("This PC on your network: {lan}")); }
                        if let Some(public) = &report.public_ipv4 { ui.label(format!("Observed public address: {public}")); }
                        ui.label(if report.upnp_available { "Automatic router control was detected. Port opening and internet access have not been verified." } else { "Automatic router control was not available. Manual setup may be needed once a public address is available." });
                        ui.add_space(10.0);
                        ui.label("A private network such as Tailscale can connect friends across CGNAT. Each host can keep their own server. Both sides need network access as well as a Relay invitation.");
                        ui.hyperlink_to("About Tailscale connections", "https://tailscale.com/docs/features/sharing");
                        if report.cgnat_status != "shared_address" {
                            ui.label(RichText::new("Direct hosting ports").strong());
                            egui::Grid::new("public-host-ports").show(ui, |ui| {
                                for (purpose, ports) in [("Secure connection", "443 / TCP"), ("Voice", "4581 / UDP"), ("Video and screen sharing", "7881 / TCP, 7882 / UDP")] { ui.label(purpose); ui.label(ports); ui.end_row(); }
                            });
                            ui.label("HTTPS and external access must work before the invitation is ready. Device approval remains a separate step in Encryption.");
                        }
                    }
                }
            }
                });
        });
        if go_to_account || close {
            open = false;
        }
        self.hosting_setup.open = open;
        if !open {
            self.hosting_setup.auto_share = false;
            self.hosting_setup.cancel_invitation_copy();
        }
    }
}

impl RelayApp {
    /// How your own server is shared, for the line under its name. `None`
    /// when this is not your own server or you cannot invite.
    pub(super) fn own_server_sharing(&self) -> Option<Option<Sharing>> {
        let b = self.bootstrap.as_ref()?;
        let own = self.session.is_some()
            && (crate::local_host::is_local_url(&self.login.url) || self.capture_local_host);
        if !own || !b.has_permission(PERMISSION_INVITES) {
            return None;
        }
        let tailscale = self.hosting_setup.hosted_address.is_some()
            || crate::embedded_network::shared_address(&b.server_id).is_some();
        if let Some(shown) = self.hosting_setup.capture_sharing {
            return Some(Some(shown));
        }
        Some(Sharing::current(
            tailscale,
            crate::iroh_route::hosted(&b.server_id).is_some(),
        ))
    }

    /// Asks the server for a new invitation code; the answer copies the link.
    fn request_invitation_copy(&mut self) {
        let Some(origin) = self.command_origin() else {
            return;
        };
        let request_id = self.next_request();
        if !self
            .hosting_setup
            .begin_invitation_copy(&origin, request_id)
        {
            return;
        }
        self.error = None;
        match self.network.send(Command::Invite {
            origin,
            request_id: Some(request_id),
        }) {
            Ok(()) => self.busy = true,
            Err(error) => {
                self.hosting_setup.cancel_invitation_copy();
                self.busy = false;
                self.error = Some(error);
            }
        }
    }

    pub(super) fn open_friend_sharing(&mut self) {
        if !self.hosting_setup.open {
            self.hosting_setup.cancel_invitation_copy();
            self.hosting_setup.invitation = None;
            self.hosting_setup.share_url.clear();
            self.hosting_setup.existing_access = false;
            // A half-made change from an earlier visit is not resumed.
            self.hosting_setup.change_open = false;
            self.hosting_setup.tailscale_step = false;
            self.hosting_setup.copy_when_shared = false;
            self.hosting_setup.firewall_checked = false;
            self.hosting_setup.firewall_error = None;
        }
        self.hosting_setup.open = true;
        self.error = None;
        self.hosting_setup.hosted_address = self
            .bootstrap
            .as_ref()
            .and_then(|b| crate::local_host::shared_address(&b.server_id));
    }

    fn private_sign_in_status(&mut self, ui: &mut egui::Ui) {
        if let Some(result) = &self.hosting_setup.embedded_result {
            match result {
                Err(error) => {
                    self.hosting_setup.auto_share = false;
                    ui.label(RichText::new(error).color(c_bad()));
                }
                Ok(status) if status.phase == "needs_login" => {
                    if let Some(auth) = &status.auth_url {
                        if crate::embedded_network::valid_auth_url(auth) {
                            ui.hyperlink_to("Sign in with Tailscale", auth);
                        }
                    }
                    ui.label(
                        "Finish signing in, then come back here. Relay continues automatically.",
                    );
                }
                Ok(status) if status.phase == "ready" => {
                    ui.label(RichText::new("Signed in").color(c_good()));
                }
                Ok(status) if matches!(status.phase.as_str(), "error" | "stopped") => {
                    ui.label(RichText::new(&status.message).color(c_bad()));
                }
                Ok(status) => {
                    ui.collapsing("Connection details", |ui| {
                        ui.label(&status.message);
                    });
                }
            }
        }
    }

    pub(super) fn friend_network_step(
        &mut self,
        ui: &mut egui::Ui,
        _invitation: &crate::invitation::Invitation,
    ) -> bool {
        self.friend_connection_step(ui)
    }

    pub(super) fn begin_friend_connection(&mut self, ctx: &egui::Context) {
        self.hosting_setup.access_opened = false;
        if !self.friend_connection_ready() {
            self.hosting_setup.check_embedded(ctx, true);
        }
    }

    fn friend_connection_ready(&self) -> bool {
        self.hosting_setup
            .embedded_result
            .as_ref()
            .is_some_and(|r| r.as_ref().is_ok_and(|s| s.phase == "ready"))
    }

    pub(super) fn friend_connection_step(&mut self, ui: &mut egui::Ui) -> bool {
        let ready = self.friend_connection_ready();
        if !ready {
            let waiting = self.hosting_setup.embedded_pending.is_some()
                || self
                    .hosting_setup
                    .embedded_result
                    .as_ref()
                    .is_some_and(|r| {
                        r.as_ref()
                            .is_ok_and(|s| matches!(s.phase.as_str(), "connecting" | "needs_login"))
                    });
            if waiting {
                ui.label("Preparing your private connection…");
            } else if ui
                .add_enabled(
                    self.hosting_setup.embedded_pending.is_none(),
                    primary("Try connection again"),
                )
                .clicked()
            {
                self.error = None;
                self.hosting_setup.check_embedded(ui.ctx(), true);
            }
            if self.hosting_setup.embedded_pending.is_some() {
                ui.spinner();
            }
            self.private_sign_in_status(ui);
            return false;
        }
        true
    }

    /// The last step's connection line: what is in use, and a way to change it.
    fn connection_section(&mut self, ui: &mut egui::Ui, sharing: Option<Sharing>) {
        let Some(current) = sharing else {
            return;
        };
        ui.add_space(14.0);
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Connection:").color(c_muted()));
            ui.label(RichText::new(current.label()).strong());
            let open = self.hosting_setup.change_open;
            if ui
                .add_enabled(
                    !self.busy,
                    secondary(if open { "Hide" } else { "Change connection" }),
                )
                .clicked()
            {
                self.hosting_setup.change_open = !open;
                self.hosting_setup.stop_invitations_confirm = false;
            }
        });
        if !self.hosting_setup.change_open {
            return;
        }
        ui.add_space(8.0);
        if self.busy {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Updating your server’s sharing settings…");
            });
            return;
        }
        if matches!(current, Sharing::Iroh | Sharing::Both) {
            ui.add_space(8.0);
            if self.hosting_setup.stop_invitations_confirm {
                ui.label("Anyone joining or reconnecting through iroh will need a new invitation. Current connections stay open. Tailscale invitations are unchanged.");
                ui.horizontal(|ui| {
                    if ui.add(secondary("Stop old invitations")).clicked() {
                        if let Some(origin) = self.command_origin() {
                            self.hosting_setup.invitations_stopped();
                            self.error = None;
                            self.busy = true;
                            self.send(Command::RotateIrohToken { origin });
                        }
                    }
                    if ui.add(secondary("Cancel")).clicked() {
                        self.hosting_setup.stop_invitations_confirm = false;
                    }
                });
            } else if ui.add(secondary("Stop old invitations…")).clicked() {
                self.hosting_setup.stop_invitations_confirm = true;
            }
            ui.add_space(8.0);
        }
        let blocker = change_blocker(
            self.voice_channel.is_some() || self.joining_voice.is_some(),
            !self.pending_sends.is_empty(),
        );
        match connection_chooser(ui, current, crate::iroh_route::available(), blocker) {
            Some(Choice::Iroh) => {
                self.error = None;
                self.start_iroh_host();
            }
            Some(Choice::Tailscale) => {
                // Tailscale first needs its own sign-in, which is the Connect step.
                self.error = None;
                self.hosting_setup.change_open = false;
                self.hosting_setup.tailscale_step = true;
            }
            None => {}
        }
    }

    fn start_embedded_host(&mut self, address: String) {
        let server = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.join("relay-server.exe")));
        let Some(server) = server.filter(|p| p.is_file()) else {
            self.error = Some("Extract the complete Relay package before hosting.".into());
            return;
        };
        let data = match crate::local_host::data_directory() {
            Ok(v) => v,
            Err(e) => {
                self.error = Some(e);
                return;
            }
        };
        self.busy = true;
        self.send(Command::HostEmbedded {
            server,
            data,
            address,
        });
    }
    /// Starts sharing without any account. The server runs in the same built-in
    /// private mode as for Tailscale, with no Tailscale involved.
    pub(super) fn start_iroh_host(&mut self) {
        let server = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.join("relay-server.exe")));
        let Some(server) = server.filter(|p| p.is_file()) else {
            self.error = Some("Extract the complete Relay package before hosting.".into());
            return;
        };
        let data = match crate::local_host::data_directory() {
            Ok(v) => v,
            Err(e) => {
                self.error = Some(e);
                return;
            }
        };
        self.busy = true;
        self.send(Command::HostIroh { server, data });
    }

    fn start_tailscale_host(&mut self, address: String) {
        if !crate::tailscale_connect::valid_address(&address) {
            return;
        }
        let server = std::env::current_exe()
            .ok()
            .and_then(|path| path.parent().map(|parent| parent.join("relay-server.exe")));
        let Some(server) = server.filter(|path| path.is_file()) else {
            self.error = Some("Extract the complete Relay package before hosting.".into());
            return;
        };
        let data = match crate::local_host::data_directory() {
            Ok(data) => data,
            Err(error) => {
                self.error = Some(error);
                return;
            }
        };
        self.busy = true;
        self.send(Command::HostTailscale {
            server,
            data,
            address,
        });
    }
}

impl HostSetup {
    fn check_embedded(&mut self, ctx: &egui::Context, connect: bool) {
        if self.embedded_pending.is_some() {
            return;
        }
        let (sender, receiver) = mpsc::channel();
        self.embedded_pending = Some(receiver);
        if connect {
            self.embedded_result = None;
        }
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = if connect {
                crate::embedded_network::connect()
            } else {
                crate::embedded_network::status()
            };
            let _ = sender.send(result);
            ctx.request_repaint();
        });
    }
}

impl RelayApp {
    /// A server on another computer, such as a Debian home server on the LAN,
    /// is already reachable at the address this app uses. Its administrators
    /// copy an ordinary invitation for that address; there is no sharing to set up.
    fn remote_invite_step(&mut self, ui: &mut egui::Ui) -> bool {
        let address = (self.session.is_some()
            && !crate::local_host::is_local_url(&self.login.url)
            && !self.capture_local_host)
            .then(|| self.login.url.clone());
        if self.hosting_setup.direct_address != address {
            self.hosting_setup.direct_address = address;
            self.hosting_setup.invitation = None;
            self.hosting_setup.cancel_invitation_copy();
        }
        let Some(address) = self.hosting_setup.direct_address.clone() else {
            return false;
        };
        let name = self
            .bootstrap
            .as_ref()
            .map(|b| b.server_name.clone())
            .unwrap_or_default();
        ui.heading(if name.is_empty() {
            "Invite friends to this server".to_owned()
        } else {
            format!("Invite friends to {name}")
        });
        ui.add_space(8.0);
        if !self
            .bootstrap
            .as_ref()
            .is_some_and(|b| b.has_permission(PERMISSION_INVITES))
        {
            ui.label("Only the server's administrators can create invitations. Ask the person who runs this server.");
            return true;
        }
        let local_only = self
            .hosting_setup
            .invitation
            .as_ref()
            .map(crate::invitation::Invitation::same_network_only)
            .unwrap_or_else(|| {
                crate::invitation::local_network_address(&address)
                    && self
                        .hosting_setup
                        .extra_addresses
                        .iter()
                        .all(|address| crate::invitation::local_network_address(address))
                    && crate::iroh_route::hosted(&self.bootstrap.as_ref().unwrap().server_id)
                        .is_none()
            });
        if local_only {
            local_invitation_notice(ui);
        }
        if let Some(invitation) = &self.hosting_setup.invitation {
            ui.label(RichText::new("Invitation copied").strong().color(c_good()));
            ui.label("Send it to your friend in Discord or any chat. They open the link in a browser and choose Open Relay.");
            ui.add_space(8.0);
            if ui
                .add(primary(if local_only {
                    "Copy local invitation"
                } else {
                    "Copy invitation"
                }))
                .clicked()
            {
                let link = invitation.browser_link();
                ui.output_mut(|o| o.copied_text = link);
                self.notice = Some((
                    "Invitation copied — send it to your friend".into(),
                    Instant::now(),
                ));
            }
            if ui.add(secondary("Invite another friend")).clicked() {
                self.hosting_setup.invitation = None;
            }
        } else {
            ui.label(format!(
                "The invitation uses this server's address: {address}"
            ));
            ui.label(
                RichText::new(crate::invitation::reachability_hint(&address)).color(c_muted()),
            );
            ui.add_space(8.0);
            let idle = !self.busy && self.hosting_setup.pending_invitation_copy.is_none();
            if ui
                .add_enabled(
                    idle,
                    primary(if local_only {
                        "Copy local invitation"
                    } else {
                        "Copy invitation"
                    }),
                )
                .clicked()
            {
                if let Some(origin) = self.command_origin() {
                    let request_id = self.next_request();
                    if self
                        .hosting_setup
                        .begin_invitation_copy(&origin, request_id)
                    {
                        self.error = None;
                        match self.network.send(Command::Invite {
                            origin,
                            request_id: Some(request_id),
                        }) {
                            Ok(()) => self.busy = true,
                            Err(error) => {
                                self.hosting_setup.cancel_invitation_copy();
                                self.error = Some(error);
                            }
                        }
                    }
                }
            }
            if self.hosting_setup.pending_invitation_copy.is_some() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Creating and copying your invitation…");
                });
            }
        }
        ui.add_space(12.0);
        ui.label(
            RichText::new("Keep the computer running this server on while your friends use it.")
                .color(c_muted()),
        );
        if let Some(error) = &self.error {
            ui.add_space(12.0);
            ui.label(RichText::new(error).color(c_bad()));
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_account_form_recovers_unfinished_setup_without_changing_credentials_or_protection() {
        let mut state = HostSetup::default();
        let mut login = crate::connection_setup::JoinFlow {
            username: "FirstOwner".into(),
            password: "synthetic-local-password".into(),
            registering: false, // A database was left by the interrupted first start.
            ..Default::default()
        };
        assert_eq!(
            state.creation_protection,
            relay_protocol::ProtectionLevel::Protected
        );
        state.local_account_ready(&mut login, Some(true));
        assert!(state.new_host && state.local_ready && login.registering);
        assert_eq!(login.username, "FirstOwner");
        assert_eq!(login.password, "synthetic-local-password");
        // Starting an already configured community selects Sign in, and must
        // not offer/send the first-owner encryption choice for another account.
        state.creation_protection = relay_protocol::ProtectionLevel::Open;
        state.local_account_ready(&mut login, Some(false));
        assert!(state.local_ready && !state.new_host && !login.registering);
        assert_eq!(
            state.creation_protection,
            relay_protocol::ProtectionLevel::Open
        );
        assert_eq!(login.password, "synthetic-local-password");
        for registering in [false, true] {
            login.registering = registering;
            state.new_host = registering;
            state.local_account_ready(&mut login, None);
            assert_eq!(
                login.registering, registering,
                "old servers keep the current choice"
            );
            assert_eq!(state.new_host, registering);
        }
    }

    fn sharing_fixture() -> (HostSetup, CommandOrigin) {
        let origin = CommandOrigin {
            url: "http://127.0.0.1:4580".into(),
            server_id: "4be44aef-8c98-4db6-a9d5-81c2eb311330".into(),
            user_id: "owner".into(),
        };
        let state = HostSetup {
            open: true,
            hosted_address: Some("http://100.76.3.4:4580".into()),
            share_url: "https://login.tailscale.com/admin/invite/friend_access".into(),
            ..Default::default()
        };
        (state, origin)
    }

    fn finish_copy_frame(
        ctx: &egui::Context,
        state: &mut HostSetup,
        request_id: u64,
        origin: &CommandOrigin,
        code: &str,
    ) -> (Result<bool, String>, String) {
        let mut result = None;
        let output = ctx.run(egui::RawInput::default(), |ctx| {
            result = Some(state.finish_invitation_copy(ctx, request_id, origin, code));
        });
        (result.unwrap(), output.platform_output.copied_text)
    }

    #[test]
    fn joining_moves_from_invitation_to_connect_to_account() {
        assert_eq!(join_step(false, false, false), 1, "nothing pasted yet");
        assert_eq!(join_step(false, true, false), 2, "measuring the routes");
        assert_eq!(join_step(true, false, false), 2, "checking the server");
        assert_eq!(join_step(true, false, true), 3, "the server answered");
    }

    #[test]
    fn the_status_line_says_plainly_who_can_join() {
        let (text, shared) = sharing_status(None);
        assert!(!shared && text.starts_with("Only this PC"), "{text}");
        assert!(
            text.contains("Invite friends"),
            "it says what to do: {text}"
        );
        for (way, word) in [
            (Sharing::Iroh, "without accounts"),
            (Sharing::Tailscale, "Tailscale"),
            (Sharing::Both, "Tailscale and without accounts"),
        ] {
            let (text, shared) = sharing_status(Some(way));
            assert!(shared && text.starts_with("Friends can join"), "{text}");
            assert!(text.ends_with(word), "{text}");
        }
    }

    #[test]
    fn one_click_copies_the_invitation_once_the_route_is_up_and_only_once() {
        let mut state = HostSetup {
            open: true,
            copy_when_shared: true,
            ..Default::default()
        };
        // Not yet: the route is still starting, or Relay is busy.
        assert!(!state.ready_to_auto_copy(false, false));
        assert!(!state.ready_to_auto_copy(true, true));
        // The route is up: copy now.
        assert!(state.ready_to_auto_copy(true, false));
        // Only for the account-free way; a Tailscale server keeps its own steps.
        let tailscale = HostSetup {
            open: true,
            copy_when_shared: true,
            hosted_address: Some("http://100.76.3.4:4580".into()),
            ..Default::default()
        };
        assert!(!tailscale.ready_to_auto_copy(true, false));
        // Without the button press nothing is copied by itself.
        state.copy_when_shared = false;
        assert!(!state.ready_to_auto_copy(true, false));
        // A finished switch to the account-free route keeps the request.
        state.copy_when_shared = true;
        state.iroh_ready(true);
        assert!(state.copy_when_shared && state.ready_to_auto_copy(true, false));
    }

    fn views(current: Sharing, iroh: bool, blocker: Option<&'static str>) -> [ChoiceView; 2] {
        connection_choices(current, iroh, blocker)
    }

    #[test]
    fn both_ways_are_always_offered_with_the_current_one_marked() {
        let tailscale = views(Sharing::Tailscale, true, None);
        assert_eq!(tailscale[0].choice, Choice::Iroh);
        assert_eq!(tailscale[0].title, "Without accounts (iroh)");
        assert!(tailscale[0].enabled, "a Tailscale server can move to iroh");
        assert_eq!(tailscale[1].choice, Choice::Tailscale);
        assert_eq!(tailscale[1].button, "In use");
        assert!(!tailscale[1].enabled);

        let iroh = views(Sharing::Iroh, true, None);
        assert_eq!(iroh[0].button, "In use");
        assert!(!iroh[0].enabled && iroh[1].enabled);

        // An older setup with both: either way can be chosen to make it one.
        let both = views(Sharing::Both, true, None);
        assert!(both[0].enabled && both[1].enabled);
        assert_eq!(Sharing::Both.label(), "Tailscale and without accounts");
        assert_eq!(Sharing::Tailscale.label(), "With Tailscale");
    }

    #[test]
    fn the_chooser_says_what_each_way_gives_in_plain_words() {
        let shown = views(Sharing::Tailscale, true, None);
        for way in &shown {
            assert!(way.about.contains("Chat, voice, video and screen sharing"));
        }
        assert!(shown[0].about.contains("No extra network account"));
        assert!(shown[1].about.contains("sign in to Tailscale"));
    }

    #[test]
    fn a_call_or_a_message_being_sent_stops_the_change_and_says_why() {
        let call = change_blocker(true, false).unwrap();
        assert!(call.contains("Leave the voice call"), "{call}");
        assert!(change_blocker(false, true)
            .unwrap()
            .contains("finish sending"));
        assert!(change_blocker(false, false).is_none());
        for shown in views(Sharing::Both, true, Some(call)) {
            assert!(!shown.enabled, "{} cannot start during a call", shown.title);
        }
        // Without the helper, iroh is not offered, and Relay says so.
        let missing = views(Sharing::Tailscale, false, None);
        assert!(!missing[0].enabled && missing[0].unavailable.is_some());
    }

    #[test]
    fn the_chooser_fits_narrow_windows_and_large_text() {
        for (width, font_scale) in [(520.0, 1.0), (360.0, 1.0), (320.0, 1.4)] {
            let ctx = egui::Context::default();
            crate::appearance::AppearanceSettings {
                font_scale,
                ..Default::default()
            }
            .apply(&ctx, None);
            for current in [Sharing::Tailscale, Sharing::Iroh, Sharing::Both] {
                for _ in 0..3 {
                    let mut used = Rect::NOTHING;
                    let _ = ctx.run(
                        egui::RawInput {
                            screen_rect: Some(Rect::from_min_size(
                                Pos2::ZERO,
                                Vec2::new(width, 900.0),
                            )),
                            ..Default::default()
                        },
                        |ctx| {
                            egui::CentralPanel::default().show(ctx, |ui| {
                                connection_chooser(ui, current, true, None);
                                used = ui.min_rect();
                            });
                        },
                    );
                    assert!(
                        used.right() <= width + 0.5,
                        "{current:?} at {width} x{font_scale}: {used:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn switching_from_tailscale_to_iroh_leaves_nothing_of_the_old_way() {
        let (mut state, origin) = sharing_fixture();
        let ctx = egui::Context::default();
        // An invitation was copied while the server was shared through Tailscale.
        assert!(state.begin_invitation_copy(&origin, 1));
        let (copied_ok, old) = finish_copy_frame(&ctx, &mut state, 1, &origin, "old_code");
        assert!(copied_ok.unwrap());
        assert!(
            old.contains("100.76.3.4"),
            "the old invitation named Tailscale"
        );
        assert!(state.invitation.is_some());
        state.change_open = true;
        // A second copy is still waiting for its answer when the change completes.
        assert!(state.begin_invitation_copy(&origin, 2));

        state.iroh_ready(true);

        assert!(
            state.hosted_address.is_none(),
            "the Tailscale address is gone"
        );
        assert!(state.iroh_only);
        assert!(
            state.invitation.is_none(),
            "the copied invitation is dropped"
        );
        assert!(state.share_url.is_empty(), "the access link is dropped");
        assert!(!state.existing_access && !state.change_open && !state.tailscale_step);
        assert!(state.pending_invitation_copy.is_none());
        // The answer to the copy which was waiting is ignored, so it cannot
        // produce an invitation with the old address.
        let (late, text) = finish_copy_frame(&ctx, &mut state, 2, &origin, "late_code");
        assert!(!late.unwrap() && text.is_empty());
        // Without the helper no route is hosted in this test, so no new draft
        // exists either, and never one for the old Tailscale address.
        assert!(!state.begin_invitation_copy(&origin, 3));
        assert_eq!(state.sharing(true), Some(Sharing::Iroh));
        assert_eq!(state.sharing(false), None);
    }

    #[test]
    fn switching_from_iroh_to_tailscale_gives_the_new_address_and_asks_for_new_access() {
        let origin = CommandOrigin {
            url: "http://127.0.0.1:4580".into(),
            server_id: "4be44aef-8c98-4db6-a9d5-81c2eb311330".into(),
            user_id: "owner".into(),
        };
        let ctx = egui::Context::default();
        let mut state = HostSetup {
            open: true,
            iroh_only: true,
            tailscale_step: true,
            change_open: true,
            share_url: "https://login.tailscale.com/admin/invite/stale".into(),
            existing_access: true,
            ..Default::default()
        };
        state.tailscale_ready("http://100.76.3.9:4580".into());
        assert_eq!(
            state.hosted_address.as_deref(),
            Some("http://100.76.3.9:4580")
        );
        assert!(!state.iroh_only && !state.tailscale_step && !state.change_open);
        assert!(state.share_url.is_empty() && !state.existing_access);
        // Without an access link no invitation can be made yet: the host is asked for one.
        assert!(!state.begin_invitation_copy(&origin, 1));
        state.share_url = "https://login.tailscale.com/admin/invite/fresh".into();
        assert!(state.begin_invitation_copy(&origin, 2));
        let (result, copied) = finish_copy_frame(&ctx, &mut state, 2, &origin, "new_code");
        assert!(result.unwrap());
        let invitation = crate::invitation::Invitation::parse(&copied).unwrap();
        assert_eq!(invitation.server, "http://100.76.3.9:4580");
        assert_eq!(invitation.code, "new_code");
        assert!(invitation.iroh.is_none(), "no account-free key from before");
        assert!(copied.find("stale").is_none());
    }

    #[test]
    fn invitation_response_copies_once_with_both_capabilities_in_the_fragment() {
        let (mut state, origin) = sharing_fixture();
        let ctx = egui::Context::default();
        assert!(state.begin_invitation_copy(&origin, 41));
        assert!(!state.begin_invitation_copy(&origin, 42));
        let (result, copied) = finish_copy_frame(&ctx, &mut state, 41, &origin, "relay_code");
        assert!(result.unwrap());
        let invitation = crate::invitation::Invitation::parse(&copied).unwrap();
        assert_eq!(invitation.server, "http://100.76.3.4:4580");
        assert_eq!(invitation.server_id, origin.server_id);
        assert_eq!(invitation.code, "relay_code");
        assert_eq!(
            invitation.network_share.as_deref(),
            Some(state.share_url.as_str())
        );
        let link = url::Url::parse(&copied).unwrap();
        assert!(link.query().is_none());
        assert_eq!(
            copied.split('#').next().unwrap(),
            crate::invitation::BROWSER_INVITATION_URL
        );
        let fragment: std::collections::HashMap<_, _> =
            url::form_urlencoded::parse(link.fragment().unwrap().as_bytes()).collect();
        assert_eq!(
            fragment.get("server").map(|v| v.as_ref()),
            Some("http://100.76.3.4:4580")
        );
        assert_eq!(
            fragment.get("invite").map(|v| v.as_ref()),
            Some("relay_code")
        );
        assert_eq!(
            fragment.get("access").map(|v| v.as_ref()),
            Some(state.share_url.as_str())
        );
        assert!(state.pending_invitation_copy.is_none());
        let (again, copied_again) = finish_copy_frame(&ctx, &mut state, 41, &origin, "relay_code");
        assert!(!again.unwrap());
        assert!(copied_again.is_empty());
    }

    #[test]
    fn remote_lan_server_copies_a_plain_invitation_with_its_own_address() {
        let origin = CommandOrigin {
            url: "http://192.168.1.50:4580".into(),
            server_id: "4be44aef-8c98-4db6-a9d5-81c2eb311330".into(),
            user_id: "admin".into(),
        };
        assert!(!crate::local_host::is_local_url(&origin.url));
        let mut state = HostSetup {
            open: true,
            direct_address: Some(origin.url.clone()),
            ..Default::default()
        };
        let ctx = egui::Context::default();
        assert!(state.begin_invitation_copy(&origin, 7));
        let (result, copied) = finish_copy_frame(&ctx, &mut state, 7, &origin, "lan_code");
        assert!(result.unwrap());
        let invitation = crate::invitation::Invitation::parse(&copied).unwrap();
        assert_eq!(invitation.server, "http://192.168.1.50:4580");
        assert_eq!(invitation.server_id, origin.server_id);
        assert_eq!(invitation.code, "lan_code");
        assert!(invitation.network_share.is_none());
        assert!(state.invitation == Some(invitation));

        // A response for a different server address is not copied.
        assert!(state.begin_invitation_copy(&origin, 8));
        state.direct_address = Some("http://192.168.1.51:4580".into());
        let (stale, copied) = finish_copy_frame(&ctx, &mut state, 8, &origin, "lan_code");
        assert!(!stale.unwrap());
        assert!(copied.is_empty());
    }

    #[test]
    fn extra_addresses_from_advanced_travel_in_the_invitation_and_old_ones_are_unchanged() {
        let origin = CommandOrigin {
            url: "http://192.168.1.50:4580".into(),
            server_id: "4be44aef-8c98-4db6-a9d5-81c2eb311330".into(),
            user_id: "admin".into(),
        };
        let ctx = egui::Context::default();
        let mut state = HostSetup {
            open: true,
            direct_address: Some(origin.url.clone()),
            extra_addresses: vec!["https://chat.example.org".into()],
            ..Default::default()
        };
        assert!(state.begin_invitation_copy(&origin, 1));
        let (result, copied) = finish_copy_frame(&ctx, &mut state, 1, &origin, "lan_code");
        assert!(result.unwrap());
        assert!(
            copied.starts_with("https://thebengine.github.io/Relay/#v=4&"),
            "{copied}"
        );
        let invitation = crate::invitation::Invitation::parse(&copied).unwrap();
        assert_eq!(invitation.server, "http://192.168.1.50:4580");
        assert_eq!(invitation.alt, ["https://chat.example.org"]);
        assert_eq!(invitation.code, "lan_code");
        assert!(!copied.split('#').next().unwrap().contains("lan_code"));
        // Without extra addresses the invitation keeps its older, smaller format.
        let mut plain = HostSetup {
            open: true,
            direct_address: Some(origin.url.clone()),
            ..Default::default()
        };
        assert!(plain.begin_invitation_copy(&origin, 2));
        let (_, copied) = finish_copy_frame(&ctx, &mut plain, 2, &origin, "lan_code");
        assert!(
            copied.starts_with("https://thebengine.github.io/Relay/#v=1&"),
            "{copied}"
        );
    }

    /// Needs the helper; run with `RELAY_IROH_HELPER=<debug relay-iroh.exe> --include-ignored --test-threads=1`.
    #[test]
    #[ignore = "needs the relay-iroh helper (RELAY_IROH_HELPER)"]
    fn an_account_free_host_invites_with_its_key_and_token_and_never_for_another_community() {
        const OWN: &str = "4be44aef-8c98-4db6-a9d5-81c2eb311330";
        let route = crate::iroh_route::host(
            4999,
            "an-ingress-capability-for-the-peer-listener-0123456789",
            OWN,
        )
        .expect("the helper hosts");
        let origin = CommandOrigin {
            url: "http://127.0.0.1:4580".into(),
            server_id: OWN.into(),
            user_id: "owner".into(),
        };
        let ctx = egui::Context::default();

        // Shared only without accounts: the key is the address, and no access link is asked for.
        let mut only = HostSetup {
            open: true,
            ..Default::default()
        };
        assert!(only.begin_invitation_copy(&origin, 1));
        let (result, copied) = finish_copy_frame(&ctx, &mut only, 1, &origin, "iroh_code");
        assert!(result.unwrap());
        let invitation = crate::invitation::Invitation::parse(&copied).unwrap();
        assert_eq!(invitation.server, route.address());
        assert_eq!(
            invitation.iroh.as_ref().map(|r| r.endpoint_id.as_str()),
            Some(route.endpoint_id.as_str())
        );
        assert_eq!(
            invitation.iroh.as_ref().map(|r| r.token.as_str()),
            Some(route.token.as_str())
        );
        assert_eq!(invitation.code, "iroh_code");
        assert!(
            !copied.split('#').next().unwrap().contains(&route.token),
            "the token stays in the fragment"
        );

        // Shared both ways: Tailscale stays the main address and the key comes along.
        let (mut both, _) = sharing_fixture();
        assert!(both.begin_invitation_copy(&origin, 2));
        let (result, copied) = finish_copy_frame(&ctx, &mut both, 2, &origin, "both_code");
        assert!(result.unwrap());
        let invitation = crate::invitation::Invitation::parse(&copied).unwrap();
        assert_eq!(invitation.server, "http://100.76.3.4:4580");
        assert!(invitation.network_share.is_some() && invitation.iroh.is_some());

        // Moved to the account-free route only: the old Tailscale invitation is gone
        // and the next one carries the key, never the Tailscale address or access link.
        assert!(
            both.invitation.is_some(),
            "the Tailscale invitation was copied"
        );
        both.iroh_ready(true);
        assert!(both.invitation.is_none());
        assert!(both.begin_invitation_copy(&origin, 20));
        let (result, copied) = finish_copy_frame(&ctx, &mut both, 20, &origin, "moved_code");
        assert!(result.unwrap());
        assert!(!copied.contains("100.76.3.4") && !copied.contains("friend_access"));
        let moved = crate::invitation::Invitation::parse(&copied).unwrap();
        assert_eq!(moved.server, route.address());
        assert!(moved.network_share.is_none());
        assert_eq!(
            moved.iroh.as_ref().map(|r| r.token.as_str()),
            Some(route.token.as_str())
        );
        assert_eq!(moved.code, "moved_code");

        // Someone else's server never gets this community's key.
        let other = CommandOrigin {
            url: "http://192.168.1.50:4580".into(),
            server_id: "dc310e12-b973-47c8-99e4-d71a40dc3c12".into(),
            user_id: "admin".into(),
        };
        let mut remote = HostSetup {
            open: true,
            direct_address: Some(other.url.clone()),
            ..Default::default()
        };
        assert!(remote.begin_invitation_copy(&other, 3));
        let (_, copied) = finish_copy_frame(&ctx, &mut remote, 3, &other, "remote_code");
        let invitation = crate::invitation::Invitation::parse(&copied).unwrap();
        assert!(invitation.iroh.is_none() && invitation.server == "http://192.168.1.50:4580");
        // A community which is not shared this way offers no draft without Tailscale.
        let mut nothing = HostSetup {
            open: true,
            ..Default::default()
        };
        assert!(!nothing.begin_invitation_copy(&other, 4));
        crate::iroh_route::rotate(OWN).unwrap();
        let fresh = crate::iroh_route::hosted(OWN).unwrap();
        assert_eq!(fresh.endpoint_id, route.endpoint_id);
        assert_ne!(fresh.token, route.token);
        only.invitations_stopped();
        assert!(only.begin_invitation_copy(&origin, 30));
        let (_, copied) = finish_copy_frame(&ctx, &mut only, 30, &origin, "fresh_code");
        let invitation = crate::invitation::Invitation::parse(&copied).unwrap();
        assert_eq!(invitation.iroh.unwrap().token, fresh.token);
        crate::iroh_route::shutdown();
    }

    #[test]
    fn stopping_iroh_invitations_cancels_stale_copy_but_keeps_tailscale_access() {
        let (mut state, origin) = sharing_fixture();
        let ctx = egui::Context::default();
        assert!(state.begin_invitation_copy(&origin, 41));
        state.copy_when_shared = true;
        state.stop_invitations_confirm = true;
        let share = state.share_url.clone();
        let address = state.hosted_address.clone();
        state.invitations_stopped();
        assert!(state.invitation.is_none() && state.pending_invitation_copy.is_none());
        assert!(!state.copy_when_shared && !state.stop_invitations_confirm);
        assert_eq!(state.share_url, share);
        assert_eq!(state.hosted_address, address);
        let (_, copied) = finish_copy_frame(&ctx, &mut state, 41, &origin, "old_code");
        assert!(
            copied.is_empty(),
            "a queued old response cannot copy an invitation"
        );
        assert!(state.begin_invitation_copy(&origin, 42));
    }

    #[test]
    fn invitation_response_rejects_changed_origin_address_share_or_access() {
        // Each case begins with a real pending draft. The response cannot copy
        // a capability for a different account, destination, or recipient grant.
        for change in 0..6 {
            let (mut state, mut origin) = sharing_fixture();
            assert!(state.begin_invitation_copy(&origin, 41));
            match change {
                0 => origin.url = "http://localhost:4580".into(),
                1 => origin.server_id = "dc310e12-b973-47c8-99e4-d71a40dc3c12".into(),
                2 => origin.user_id = "another-owner".into(),
                3 => state.hosted_address = Some("http://100.76.3.5:4580".into()),
                4 => {
                    state.share_url =
                        "https://login.tailscale.com/admin/invite/different_access".into()
                }
                5 => state.existing_access = true,
                _ => unreachable!(),
            }
            let (result, copied) = finish_copy_frame(
                &egui::Context::default(),
                &mut state,
                41,
                &origin,
                "relay_code",
            );
            assert!(!result.unwrap(), "changed draft {change} was accepted");
            assert!(copied.is_empty());
            assert!(state.invitation.is_none());
            assert!(state.pending_invitation_copy.is_none());
        }
    }

    #[test]
    fn old_response_after_close_and_reopen_cannot_cancel_new_invitation_copy() {
        let (mut state, origin) = sharing_fixture();
        let ctx = egui::Context::default();
        assert!(state.begin_invitation_copy(&origin, 41));
        state.open = false;
        let (closed, copied) = finish_copy_frame(&ctx, &mut state, 41, &origin, "old_code");
        assert!(!closed.unwrap());
        assert!(copied.is_empty());
        assert!(state.pending_invitation_copy.is_none());
        assert!(!state.begin_invitation_copy(&origin, 42));
        state.open = true;
        assert!(state.begin_invitation_copy(&origin, 42));
        let (stale, copied) = finish_copy_frame(&ctx, &mut state, 41, &origin, "old_code");
        assert!(!stale.unwrap());
        assert!(copied.is_empty());
        assert_eq!(
            state.pending_invitation_copy.as_ref().unwrap().request_id,
            42
        );
        assert!(!state.fail_invitation_copy(41));
        let (current, copied) = finish_copy_frame(&ctx, &mut state, 42, &origin, "new_code");
        assert!(current.unwrap());
        assert_eq!(
            crate::invitation::Invitation::parse(&copied).unwrap().code,
            "new_code"
        );
    }

    #[test]
    fn invitation_failure_timeout_and_malformed_code_release_retry() {
        let (mut state, origin) = sharing_fixture();
        let ctx = egui::Context::default();
        assert!(state.begin_invitation_copy(&origin, 41));
        assert!(!state.fail_invitation_copy(40));
        assert!(state.fail_invitation_copy(41));
        assert!(state.pending_invitation_copy.is_none());
        assert!(state.begin_invitation_copy(&origin, 42));
        let started = state.pending_invitation_copy.as_ref().unwrap().started;
        assert!(!state.expire_invitation_copy(started + Duration::from_secs(29)));
        assert!(state.expire_invitation_copy(started + Duration::from_secs(30)));
        assert!(state.begin_invitation_copy(&origin, 43));
        let (malformed, copied) = finish_copy_frame(&ctx, &mut state, 43, &origin, "invalid code");
        assert!(malformed.is_err());
        assert!(copied.is_empty());
        assert!(state.pending_invitation_copy.is_none());
        assert!(state.begin_invitation_copy(&origin, 44));
        state.pending_invitation_copy.as_mut().unwrap().started =
            Instant::now() - Duration::from_secs(31);
        let (late, copied) = finish_copy_frame(&ctx, &mut state, 44, &origin, "late_code");
        assert!(!late.unwrap());
        assert!(copied.is_empty());
        assert!(state.begin_invitation_copy(&origin, 45));
        let (retry, copied) = finish_copy_frame(&ctx, &mut state, 45, &origin, "retry_code");
        assert!(retry.unwrap());
        assert!(!copied.is_empty());
    }

    #[test]
    fn sharing_page_only_opens_the_fixed_console_filtered_to_the_private_host() {
        let page = sharing_page_url("http://100.76.3.4:4580").unwrap();
        let page = url::Url::parse(&page).unwrap();
        assert_eq!(page.scheme(), "https");
        assert_eq!(page.host_str(), Some("console.tailscale.com"));
        assert_eq!(page.path(), "/admin/machines");
        assert_eq!(page.query(), Some("q=100.76.3.4"));
        assert!(page.username().is_empty() && page.password().is_none());
        assert!(page.fragment().is_none());
        for invalid in [
            "http://relay.example:4580",
            "https://100.76.3.4:4580",
            "http://127.0.0.1:4580",
            "http://localhost:4580",
            "http://192.168.1.2:4580",
            "http://100.63.255.255:4580",
            "http://100.128.0.1:4580",
            "http://100.76.3.4:4581",
            "http://100.76.3.4:4580/admin",
            "http://100.76.3.4:4580/?q=someone-else",
            "http://100.76.3.4:4580/#fragment",
            "http://user@100.76.3.4:4580",
            "http://user:password@100.76.3.4:4580",
            "http://100.76.3.4.evil.example:4580",
            "https://console.tailscale.com/admin/machines?q=100.76.3.4",
        ] {
            assert!(sharing_page_url(invalid).is_none(), "accepted {invalid}");
        }
    }

    #[test]
    fn stopped_private_connection_check_does_not_leave_an_endless_spinner() {
        let (sender, receiver) = mpsc::channel();
        let mut state = HostSetup {
            embedded_pending: Some(receiver),
            ..Default::default()
        };
        drop(sender);
        state.poll(&egui::Context::default());
        assert!(state.embedded_pending.is_none());
        assert!(matches!(state.embedded_result, Some(Err(_))));
    }
    fn report(wan: &str, classification: &str) -> HostReport {
        serde_json::from_value(serde_json::json!({"schema_version":1,"lan_ipv4":"192.168.1.2","default_gateway":"192.168.1.1","public_ipv4":"1.2.3.4","router_wan_ipv4":wan,"upnp_available":false,"wan_status":"upstream_nat","cgnat_status":classification})).unwrap()
    }
    /// One frame of Invite friends (`invite`) or Join friends with a body
    /// taller than the dialog. Returns (close, window rect, body clip rect).
    fn header_dialog_frame(
        ctx: &egui::Context,
        screen: Rect,
        invite: bool,
        events: Vec<egui::Event>,
    ) -> (bool, Rect, Rect) {
        let title = if invite {
            "Invite friends"
        } else {
            "Join friends"
        };
        let mut close = false;
        let mut body = Rect::NOTHING;
        let _ = ctx.run(
            egui::RawInput {
                screen_rect: Some(screen),
                focused: true,
                events,
                ..Default::default()
            },
            |ctx| {
                let mut open = true;
                let window = if invite {
                    invite_friends_window(ctx, &mut open)
                } else {
                    onboarding_ui::join_friends_window(ctx, &mut open)
                };
                close = show_with_header(ctx, window, title, |ui| {
                    body = ui.clip_rect();
                    for _ in 0..40 {
                        ui.label("Invitation step");
                    }
                    let _ = ui.button("Last action");
                });
            },
        );
        let window = ctx
            .memory(|memory| memory.area_rect(egui::Id::new(title)))
            .unwrap();
        (close, window, body)
    }

    #[test]
    fn invite_and_join_keep_the_header_in_place_and_close_from_it() {
        let key = |key| egui::Event::Key {
            key,
            physical_key: Some(key),
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        for invite in [true, false] {
            for (size, font_scale) in [
                (Vec2::new(940.0, 650.0), 1.0),
                (Vec2::new(360.0, 420.0), 1.0),
                (Vec2::new(360.0, 420.0), 1.4),
            ] {
                let case = format!("invite {invite}, {size:?} x{font_scale}");
                let fresh = || {
                    let ctx = egui::Context::default();
                    crate::appearance::AppearanceSettings {
                        font_scale,
                        ..Default::default()
                    }
                    .apply(&ctx, None);
                    let screen = Rect::from_min_size(Pos2::ZERO, size);
                    let mut frame = header_dialog_frame(&ctx, screen, invite, vec![]);
                    for _ in 0..3 {
                        frame = header_dialog_frame(&ctx, screen, invite, vec![]);
                    }
                    (ctx, screen, frame)
                };
                let (ctx, screen, (_, window, body)) = fresh();
                assert!(screen.contains_rect(window), "{case}: {window:?}");
                // Only the body scrolls: it starts below the header and is cut
                // off at the bottom of the dialog.
                assert!(body.top() > window.top() + 24.0, "{case}: {body:?}");
                assert!(body.bottom() <= window.bottom(), "{case}");
                // Scroll the body; the header and its Close stay where they were.
                let over_body = egui::Event::PointerMoved(body.center());
                for _ in 0..10 {
                    header_dialog_frame(
                        &ctx,
                        screen,
                        invite,
                        vec![
                            over_body.clone(),
                            egui::Event::Scroll(Vec2::new(0.0, -400.0)),
                        ],
                    );
                }
                let (_, after, scrolled) = header_dialog_frame(&ctx, screen, invite, vec![]);
                assert_eq!((after, scrolled.top()), (window, body.top()), "{case}");
                let close = Pos2::new(window.right() - 24.0, (window.top() + body.top()) / 2.0);
                let press = |pressed| egui::Event::PointerButton {
                    pos: close,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                };
                let (closed, ..) = header_dialog_frame(
                    &ctx,
                    screen,
                    invite,
                    vec![egui::Event::PointerMoved(close), press(true), press(false)],
                );
                assert!(closed, "{case}: Close after scrolling");
                // Escape closes and other keys do not.
                let (ctx, screen, _) = fresh();
                let (closed, ..) =
                    header_dialog_frame(&ctx, screen, invite, vec![key(egui::Key::Enter)]);
                assert!(!closed, "{case}");
                let (closed, ..) =
                    header_dialog_frame(&ctx, screen, invite, vec![key(egui::Key::Escape)]);
                assert!(closed, "{case}: Escape");
                // Close is the first keyboard stop after the window itself.
                let (ctx, screen, _) = fresh();
                header_dialog_frame(&ctx, screen, invite, vec![key(egui::Key::Tab)]);
                header_dialog_frame(&ctx, screen, invite, vec![key(egui::Key::Tab)]);
                let (closed, ..) =
                    header_dialog_frame(&ctx, screen, invite, vec![key(egui::Key::Enter)]);
                assert!(closed, "{case}: Tab, Tab, Enter");
            }
        }
    }

    #[test]
    fn shared_diagnosis_requires_a_shared_router_address() {
        assert!(report("100.127.1.2", "shared_address").validate().is_ok());
        assert!(report("192.168.1.2", "shared_address").validate().is_err());
        assert!(report("1.2.3.4", "shared_address").validate().is_err());
    }
    #[test]
    fn editing_router_address_discards_old_probe_result() {
        let (sender, receiver) = mpsc::channel();
        let mut state = HostSetup {
            router_wan: "1.2.3.4".into(),
            pending: Some(("100.127.1.2".into(), receiver)),
            ..Default::default()
        };
        sender
            .send(Ok(report("100.127.1.2", "shared_address")))
            .unwrap();
        state.poll(&egui::Context::default());
        assert!(state.pending.is_none());
        assert!(state.result.is_none());
    }
}
