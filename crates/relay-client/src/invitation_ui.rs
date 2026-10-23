use super::*;
use crate::appearance::{TEXT_BODY, TEXT_CAPTION};
use crate::connection_setup::FlowKind;
use crate::{connection_setup::SetupState, invitation::Invitation};

fn setup_controls(
    ui: &mut egui::Ui,
    address: &mut String,
    invite_code: &mut String,
    registering: &mut bool,
    state: &mut SetupState,
    enabled: bool,
) -> Option<String> {
    if field(ui, "SERVER ADDRESS OR INVITATION", address, false).changed() {
        state.edited(invite_code);
    }
    if address.trim().starts_with("relay:")
        || address
            .trim()
            .starts_with(crate::invitation::BROWSER_INVITATION_URL)
        || Invitation::parse(address).is_ok()
    {
        if ui
            .add_enabled(enabled, secondary("Use invitation"))
            .clicked()
        {
            match Invitation::parse(address) {
                Ok(invitation) => {
                    *address = invitation.server.clone();
                    *invite_code = invitation.code.clone();
                    *registering = true;
                    state.invitation = Some(invitation);
                    state.pending = None;
                    state.result = None;
                }
                Err(error) => state.result = Some(Err(error)),
            }
        }
    } else {
        ui.label(
            RichText::new(crate::invitation::reachability_hint(address))
                .size(font_size(TEXT_CAPTION))
                .color(c_muted()),
        );
        if state.invitation.is_some() {
            ui.label(
                RichText::new("Invitation ready · Enter your own username and password.")
                    .size(font_size(12.0))
                    .color(c_good()),
            );
        }
        let can_check =
            enabled && state.pending.is_none() && crate::community::normalize_url(address).is_ok();
        if ui
            .add_enabled(
                can_check,
                secondary(if state.pending.is_some() {
                    "Checking…"
                } else {
                    "Check connection"
                }),
            )
            .clicked()
        {
            state.result = None;
            return crate::community::normalize_url(address).ok();
        }
    }
    if let Some(result) = &state.result {
        match result {
            Ok(server) => {
                ui.label(
                    RichText::new(format!("Connected to {}", server.server_name))
                        .size(font_size(12.0))
                        .color(c_good()),
                );
                ui.label(
                    RichText::new(if server.rtc_enabled {
                        "Voice and video are available."
                    } else {
                        "Chat is available. The host has not enabled voice and video."
                    })
                    .size(font_size(TEXT_BODY))
                    .color(c_muted()),
                );
            }
            Err(error) => {
                ui.label(RichText::new(error).size(font_size(12.0)).color(c_bad()));
            }
        }
    }
    ui.add_space(8.0);
    None
}

impl RelayApp {
    pub(super) fn setup_controls(&mut self, ui: &mut egui::Ui, kind: FlowKind) {
        let previous = if kind == FlowKind::AddCommunity {
            self.add_community.setup.invitation.clone()
        } else {
            self.login.setup.invitation.clone()
        };
        let target = if kind == FlowKind::AddCommunity {
            setup_controls(
                ui,
                &mut self.add_community.url,
                &mut self.add_community.invite,
                &mut self.add_community.registering,
                &mut self.add_community.setup,
                !self.busy,
            )
        } else {
            setup_controls(
                ui,
                &mut self.login.url,
                &mut self.login.invite,
                &mut self.login.registering,
                &mut self.login.setup,
                !self.busy,
            )
        };
        if let Some(url) = target {
            let request_id = self.next_request();
            let state = if kind == FlowKind::AddCommunity {
                &mut self.add_community.setup
            } else {
                &mut self.login.setup
            };
            match state.begin_check(&url, request_id, true) {
                Ok(Some(url)) => {
                    if let Err(error) = self.network.send(Command::CheckServer {
                        url: url.clone(),
                        request_id,
                    }) {
                        state.accept(&url, &url, request_id, Err(error));
                    }
                }
                Ok(None) => {}
                Err(error) => state.result = Some(Err(error)),
            }
        }
        let invitation = if kind == FlowKind::AddCommunity {
            self.add_community.setup.invitation.clone()
        } else {
            self.login.setup.invitation.clone()
        };
        if previous != invitation {
            self.hosting_setup.access_opened = false;
            if invitation
                .as_ref()
                .is_some_and(|i| crate::invitation::private_address(&i.server))
            {
                let state = if kind == FlowKind::AddCommunity {
                    &mut self.add_community.setup
                } else {
                    &mut self.login.setup
                };
                state.private_connection_started = true;
                self.begin_friend_connection(ui.ctx());
            }
        }
        if let Some(invitation) =
            invitation.filter(|i| crate::invitation::private_address(&i.server))
        {
            self.friend_network_step(ui, &invitation);
        }
    }

