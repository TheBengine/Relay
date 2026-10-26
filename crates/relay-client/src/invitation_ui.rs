use super::*;
use crate::appearance::{TEXT_BODY, TEXT_CAPTION};
use crate::connection_setup::FlowKind;
use crate::{connection_setup::SetupState, invitation::Invitation};

/// The "Find on local network" action and its results. Choosing a result only
/// hands it back; the caller fills the ordinary address field.
fn lan_find_controls(
    ui: &mut egui::Ui,
    finder: &mut crate::lan_discovery::Finder,
    enabled: bool,
    relay_account: bool,
    saved: &[crate::community::SavedCommunity],
) -> Option<crate::lan_discovery::Choice> {
    finder.poll();
    ui.add_space(10.0);
    ui.horizontal_wrapped(|ui| {
        if finder.searching() {
            ui.add_enabled(false, secondary("Searching…"));
            if ui.add(secondary("Cancel")).clicked() {
                finder.cancel();
            }
        } else if ui
            .add_enabled(enabled, secondary("Find on local network"))
            .clicked()
        {
            finder.start();
        }
    });
    ui.add(
        egui::Label::new(
            RichText::new("Looks for Relay servers on this Wi-Fi or wired network whose hosts turned on local discovery. It does not search the internet. Choosing one only fills in the address above; you still check the connection, and nothing is joined or trusted because it was found.")
                .size(font_size(TEXT_CAPTION))
                .color(c_muted()),
        )
        .wrap(true),
    );
    if finder.searching() {
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(100));
    }
    let mut picked = None;
    for (index, found) in finder.found.iter().enumerate() {
        let joinable = crate::lan_discovery::joinable(relay_account, found);
        let saved_route =
            crate::lan_discovery::secure_route(saved, found, crate::lan_discovery::helper_available());
        ui.push_id(("lan-found", index), |ui| {
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(&found.name).strong());
                ui.label(
                    RichText::new(found.address())
                        .size(font_size(TEXT_CAPTION))
                        .color(c_muted()),
                );
                if ui
                    .add_enabled(enabled && joinable, secondary("Use this server"))
                    .clicked()
                {
                    picked = Some(crate::lan_discovery::Choice::Network(found.clone()));
                }
                if saved_route.is_none() && ui.add_enabled(enabled,secondary("Use host's local code")).clicked(){
                    picked=Some(crate::lan_discovery::Choice::Pair(found.clone()));
                }
                if saved_route.is_some()
                    && ui
                        .add_enabled(enabled, secondary("Use my saved secure address"))
                        .clicked()
                {
                    picked = Some(crate::lan_discovery::Choice::Saved(found.clone()));
                }
            });
            if let Some(route) = &saved_route {
                let stored = route.display_name();
                ui.add(
                    egui::Label::new(
                        RichText::new(format!("You already saved a secure address for \u{201c}{stored}\u{201d} with this same server id. Using it checks the connection and the server's identity as usual."))
                            .size(font_size(TEXT_CAPTION))
                            .color(c_muted()),
                    )
                    .wrap(true),
                );
            } else if !joinable {
                ui.add(
                    egui::Label::new(
                        RichText::new("This server answers on plain HTTP. Use the host's local code or a secure invitation to connect. Your Relay account still signs in only over HTTPS or Relay's private route.")
                            .size(font_size(TEXT_CAPTION))
                            .color(c_muted()),
                    )
                    .wrap(true),
                );
            }
        });
    }
    if finder.failed {
        ui.label(
            RichText::new("Relay could not use the network to search.")
                .size(font_size(TEXT_CAPTION))
                .color(c_bad()),
        );
    } else if finder.finished && finder.found.is_empty() {
        ui.label(
            RichText::new("No servers answered. A server must be on this network, and its host must have turned on local discovery.")
                .size(font_size(TEXT_CAPTION))
                .color(c_muted()),
        );
    }
    picked
}

enum SetupAction {
    Check(String),
    Invitation(Invitation),
}

#[cfg(test)]
mod setup_action_tests {
    use super::*;

    #[test]
    fn advanced_invitation_click_keeps_full_capability_for_the_shared_join_flow() {
        let ctx = egui::Context::default();
        configure(&ctx);
        ctx.enable_accesskit();
        let endpoint = "b".repeat(64);
        let invitation = Invitation::new(
            &crate::route_choice::iroh_address(&endpoint),
            "4be44aef-8c98-4db6-a9d5-81c2eb311330",
            "synthetic-invitation",
        ).unwrap().with_iroh(crate::invitation_routes::IrohRoute::new(
            &endpoint, None, "abcdefghijklmnopqrstuvwxyzABCDEF0123456789_-",
        ).unwrap());
        let mut address = invitation.browser_link();
        let mut code = String::new();
        let mut state = SetupState::default();
        let mut action = None;
        let mut frame = |events| ctx.run(egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(600.0, 800.0))),
            focused: true,
            events,
            ..Default::default()
        }, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                action = setup_controls(ui, &mut address, &mut code, &mut state, true);
            });
        });
        let first = frame(Vec::new());
        let button = first.platform_output.accesskit_update.unwrap().nodes.into_iter()
            .find(|(_, node)| node.name() == Some("Use invitation")).unwrap().0;
        frame(vec![egui::Event::AccessKitActionRequest(egui::accesskit::ActionRequest {
            target: button,
            action: egui::accesskit::Action::Default,
            data: None,
        })]);
        let Some(SetupAction::Invitation(chosen)) = action else {
            panic!("The advanced control must request the ordinary join flow");
        };
        assert!(chosen == invitation);
        assert_eq!(address, invitation.browser_link());
        assert!(code.is_empty() && state.invitation.is_none() && state.pending.is_none());
    }
}

