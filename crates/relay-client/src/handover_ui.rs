//! Explicit full-data consent and resumable standby transfer on the Hosts page.
use super::*;
use crate::handover_client::{Action, Stage, View};
use relay_protocol::handover::Phase;

fn next_move_step(
    status: Option<&relay_protocol::handover::Status>,
    stage: Option<Stage>,
    on_target: bool,
    can_finish: bool,
) -> (&'static str, &'static str) {
    if stage == Some(Stage::Active) {
        return ("5. The new PC is hosting", "Keep Relay open on the new PC. The old host must stay retired.");
    }
    let Some(status) = status else {
        return ("1. Prepare the new PC", "On the new PC, open this community's Hosts settings, accept the Data copy role, and prepare a setup code. Send that code privately to the administrator on the old host.");
    };
    let Some(preparation) = &status.preparation else {
        return ("1. Prepare the new PC", "Prepare a setup code on the new PC, then paste it below on the administrator's PC. The current server keeps running; preparing a code does not copy its database.");
    };
    match preparation.phase {
        Phase::Offered if on_target => ("2. On the new PC: accept the full server data", "Your account was selected. Continue on the new PC and review the separate full-data consent below. Accepting the Data copy role alone does not authorize receiving all accounts, messages and files."),
        Phase::Offered => ("2. On the new PC: accept the move", "Ask the selected member to open this same Hosts page and accept the full server data. Then refresh the move status here."),
        Phase::Accepted if !can_finish => ("Preparation saved — final switch unavailable", "The current host still runs normally. This build keeps the service pause and permanent handover disabled until the complete two-PC flow has been accepted. Keep this preparation; do not manually stop or replace either database."),
        Phase::Accepted => ("3. On the old host: pause and prepare the final copy", "An administrator must confirm the service pause below. Then the new PC downloads or resumes the final transfer. Chat and calls stay paused until the move completes or is cancelled before the grant."),
        Phase::Frozen if status.retired.is_some() => ("5. On the new PC: resume the granted host", "The old host has permanently stopped serving this community. On the new PC, refresh this page and choose Resume this PC as host. Relay validates its signed grant before activation; do not restart the old database."),
        Phase::Frozen if status.received.is_none() && on_target => ("4. On the new PC: receive the final copy", "On the new PC, choose Download or resume transfer below. Keep Relay open. Verification must complete before that PC can stand by for the final grant."),
        Phase::Frozen if status.received.is_none() => ("4. On the new PC: receive the final copy", "Ask the selected member to choose Download or resume transfer on this page. Refresh here after that PC has verified the complete copy."),
        Phase::Frozen if on_target => ("4. On the new PC: start standby and keep Relay open", "On the new PC, choose Start this PC and wait. The administrator then checks that PC's connection and generated media before permanently granting the move. This standby copy is not yet the active server."),
        Phase::Frozen => ("4. On the old host: check the new PC", "The new PC must choose Start this PC and wait. Then check its connection below. Finishing requires a fresh voice/video check and separate confirmation that the old host can never resume."),
    }
}

#[derive(Default)]
pub(super) struct State {
    owner: Option<CommandOrigin>,
    request: Option<u64>,
    view: Option<View>,
    error: Option<String>,
    target_url: String,
    invitation: String,
    consent: bool,
    pause_consent: bool,
    stop_consent: bool,
    /// The status is checked once when the panel is first shown.
    inspected: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn move_panel_discards_late_account_and_request_results() {
        let owner = CommandOrigin {
            url: "https://source.example".into(),
            server_id: "community".into(),
            user_id: "member".into(),
        };
        let mut state = State {
            owner: Some(owner.clone()),
            request: Some(7),
            ..State::default()
        };
        let mut view = crate::handover_client::Job::failure(
            owner,
            7,
            crate::handover_client::Fault::Connection,
        );
        view.origin.user_id = "other".into();
        assert!(!state.receive(view.clone()));
        view.origin.user_id = "member".into();
        view.request_id = 6;
        assert!(!state.receive(view.clone()));
        view.request_id = 7;
        assert!(state.receive(view.clone()));
        assert!(state.request.is_none());
        assert!(!state.receive(view));
    }
}