    pub(super) fn receive_invite(
        &mut self,
        ctx: &egui::Context,
        code: &str,
        request_id: Option<u64>,
        proof: Option<crate::invite_proof::InviteProof>,
    ) {
        if let Some(request_id) = request_id {
            self.hosting_setup.hosted_address = self
                .bootstrap
                .as_ref()
                .and_then(|b| crate::local_host::shared_address(&b.server_id));
            if let Some(origin) = self.command_origin() {
                match self
                    .hosting_setup
                    .finish_invitation_copy_with_proof(ctx, request_id, &origin, code, proof)
                {
                    Ok(true) => {
                        self.notice = Some((
                            "Invitation copied — send it to your friend".into(),
                            Instant::now(),
                        ))
                    }
                    Ok(false) => {}
                    Err(error) => self.error = Some(error),
                }
            } else {
                self.hosting_setup.cancel_invitation_copy();
            }
            return;
        }
        self.open_invite_with(code, proof);
    }

    pub(super) fn open_invite(&mut self, code: &str) {
        self.open_invite_with(code, None);
    }

    fn open_invite_with(&mut self, code: &str, proof: Option<crate::invite_proof::InviteProof>) {
        let Some(bootstrap) = &self.bootstrap else {
            return;
        };
        let address = if crate::local_host::is_local_url(&self.login.url) {
            self.hosting_setup.hosted_address =
                crate::local_host::shared_address(&bootstrap.server_id);
            self.hosting_setup
                .hosted_address
                .as_deref()
                .unwrap_or(&self.login.url)
        } else {
            &self.login.url
        };
        match Invitation::new(address, &bootstrap.server_id, code) {
            Ok(mut invitation) => {
                if let Some(proof) = proof {
                    invitation = invitation.with_proof(proof);
                }
                if self.hosting_setup.open {
                    if !crate::invitation::private_address(&invitation.server) {
                        self.hosting_setup.hosted_address = None;
                        self.error = Some("Sharing is not ready yet. Enable it before copying an invitation for friends.".into());
                        return;
                    }
                    if !self.hosting_setup.existing_access {
                        match invitation.with_network_share(&self.hosting_setup.share_url) {
                            Ok(shared) => invitation = shared,
                            Err(error) => {
                                self.error = Some(error);
                                return;
                            }
                        }
                    }
                    self.hosting_setup.invitation = Some(invitation);
                    return;
                }
                self.invite_address = invitation.server.clone();
                self.invite_dialog = Some(invitation);
            }
            Err(error) => self.error = Some(error),
        }
    }

    pub(super) fn invite_dialog(&mut self, ctx: &egui::Context) {
        let Some(invitation) = self.invite_dialog.clone() else {
            return;
        };
        if crate::local_host::is_local_url(&self.login.url)
            && crate::local_host::is_local_url(&self.invite_address)
        {
            self.invite_dialog = None;
            self.open_friend_sharing();
            return;
        }
        let mut open = true;
        let window = desktop_ui::dialog(ctx, "Invite someone")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(400.0);
        // The shared header (title, Close, Escape) stays above the scrolling body.
        if host_setup_ui::show_with_header(ctx, window, "Invite someone", |ui| {
            ui.label("Copy the invitation and send it to your friend in any chat. They paste it into Relay.");
            ui.add_space(12.0);
            let offer = self.prefs.connection.offer.clone();
            let final_invitation = Invitation::new(
                &self.invite_address,
                &invitation.server_id,
                &invitation.code,
            )
            .map(|invite| match &invitation.proof {
                Some(proof) => invite.with_proof(proof.clone()),
                None => invite,
            })
            // Every route this PC offers travels with it, as in the host's own dialog.
            .and_then(|invite| invite.with_alternatives(&offer))
            .and_then(|invite| {
                match crate::iroh_route::hosted(&invitation.server_id) {
                    Some(route) => Ok(invite.with_iroh(route.to_invitation()?)),
                    None => Ok(invite),
                }
            });
            let local_only = final_invitation
                .as_ref()
                .is_ok_and(|invite| invite.same_network_only());
            if local_only {
                host_setup_ui::local_invitation_notice(ui);
            }
            let link = final_invitation.map(|invite| invite.browser_link());
            if ui
                .add_enabled(
                    link.is_ok(),
                    primary(if local_only {
                        "Copy local invitation"
                    } else {
                        "Copy invitation"
                    })
                    .min_size(Vec2::new(ui.available_width(), 40.0)),
                )
                .clicked()
            {
                if let Ok(link) = &link {
                    ui.output_mut(|output| output.copied_text = link.clone());
                    self.notice = Some(("Invitation copied".into(), Instant::now()));
                }
            }
            if let Err(error) = link {
                ui.label(RichText::new(error).size(font_size(12.0)).color(c_bad()));
            }
            ui.add_space(6.0);
            ui.label(
                RichText::new("Each person keeps their own encryption keys.")
                    .size(font_size(TEXT_CAPTION))
                    .color(c_muted()),
            );
            if crate::local_host::is_local_url(&self.login.url)
                && ui
                    .add(secondary("Invite friends from outside your home"))
                    .clicked()
            {
                self.open_friend_sharing();
                self.invite_dialog = None;
            }
            ui.add_space(8.0);
            ui.collapsing("Advanced", |ui| {
                field(
                    ui,
                    "ADDRESS YOUR FRIENDS CAN REACH",
                    &mut self.invite_address,
                    false,
                );
                ui.label(
                    RichText::new(crate::invitation::reachability_hint(&self.invite_address))
                        .size(font_size(12.0))
                        .color(c_muted()),
                );
            });
        }) {
            open = false;
        }
        if !open {
            self.invite_dialog = None;
        }
    }
}
