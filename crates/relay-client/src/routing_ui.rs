//! Joining with an invitation which offers more than one route.
//!
//! The friend never has to choose. Relay measures the routes in the background,
//! shows one calm line while it does, and goes on with the fastest one that
//! answers as the right community. Only a route that would send the password in
//! plain text over the internet stops for the friend's own decision.
use super::*;

/// A calm box for the join flow's state: what is happening, or why it stopped.
pub(super) fn status_card(ui: &mut egui::Ui, body: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::none()
        .fill(c_rail())
        .rounding(8.0)
        .inner_margin(Margin::same(14.0))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            body(ui);
        });
}

use crate::{
    connection_setup::FlowKind,
    invitation::Invitation,
    route_choice::{self, Capabilities, Outcome, Plan},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RefusedAction {
    Retry,
    AnotherInvitation,
    AllowOnce,
}

fn refused_actions(ui: &mut egui::Ui, overridable: bool, enabled: bool) -> Option<RefusedAction> {
    let mut action = None;
    ui.horizontal_wrapped(|ui| {
        if overridable {
            // Keep the safe choice prominent when a route needs consent.
            if ui
                .add_enabled(enabled, primary("Use another invitation"))
                .clicked()
            {
                action = Some(RefusedAction::AnotherInvitation);
            }
            if ui
                .add_enabled(enabled, secondary("Use it anyway"))
                .clicked()
            {
                action = Some(RefusedAction::AllowOnce);
            }
        } else {
            if ui.add_enabled(enabled, primary("Try again")).clicked() {
                action = Some(RefusedAction::Retry);
            }
            if ui
                .add_enabled(enabled, secondary("Use another invitation"))
                .clicked()
            {
                action = Some(RefusedAction::AnotherInvitation);
            }
        }
    });
    action
}

impl RelayApp {
    fn join_flow_mut(&mut self, kind: FlowKind) -> &mut crate::connection_setup::JoinFlow {
        match kind {
            FlowKind::AddCommunity => &mut self.add_community,
            FlowKind::Login => &mut self.login,
        }
    }

    /// A pasted invitation arrives here. A single route is used at once; several
    /// are measured on a background thread.
    pub(super) fn start_joining(
        &mut self,
        ctx: &egui::Context,
        kind: FlowKind,
        invitation: Invitation,
    ) {
        let prefs = self.prefs.connection.clone();
        let caps = Capabilities::current();
        {
            let state = &mut self.join_flow_mut(kind).setup;
            state.edited(&mut String::new());
            state.invitation_text.clear();
            state.clipboard.settle();
        }
        self.error = None;
        let all = route_choice::candidates(&invitation);
        if matches!(route_choice::plan(&all, prefs.mode, caps), Plan::Probe(_)) {
            let wake = ctx.clone();
            self.join_flow_mut(kind).setup.routing = Some(route_choice::Job::start(
                invitation,
                prefs,
                caps,
                move || wake.request_repaint(),
            ));
            return;
        }
        // One route needs no measuring, so deciding does not touch the network.
        let prober = route_choice::HealthProber::for_invitation(&invitation);
        let outcome = route_choice::decide(&invitation, &prefs, caps, &prober);
        self.finish_joining(ctx, kind, invitation, outcome);
    }

    fn finish_joining(
        &mut self,
        ctx: &egui::Context,
        kind: FlowKind,
        original: Invitation,
        outcome: Outcome,
    ) {
        match outcome.invitation {
            Ok(invitation) => {
                self.adopt_invitation(ctx, kind, invitation, Some(outcome.summary));
            }
            Err(message) => {
                self.join_flow_mut(kind).setup.refused = Some(route_choice::Refused {
                    invitation: original,
                    message,
                    overridable: outcome.needs_consent,
                });
            }
        }
    }

    /// The chosen invitation becomes the form's address, exactly as a pasted one
    /// always did: the community is pinned to the invitation and the private
    /// network starts only when the chosen route needs it.
    fn adopt_invitation(
        &mut self,
        ctx: &egui::Context,
        kind: FlowKind,
        invitation: Invitation,
        note: Option<String>,
    ) {
        let private = crate::invitation::private_address(&invitation.server);
        // The account-free route can only be opened with what the invitation told us.
        if crate::iroh_route::is_iroh(&invitation.server) {
            if let Some(route) = &invitation.iroh {
                crate::iroh_route::register(&crate::iroh_route::SavedIroh::from_invitation(route));
            }
        }
        let flow = self.join_flow_mut(kind);
        flow.url = invitation.server.clone();
        flow.invite = invitation.code.clone();
        flow.password.clear();
        flow.registering = true;
        flow.setup.invitation = Some(invitation);
        flow.setup.private_connection_started = private;
        flow.setup.route_note = note.filter(|n| !n.is_empty());
        self.error = None;
        self.hosting_setup.access_opened = false;
        if private {
            self.begin_friend_connection(ctx);
        }
    }

    /// Display-only join states for offscreen captures (`--capture-panel`).
    /// Nothing is measured or sent: the job never answers, and the invitation
    /// uses a documentation address and a made-up code.
    pub(super) fn prepare_join_capture_panel(&mut self, panel: &str) {
        if self.capture_path.is_none() || !panel.starts_with("join-") {
            return;
        }
        let Ok(invitation) = Invitation::new(
            "http://203.0.113.9:4580",
            "6f1c2b9e-4d4f-4c4e-9a59-2f1d6c1d0a11",
            "OWNED-PREVIEW",
        ) else {
            return;
        };
        self.hosting_setup.entry = host_setup_ui::LoginEntry::Join;
        self.login.setup.clipboard.settle();
        match panel {
            "join-routing" => {
                self.login.setup.routing = Some(route_choice::Job::preview(invitation));
            }
            "join-refused" => {
                self.login.setup.refused = Some(route_choice::Refused {
                    invitation,
                    message: route_choice::PLAIN_PUBLIC_MESSAGE.into(),
                    overridable: true,
                });
            }
            _ => {}
        }
    }

    /// Shows the measuring or refused state. Returns true when the normal
    /// invitation entry should not be drawn.
    pub(super) fn routing_state(&mut self, ui: &mut egui::Ui, kind: FlowKind) -> bool {
        let ctx = ui.ctx().clone();
        if let Some(mut job) = self.join_flow_mut(kind).setup.routing.take() {
            match job.poll() {
                Some(outcome) => {
                    let original = job.original.clone();
                    self.finish_joining(&ctx, kind, original, outcome);
                }
                None => {
                    ui.add_space(8.0);
                    status_card(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(RichText::new("Finding the fastest connection…").strong());
                        });
                        if job.waited() > std::time::Duration::from_secs(2) {
                            ui.label(
                                RichText::new("Relay tries every way your friend offered.")
                                    .color(c_muted()),
                            );
                        }
                    });
                    ui.add_space(12.0);
                    let cancel = ui.add(secondary("Cancel")).clicked();
                    ctx.request_repaint_after(std::time::Duration::from_millis(100));
                    if !cancel {
                        self.join_flow_mut(kind).setup.routing = Some(job);
                    }
                    return true;
                }
            }
        }
        if let Some(refused) = self.join_flow_mut(kind).setup.refused.take() {
            ui.add_space(8.0);
            status_card(ui, |ui| {
                ui.label(
                    RichText::new(if refused.overridable {
                        "This connection is not encrypted"
                    } else {
                        "Relay could not connect"
                    })
                    .strong()
                    .color(c_bad()),
                );
                ui.add_space(4.0);
                ui.add(egui::Label::new(&refused.message).wrap(true));
                if refused.overridable {
                    ui.add_space(4.0);
                    ui.add(
                        egui::Label::new(
                            RichText::new(
                                "Use it only on a network you trust, or if your friend has no safer address.",
                            )
                            .color(c_muted()),
                        )
                        .wrap(true),
                    );
                }
            });
            ui.add_space(12.0);
            match refused_actions(ui, refused.overridable, !self.busy) {
                Some(RefusedAction::Retry) => {
                    // Reuse the original invitation and current safety settings.
                    // Only this click starts another bounded route check.
                    self.start_joining(&ctx, kind, refused.invitation);
                }
                Some(RefusedAction::AnotherInvitation) => {}
                Some(RefusedAction::AllowOnce) => {
                    // For this one invitation only; the setting itself is untouched.
                    let mut once = self.prefs.connection.clone();
                    once.allow_plain_public_login = true;
                    let prober = route_choice::HealthProber::for_invitation(&refused.invitation);
                    let outcome = route_choice::decide(
                        &refused.invitation,
                        &once,
                        Capabilities::current(),
                        &prober,
                    );
                    self.finish_joining(&ctx, kind, refused.invitation.clone(), outcome);
                }
                None => self.join_flow_mut(kind).setup.refused = Some(refused),
            }
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_rect(shape: &egui::Shape, label: &str) -> Option<Rect> {
        match shape {
            egui::Shape::Text(text) if text.galley.job.text == label => {
                Some(Rect::from_min_size(text.pos, text.galley.size()))
            }
            egui::Shape::Vec(shapes) => shapes.iter().find_map(|s| text_rect(s, label)),
            _ => None,
        }
    }

    fn frame(
        ctx: &egui::Context,
        width: f32,
        overridable: bool,
        enabled: bool,
        events: Vec<egui::Event>,
    ) -> (egui::FullOutput, Option<RefusedAction>) {
        let mut action = None;
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(width, 240.0))),
                focused: true,
                events,
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    action = refused_actions(ui, overridable, enabled);
                });
            },
        );
        (output, action)
    }

    #[test]
    fn retry_buttons_keep_connection_failures_separate_from_safety_consent() {
        for (width, font_scale) in [(600.0, 1.0), (300.0, 1.0), (300.0, 1.4)] {
            for overridable in [false, true] {
                let actions = if overridable {
                    [
                        ("Use another invitation", RefusedAction::AnotherInvitation),
                        ("Use it anyway", RefusedAction::AllowOnce),
                    ]
                } else {
                    [
                        ("Try again", RefusedAction::Retry),
                        ("Use another invitation", RefusedAction::AnotherInvitation),
                    ]
                };
                for enabled in [false, true] {
                    for (label, expected) in actions {
                        let ctx = egui::Context::default();
                        configure(&ctx);
                        crate::appearance::AppearanceSettings {
                            font_scale,
                            ..Default::default()
                        }
                        .apply(&ctx, None);
                        frame(&ctx, width, overridable, enabled, vec![]);
                        let (output, action) = frame(&ctx, width, overridable, enabled, vec![]);
                        assert_eq!(action, None, "redraw never retries");
                        let button = output
                            .shapes
                            .iter()
                            .find_map(|s| text_rect(&s.shape, label))
                            .expect("action is visible");
                        assert!(Rect::from_min_size(Pos2::ZERO, Vec2::new(width, 240.0))
                            .contains_rect(button));
                        let absent = if overridable {
                            "Try again"
                        } else {
                            "Use it anyway"
                        };
                        assert!(!output
                            .shapes
                            .iter()
                            .any(|s| text_rect(&s.shape, absent).is_some()));
                        let pos = button.center();
                        let press = |pressed| egui::Event::PointerButton {
                            pos,
                            button: egui::PointerButton::Primary,
                            pressed,
                            modifiers: egui::Modifiers::NONE,
                        };
                        let (_, clicked) = frame(
                            &ctx,
                            width,
                            overridable,
                            enabled,
                            vec![egui::Event::PointerMoved(pos), press(true), press(false)],
                        );
                        assert_eq!(
                            clicked,
                            enabled.then_some(expected),
                            "{label}, width{width}, font{font_scale}, enabled{enabled}"
                        );
                    }
                }
            }
        }
    }
}