impl State {
    fn receive(&mut self, mut view: View) -> bool {
        if self.owner.as_ref() != Some(&view.origin) || self.request != Some(view.request_id) {
            return false;
        }
        if let Some(old) = &self.view {
            if old.request_id == view.request_id && old.sequence >= view.sequence {
                return false;
            }
            if view.status.is_none() {
                view.status = old.status.clone();
            }
            if view.card.is_none() {
                view.card = old.card.clone();
            }
        }
        let old_preparation = self
            .view
            .as_ref()
            .and_then(|v| v.status.as_ref())
            .and_then(|s| s.preparation.as_ref())
            .map(|p| p.id.clone());
        let new_preparation = view
            .status
            .as_ref()
            .and_then(|s| s.preparation.as_ref())
            .map(|p| p.id.clone());
        if old_preparation != new_preparation {
            self.consent = false;
            self.pause_consent = false;
            self.stop_consent = false;
        }
        self.error = view.fault.map(|f| f.message().to_owned());
        if view.finished {
            self.request = None;
        }
        self.view = Some(view);
        true
    }
}

impl RelayApp {
    pub(super) fn handover_tick(&mut self) {
        let owner = self.command_origin();
        if self.handover.owner != owner {
            if let Some(origin) = self.handover.owner.take() {
                let _ = self.network.send(Command::StopHandover {
                    origin,
                    account_change: true,
                });
            }
            self.handover = State {
                owner,
                ..State::default()
            };
        }
    }
    pub(super) fn handover_received(&mut self, view: View) {
        if self.command_origin().as_ref() == Some(&view.origin) {
            self.handover.receive(view);
        }
    }
    fn handover_request(&mut self, action: Action) {
        let Some(origin) = self.command_origin() else {
            return;
        };
        if self.handover.request.is_some() {
            return;
        }
        if !matches!(action, Action::Inspect | Action::Decline | Action::Cancel) {
            let _ = self.network.send(Command::KeepSignedInForMove {
                origin: origin.clone(),
            });
        }
        let request_id = self.next_request();
        match self.network.send(Command::Handover {
            origin: origin.clone(),
            action,
            request_id,
        }) {
            Ok(()) => {
                self.handover.owner = Some(origin);
                self.handover.request = Some(request_id);
                self.handover.error = None;
            }
            Err(error) => self.handover.error = Some(error),
        }
    }
    pub(super) fn handover_ui(&mut self, ui: &mut egui::Ui, bootstrap: &Bootstrap) {
        if !bootstrap
            .capabilities
            .iter()
            .any(|c| c == "handover-preparation-v1")
        {
            return;
        }
        self.handover_tick();
        let admin = bootstrap.has_permission(PERMISSION_ADMIN);
        let finish_enabled=self.hosting_view().is_some_and(|v|v.abilities.planned_handover);
        let busy = self.handover.request.is_some();
        let status = self.handover.view.as_ref().and_then(|v| v.status.clone());
        let mut action = None;
        egui::CollapsingHeader::new("Move this community").default_open(bootstrap.host_paused).show(ui,|ui| {
            ui.label("Prepare another PC to host this community. The current host stays in charge until a completed move.");
            let on_target = status.as_ref().and_then(|s| s.preparation.as_ref())
                .is_some_and(|p| p.target_user_id == bootstrap.user.id);
            let (step, instruction) = next_move_step(status.as_ref(), self.handover.view.as_ref().map(|v| v.stage), on_target, finish_enabled);
            ui.add_space(8.0);
            ui.label(RichText::new(step).strong());
            ui.add(egui::Label::new(instruction).wrap(true));
            ui.add_space(8.0);
            if !self.handover.inspected && !busy && self.handover.view.is_none() {
                self.handover.inspected = true;
                action = Some(Action::Inspect);
            }
            ui.horizontal(|ui| {
                if ui.add_enabled(!busy,secondary(if status.is_some(){"Refresh move status"}else{"Check move status"})).clicked(){action=Some(Action::Inspect);}
                if busy {
                    ui.spinner();
                    if ui.add(secondary("Stop on this PC")).clicked() {
                        if let Some(origin)=self.command_origin(){let _=self.network.send(Command::StopHandover{origin,account_change:false});}
                    }
                }
            });
            if let Some(error)=&self.handover.error {ui.colored_label(c_bad(),error);}
            if let Some(v)=&self.handover.view {
                if !v.finished {
                    ui.label(match v.stage {Stage::Waiting=>"Preparing the encrypted transfer…",Stage::Downloading=>"Receiving the encrypted transfer…",Stage::Verifying=>"Checking the complete server copy…",Stage::Ready=>"This PC is standing by. Waiting for the old host to grant the move…",_=>"Checking the host…"});
                }
                if v.finished&&v.stage==Stage::Checked {ui.label("The new host's connection check passed. Finishing the move runs a fresh check again.");}
                if v.finished&&v.stage==Stage::RecoveredGrant {ui.label("The old host is offline. Relay recovered and verified this PC's saved signed grant. The new host has not been started by this status check.");}
                if v.stage==Stage::Active {ui.label("This PC is hosting the transferred community. Relay will resume it when the app starts again.");}
                if v.total_parts>0 {
                    ui.add(egui::ProgressBar::new(v.parts as f32/v.total_parts as f32).text(format!("{} / {} parts · {:.1} MiB",v.parts,v.total_parts,v.bytes as f64/1048576.0)));
                }
                if let Some(card)=&v.card {
                    ui.label("Send this private setup code only to the community administrator. It contains the new connection invitation, but no login password or server data.");
                    if ui.add(secondary("Copy setup code")).clicked(){ui.output_mut(|output|output.copied_text=card.clone());}
                }
            }
            let Some(s)=&status else{return;};
            if let Some(p)=&s.preparation {
                if admin||p.target_user_id==bootstrap.user.id {
                    ui.label(RichText::new("Relay keeps you signed in to this community on this PC until you sign out, so you can continue the move after a restart.").color(c_muted()));
                }
                let name=bootstrap.members.iter().find(|u|u.id==p.target_user_id).map(|u|u.username.as_str()).unwrap_or("The selected member");
                ui.label(format!("New host: {name}"));
                let mine=p.target_user_id==bootstrap.user.id;
                let ready=!busy&&self.handover.error.is_none();
                match p.phase {
                    Phase::Offered=>{
                        ui.label("Waiting for the new host to accept.");
                        if mine {
                            ui.label("Hosting gives this PC the whole community database, including accounts, history and files. End-to-end encrypted content stays encrypted. This is broader access than keeping a member copy.");
                            ui.checkbox(&mut self.handover.consent,"I agree to receive and host the full community data on this PC.");
                            ui.horizontal(|ui|{
                                if ui.add_enabled(ready&&self.handover.consent,primary("Accept full server data")).clicked(){action=Some(Action::Accept{confirm_full_data:true});}
                                if ui.add_enabled(ready,secondary("Decline")).clicked(){action=Some(Action::Decline);}
                            });
                        }
                    },
                    Phase::Accepted=>{
                        ui.label("The new host has accepted. Chat and calls can continue on the current host.");
                        if admin&&finish_enabled {
                            ui.label("Pausing stops chat and calls while the final copy is transferred. The old host can resume until you permanently grant the move.");
                            ui.checkbox(&mut self.handover.pause_consent,"I agree to pause this community for the final transfer.");
                            if ui.add_enabled(ready&&self.handover.pause_consent,primary("Pause and prepare final transfer")).clicked(){action=Some(Action::Freeze{confirm_pause:true});}
                        } else if finish_enabled {
                            ui.label("Waiting for an administrator to pause this community for the final transfer.");
                        } else {ui.label("Finishing a move is not available in this build.");}
                    },
                    Phase::Frozen=>{
                        if s.retired.is_some(){ui.label("The old host has granted this move and cannot resume hosting.");}
                        else {ui.colored_label(c_bad(),"The old host is paused for this move. Chat and calls are stopped.");}
                        if s.received.is_some()&&s.retired.is_none(){
                            ui.label("The complete copy has been verified. The selected new host must keep Relay open and choose Start this PC and wait.");
                            if admin {
                                if ui.add_enabled(ready,secondary("Check new host connection")).clicked(){action=Some(Action::CheckDestination);}
                                if finish_enabled {
                                    ui.label("Permanently granting the move prevents this old host from serving the community again. Cancelling or restoring a backup cannot undo it.");
                                    ui.checkbox(&mut self.handover.stop_consent,"I understand: permanently stop the old host and transfer authority.");
                                    let checked=self.handover.view.as_ref().is_some_and(|v|v.finished&&v.stage==Stage::Checked);
                                    if ui.add_enabled(ready&&checked&&self.handover.stop_consent,primary("Finish move")).clicked(){action=Some(Action::Complete{confirm_stop_source:true});}
                                }else{ui.label("The final switch is not available in this build. You can check a prepared new host's connection.");}
                            }
                        }
                        if admin&&finish_enabled&&s.retired.is_some()&&ui.add_enabled(ready,secondary("Retry grant delivery")).clicked(){action=Some(Action::Complete{confirm_stop_source:true});}
                        if mine &&s.retired.is_none() && ui.add_enabled(ready,primary("Download or resume transfer")).clicked(){action=Some(Action::Download{retry_export:false});}
                        if mine&&s.received.is_some()&&self.handover.view.as_ref().is_none_or(|v|v.stage!=Stage::Active) {
                            ui.label(if crate::iroh_route::is_iroh(&p.target_url){"Uses this PC's own private invitation and voice/video service. Keep Relay open while the old host completes the move."}else{"Uses this PC's own voice/video service and its configured HTTPS address."});
                            if ui.add_enabled(ready,primary(if s.retired.is_some(){"Resume this PC as host"}else{"Start this PC and wait"})).clicked(){action=Some(Action::RunDestination);}
                        }
                    },
                }
                if (admin||mine)&&s.retired.is_none() &&ui.add_enabled(ready,secondary(if p.phase==Phase::Frozen{"Cancel move and resume old host"}else{"Cancel move"})).clicked(){action=Some(Action::Cancel);}
            }else if !busy &&self.handover.error.is_none() {
                ui.label("On the new PC, accept the Data copy role under Hosts first. Then prepare that PC and send its setup code to the administrator.");
                if bootstrap.capabilities.iter().any(|c|c=="handover-private-setup-v1") {
                    ui.label("Create this PC's own private invitation without starting a community or receiving its data.");
                    if ui.add(secondary("Prepare private setup code")).clicked(){action=Some(Action::SetupPrivate);}
                }
                ui.label("Or use an existing HTTPS address for the new host");
                ui.add(egui::TextEdit::singleline(&mut self.handover.target_url).hint_text("https://…").char_limit(2048));
                if ui.add_enabled(!self.handover.target_url.trim().is_empty(),secondary("Prepare this PC")).clicked(){action=Some(Action::Setup{target_url:self.handover.target_url.trim().to_owned()});}
                if admin {
                    ui.separator();
                    ui.label("Setup code from the new host");
                    ui.add(egui::TextEdit::multiline(&mut self.handover.invitation).desired_rows(2).char_limit(8192));
                    // A signed code binds the offer to the same source host and
                    // the chosen member. The server independently enforces roles.
                    let pin=bootstrap.host_authority.as_ref();
                    let card=pin.and_then(|pin|crate::handover_client::decode_card(&self.handover.invitation,pin).ok());
                    if let Some(card)=&card {
                        if let Some(user)=bootstrap.members.iter().find(|u|u.id==card.body.user_id){
                            ui.label(if crate::iroh_route::is_iroh(&card.body.target_url){
                                format!("Ask {} to host this community on their PC, over its own private connection.",user.username)
                            }else{
                                format!("Ask {} to host at {}",user.username,card.body.target_url)
                            });
                        }
                    }
                    if ui.add_enabled(card.is_some(),primary("Ask this member to host")).clicked(){action=Some(Action::Offer{card:self.handover.invitation.clone()});}
                }
            }
            if self.handover.view.as_ref().is_some_and(|v| matches!(v.fault,
                Some(crate::handover_client::Fault::Integrity | crate::handover_client::Fault::Changed))) {
                ui.add(egui::Label::new("A damaged or changed export needs a new preparation. Refresh the status, then ask the administrator to cancel this ungranted move and prepare it again. Existing received data and keys are kept; do not replace that database manually.").wrap(true));
            }
        });
        ui.add_space(8.0);
        if let Some(action) = action {
            self.handover_request(action);
        }
    }
}