fn setup_controls(
    ui: &mut egui::Ui,
    address: &mut String,
    invite_code: &mut String,
    state: &mut SetupState,
    enabled: bool,
) -> Option<SetupAction> {
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
                Ok(invitation) => return Some(SetupAction::Invitation(invitation)),
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
            return crate::community::normalize_url(address).ok().map(SetupAction::Check);
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
        // Pasting into the advanced address form must use the same bounded
        // route selection, consent and iroh admission as the normal Join form.
        if self.routing_state(ui, kind) {
            return;
        }
        let previous = if kind == FlowKind::AddCommunity {
            self.add_community.setup.invitation.clone()
        } else {
            self.login.setup.invitation.clone()
        };
        self.lan_pairing_join_controls(ui,kind);
        let target = if kind == FlowKind::AddCommunity {
            setup_controls(
                ui,
                &mut self.add_community.url,
                &mut self.add_community.invite,
                &mut self.add_community.setup,
                !self.busy,
            )
        } else {
            setup_controls(
                ui,
                &mut self.login.url,
                &mut self.login.invite,
                &mut self.login.setup,
                !self.busy,
            )
        };
        if let Some(SetupAction::Invitation(invitation)) = target {
            self.start_joining(ui.ctx(), kind, invitation);
            ui.ctx().request_repaint();
            return;
        }
        let relay_account = self.central_account.account.is_some();
        let found = {
            let state = if kind == FlowKind::AddCommunity {
                &mut self.add_community.setup
            } else {
                &mut self.login.setup
            };
            lan_find_controls(
                ui,
                &mut state.lan,
                !self.busy,
                relay_account,
                &self.prefs.communities,
            )
        };
        if let Some(choice) = found {
            let flow = if kind == FlowKind::AddCommunity {
                &mut self.add_community
            } else {
                &mut self.login
            };
            // A saved identity for this address is never replaced by an answer.
            let selected = match &choice {
                crate::lan_discovery::Choice::Network(found) => {
                    flow.select_discovered(&self.prefs.communities, found)
                }
                crate::lan_discovery::Choice::Pair(found) => {
                    let selected=flow.select_discovered(&self.prefs.communities,found);
                    if selected.is_ok(){flow.password.clear();flow.second_factor.clear();flow.factor_binding=None;flow.setup.lan_pairing.found=Some(found.clone());}
                    selected
                }
                crate::lan_discovery::Choice::Saved(found) => {
                    flow.select_saved_route(
                        &self.prefs.communities,
                        found,
                        crate::lan_discovery::helper_available(),
                    )
                }
            };
            match selected {
                Ok(()) => ui.ctx().request_repaint(),
                Err(message) => self.error = Some(message),
            }
        }
        if let Some(SetupAction::Check(url)) = target {
            let request_id = self.next_request();
            let state = if kind == FlowKind::AddCommunity {
                &mut self.add_community.setup
            } else {
                &mut self.login.setup
            };
            match state.begin_check(&url, request_id, true) {
                Ok(Some(url)) => {
                    if let Err(error) = self.network.send(Command::CheckServer {
                        cancel: state.check_cancellation(),
                        authority: state.invitation.as_ref().and_then(|i|i.authority.clone()),
                        locator_invite: state.invitation.as_ref().filter(|i|i.authority.is_some()).map(|i|i.code.clone()),
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
                let authority = match self.bootstrap.as_ref().map(|b|
                    crate::host_route::bootstrap_pin(&origin.url,&b.server_id,b.host_authority.as_ref(),None)).transpose() {
                    Ok(pin)=>pin.flatten(), Err(error)=>{self.error=Some(error);return;}
                };
                match self
                    .hosting_setup
                    .finish_invitation_copy_with_proof(ctx, request_id, &origin, code, proof, authority)
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
        let invitation=Invitation::new(address, &bootstrap.server_id, code).and_then(|invite|
            invite.with_authority(crate::host_route::bootstrap_pin(&self.login.url,&bootstrap.server_id,bootstrap.host_authority.as_ref(),None)?));
        match invitation {
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
            ui.label("Send this link to your friend in any chat. They click it, then choose Open Relay in their browser. They can also paste it into Join a server in the app.");
            ui.add_space(12.0);
            let offer = self.prefs.connection.offer.clone();
            let final_invitation = Invitation::new(
                &self.invite_address,
                &invitation.server_id,
                &invitation.code,
            )
            .and_then(|invite| invite.with_authority(invitation.authority.clone()))
            .map(|invite| match &invitation.proof {
                Some(proof) => invite.with_proof(proof.clone()),
                None => invite,
            })
            // Every route this PC offers travels with it, as in the host's own dialog.
            .and_then(|invite| invite.with_alternatives(&offer))
            .and_then(|invite| {
                match crate::iroh_route::hosted_status(&invitation.server_id) {
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
            let link = final_invitation.map(|invite| invite.for_sharing().browser_link());
            // Keep feedback in the dialog and bound to the exact invitation;
            // editing the address or opening another invite must not say Copied.
            let copied_id = egui::Id::new(("invitation-copied", link.as_ref().ok()));
            let copied = ctx.data(|data| data.get_temp::<Instant>(copied_id))
                .is_some_and(|at| at.elapsed() < Duration::from_secs(5));
            if copied {
                ctx.request_repaint_after(Duration::from_secs(5));
            }
            if ui
                .add_enabled(
                    link.is_ok(),
                    primary(if copied {
                        "Copied — ready to send"
                    } else if local_only {
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
                    ctx.data_mut(|data| data.insert_temp(copied_id, Instant::now()));
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
