//! Shared reducer for cold-start and forwarded invitations. Never joins a server.
use super::*;
use crate::invitation_launch::{Form, Offer};

impl RelayApp {
    #[cfg(windows)]
    pub fn invitation_link_status(&mut self, result: crate::url_protocol::Outcome) {
        let hint=match result {
            crate::url_protocol::Outcome::Ready=>return,
            crate::url_protocol::Outcome::Preserved=>"Invitation links already belong to another installation or app. You can paste your invitation in Join friends.",
            _=>"Invitation links could not be enabled. You can paste your invitation in Join friends.",
        };
        self.notice = Some((hint.into(), Instant::now()));
    }
    fn invitation_form(&self) -> Form<'_> {
        if self.invitation_overlay_open() {
            return Form::Other;
        }
        let setup = if self.session.is_some() {
            if !self.add_community_open {
                return Form::Empty;
            }
            if self.add_community.setup.invitation.is_none()
                && (!self.add_community.url.is_empty()
                    || !self.add_community.password.is_empty()
                    || !self.add_community.invite.is_empty()
                    || self.add_community.username != self.login.username)
            {
                return Form::Other;
            }
            &self.add_community.setup
        } else {
            match self.hosting_setup.entry {
                host_setup_ui::LoginEntry::Welcome => return Form::Empty,
                host_setup_ui::LoginEntry::Join => &self.login.setup,
                _ => return Form::Other,
            }
        };
        if let Some(invitation) = &setup.invitation {
            Form::Invitation(invitation)
        } else if setup.invitation_text.trim().is_empty() {
            Form::Empty
        } else {
            Form::Text(&setup.invitation_text)
        }
    }

    pub(super) fn invitations_tick(&mut self, ctx: &egui::Context) {
        let mut pending = std::mem::take(&mut self.external_invitation);
        let updating = self.update_ui.preparing_install();
        #[cfg(windows)]
        let mut activate = false;
        #[cfg(windows)]
        if let Some(inbox) = &self.invitation_inbox {
            // One bounded channel; only normal frames wake the UI. A timed-out
            // caller cannot later replace a form after its exchange expired.
            if let Some(incoming) = inbox.next() {
                if !incoming.current() {
                    incoming.respond(Offer::Busy);
                } else {
                    let offer = match &incoming.action {
                        crate::single_instance::Action::Activate => {
                            if updating {
                                Offer::Busy
                            } else {
                                Offer::Accepted
                            }
                        }
                        crate::single_instance::Action::Open(invite) => {
                            pending.offer(invite.clone(), updating, self.invitation_form())
                        }
                    };
                    activate |= !updating;
                    if offer == Offer::Busy && !updating {
                        let hint = "Finish or close the window already open, then open the invitation again.";
                        if self.session.is_none() {
                            self.error = Some(hint.into());
                        }
                        self.notice = Some((hint.into(), Instant::now()));
                    }
                    incoming.respond(offer);
                }
            }
        }
        // Keep the capability in the existing volatile slot while account
        // sign-in or automatic server reopening can still reset setup forms.
        // Afterward it opens the ordinary Join screen for the active session.
        let account_gate = self.central_account.provider.is_some()
            && self.central_account.account.is_none()
            && !self.account_start.bypass;
        let changing_account = account_gate
            || self.central_account.waiting()
            || self.account_start.pending.is_some()
            || self.account_start.auto.is_some();
        let invitation = pending.take_ready(
            self.busy || changing_account,
            updating,
            self.invitation_form(),
        );
        self.external_invitation = pending;
        if let Some(invitation) = invitation {
            if self.session.is_some() {
                // This leaves the current community/call untouched.
                self.open_community_dialog();
                crate::invitation_launch::prefill(&mut self.add_community.setup, invitation);
            } else {
                crate::invitation_launch::prefill(&mut self.login.setup, invitation);
                self.hosting_setup.entry = host_setup_ui::LoginEntry::Join;
                self.hosting_setup.new_host = false;
                self.hosting_setup.resume_share = false;
            }
            self.error = None;
        }
        #[cfg(windows)]
        if activate {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(
                egui::UserAttentionType::Informational,
            ));
        }
        #[cfg(not(windows))]
        let _ = ctx;
    }
}
