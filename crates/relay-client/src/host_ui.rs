//! The Hosts page: who may help run this community, what each member agreed to,
//! and an honest answer to "is this machine ready for that job?".
//!
//! Everything shown comes from the server's `/api/hosting` view. A dimension no
//! machine has measured is shown as "Not measured", never as a guess. Colour is
//! always paired with a shape and a word.
use super::*;
use crate::appearance::{TEXT_BODY, TEXT_CAPTION};
use crate::settings_ui as controls;
use relay_protocol::hosting::{
    self, Assessment, CandidateState, CandidateView, Dimension, HostRole, HostingView, Verdict,
};
use serde_json::{json, Value};

#[cfg(all(test, windows))]
#[path = "hosts_native_fixture.rs"]
mod native_fixture;

#[path = "replica_copy_ui.rs"]
pub(in crate::app) mod copy_ui;

const ACTION: &str = "action:hosting";

/// A failed request. A failed load makes what is on screen possibly stale; a
/// failed change leaves it as it was.
struct Failure {
    loading: bool,
    message: String,
}

#[derive(Default)]
pub(super) struct State {
    offer_member: String,
    offer_roles: [bool; 2],
    accept_roles: [bool; 2],
    accept_revision: Option<u64>,
    withdraw_confirm: Option<String>,
    /// The last answer that passed validation, with the community and account
    /// it belongs to. Nothing else is ever shown as host data.
    view: Option<(CommandOrigin, HostingView)>,
    loaded: Option<Instant>,
    failure: Option<Failure>,
    /// Counted for the native QA fixture only: how many requests were attempted.
    #[cfg(test)]
    dispatched: u32,
}

/// Where requests go. The app uses its network worker; tests use an owned
/// channel, so a refused send can be reproduced without a window.
pub(super) trait Sink {
    fn send(&self, command: Command) -> Result<(), String>;
}

impl Sink for Network {
    fn send(&self, command: Command) -> Result<(), String> {
        Network::send(self, command)
    }
}

struct Request<'a> {
    method: AdminMethod,
    path: String,
    body: Value,
    key: &'a str,
}

/// An answer is shown only if it is a valid host view for this community and
/// account. Anything else is a failure the member can act on.
fn validate(
    data: &Value,
    origin: &CommandOrigin,
    instance_id: &str,
) -> Result<HostingView, String> {
    if let Some(schema) = data.get("schema").and_then(Value::as_u64) {
        if schema != u64::from(hosting::SCHEMA) {
            return Err("This server uses a different version of the host settings than this Relay understands. Update Relay and try again.".into());
        }
    }
    let view: HostingView = serde_json::from_value(data.clone())
        .map_err(|_| "The server's answer could not be read.".to_string())?;
    if view.schema != hosting::SCHEMA {
        return Err("This server uses a different version of the host settings than this Relay understands. Update Relay and try again.".into());
    }
    if view.you != origin.user_id {
        return Err("The answer was meant for a different account.".into());
    }
    if view.active_host.instance_id != instance_id {
        return Err("The answer came from a different server session than this connection. Refresh and try again.".into());
    }
    Ok(view)
}

/// Load automatically only once: after any failure the page waits for Retry.
fn should_auto_request(has_view: bool, failed: bool, pending: bool) -> bool {
    !has_view && !failed && !pending
}

impl State {
    fn fail(&mut self, key: &str, message: String) {
        self.failure = Some(Failure {
            loading: key == "hosting",
            message,
        });
    }

    /// The view, only while it belongs to this community and account.
    fn view_for(&self, origin: Option<&CommandOrigin>) -> Option<&HostingView> {
        self.view
            .as_ref()
            .filter(|(owner, _)| Some(owner) == origin)
            .map(|(_, view)| view)
    }

    /// Send one request. A refusal to send is a failure of that request: it is
    /// shown on this page and keeps a stale view stale. It is not retried.
    fn dispatch(
        &mut self,
        sink: &impl Sink,
        origin: Option<CommandOrigin>,
        request_id: u64,
        requests: &mut HashMap<u64, String>,
        request: Request<'_>,
    ) {
        #[cfg(test)]
        {
            self.dispatched += 1;
        }
        let sent = match origin {
            None => Err("Connect to a community before changing its settings.".to_string()),
            Some(origin) => sink.send(Command::Admin {
                origin,
                method: request.method,
                path: request.path,
                body: request.body,
                request_id,
            }),
        };
        match sent {
            Ok(()) => {
                requests.insert(request_id, request.key.to_string());
            }
            Err(message) => self.fail(request.key, message),
        }
    }

    /// Take an answer. Only a valid one replaces the last good view, marks it
    /// loaded and clears an earlier failure. Returns whether it was accepted.
    fn receive(
        &mut self,
        key: &str,
        data: &Value,
        origin: Option<CommandOrigin>,
        instance_id: &str,
        now: Instant,
    ) -> bool {
        let checked = match origin {
            Some(origin) => validate(data, &origin, instance_id).map(|view| (origin, view)),
            None => Err("Connect to a community first.".to_string()),
        };
        match checked {
            Ok(view) => {
                self.view = Some(view);
                self.loaded = Some(now);
                self.failure = None;
                true
            }
            Err(message) => {
                // The change may have happened even though its answer is
                // unusable, so what is on screen is no longer trustworthy.
                let message = if key == ACTION {
                    format!("{message} Your change may or may not have been saved, so refresh before changing anything else.")
                } else {
                    message
                };
                self.failure = Some(Failure {
                    loading: true,
                    message,
                });
                false
            }
        }
    }
}

/// A failed refresh leaves the screen possibly out of date; a failed change
/// does not.
fn is_stale(failure: Option<&Failure>) -> bool {
    failure.is_some_and(|failure| failure.loading)
}

fn failure_message(failure: &Failure, age_secs: Option<u64>) -> String {
    if failure.loading {
        let age = age_secs
            .map(|secs| format!(" from {secs} s ago"))
            .unwrap_or_default();
        format!(
            "Could not refresh: {} Showing the last copy{age}. It may be out of date, so changes are paused.",
            failure.message
        )
    } else {
        format!("Your change was not saved: {}", failure.message)
    }
}

fn role_index(role: HostRole) -> usize {
    match role {
        HostRole::Replica => 0,
        HostRole::VoiceGateway => 1,
    }
}

fn chosen(flags: &[bool; 2]) -> Vec<HostRole> {
    HostRole::ALL
        .into_iter()
        .filter(|role| flags[role_index(*role)])
        .collect()
}

pub(super) fn state_word(state: CandidateState) -> &'static str {
    match state {
        CandidateState::Pending => "Waiting for an answer",
        CandidateState::Approved => "Approved",
        CandidateState::Declined => "Declined",
        CandidateState::Revoked => "Withdrawn",
    }
}

/// One line per offered role, saying who must act next.
pub(super) fn role_status(view: &CandidateView, role: HostRole) -> String {
    let candidate = &view.candidate;
    if candidate.holds(role) {
        format!("{}: approved", role.label())
    } else if candidate.waiting_for_acceptance().contains(&role) {
        format!("{}: waiting for {} to accept", role.label(), view.name)
    } else {
        format!("{}: not active", role.label())
    }
}

fn verdict_color(ui: &egui::Ui, verdict: Verdict) -> Color32 {
    match verdict {
        Verdict::Green => c_good(),
        Verdict::Yellow if ui.visuals().dark_mode => Color32::from_rgb(224, 170, 60),
        Verdict::Yellow => Color32::from_rgb(150, 96, 0),
        Verdict::Red => c_bad(),
        Verdict::Unknown => c_muted(),
    }
}

/// A shape per verdict: disc, triangle, square, empty ring.
fn verdict_badge(ui: &mut egui::Ui, verdict: Verdict) {
    let size = font_size(12.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    let color = verdict_color(ui, verdict);
    let painter = ui.painter();
    let center = rect.center();
    let r = size * 0.45;
    match verdict {
        Verdict::Green => {
            painter.circle_filled(center, r, color);
        }
        Verdict::Yellow => {
            painter.add(egui::Shape::convex_polygon(
                vec![
                    egui::pos2(center.x, center.y - r),
                    egui::pos2(center.x + r, center.y + r * 0.8),
                    egui::pos2(center.x - r, center.y + r * 0.8),
                ],
                color,
                egui::Stroke::NONE,
            ));
        }
        Verdict::Red => {
            painter.rect_filled(
                egui::Rect::from_center_size(center, egui::vec2(r * 1.7, r * 1.7)),
                1.0,
                color,
            );
        }
        Verdict::Unknown => {
            painter.circle_stroke(center, r, egui::Stroke::new(1.5_f32, color));
        }
    }
}

fn verdict_line(ui: &mut egui::Ui, verdict: Verdict, label: &str, detail: &str) {
    ui.horizontal_wrapped(|ui| {
        verdict_badge(ui, verdict);
        ui.label(
            RichText::new(format!("{label} · {}", verdict.word()))
                .size(font_size(TEXT_BODY))
                .color(verdict_color(ui, verdict))
                .strong(),
        );
        ui.label(
            RichText::new(detail)
                .size(font_size(TEXT_BODY))
                .color(c_muted()),
        );
    });
}

fn assessment_ui(ui: &mut egui::Ui, assessment: &Assessment) {
    verdict_line(
        ui,
        assessment.overall,
        assessment.task.label(),
        &assessment.summary,
    );
    ui.indent(("host-assessment", assessment.task.label()), |ui| {
        for Dimension {
            aspect,
            required,
            verdict,
            text,
        } in &assessment.dimensions
        {
            if *required {
                verdict_line(ui, *verdict, aspect.label(), text);
            }
        }
    });
    ui.add_space(4.0);
}

fn card(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::none()
        .fill(c_card())
        .rounding(8.0)
        .inner_margin(egui::Margin::same(12.0))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
    ui.add_space(8.0);
}

impl RelayApp {
    pub(super) fn hosting_view(&self) -> Option<HostingView> {
        self.admin_ui
            .hosting
            .view_for(self.command_origin().as_ref())
            .cloned()
    }

    fn hosting_pending(&self) -> bool {
        self.admin_requests
            .values()
            .any(|key| key == "hosting" || key == ACTION)
    }

    /// An answer to `/api/hosting` or to a change of it.
    pub(super) fn hosting_received(&mut self, key: &str, data: Value) {
        let origin = self.command_origin();
        let instance_id = self
            .bootstrap
            .as_ref()
            .map(|b| b.instance_id.clone())
            .unwrap_or_default();
        let accepted =
            self.admin_ui
                .hosting
                .receive(key, &data, origin, &instance_id, Instant::now());
        if accepted && key == ACTION {
            self.notice = Some(("Host settings saved".into(), Instant::now()));
        }
    }

    /// A request failed after it was sent. The page offers Retry.
    pub(super) fn hosting_failed(&mut self, key: &str, message: String) {
        self.admin_ui.hosting.fail(key, message);
    }

    /// Send a hosting request through the page's own state, so a refusal to
    /// send becomes a visible failure instead of a silent no-op.
    fn hosting_request(&mut self, method: AdminMethod, path: String, body: Value, key: &str) {
        let origin = self.command_origin();
        let request_id = self.next_request();
        self.admin_ui.hosting.dispatch(
            &self.network,
            origin,
            request_id,
            &mut self.admin_requests,
            Request {
                method,
                path,
                body,
                key,
            },
        );
    }

    /// Ask for the current view. The last good one stays on screen, marked as
    /// possibly stale, until a valid answer replaces it.
    fn request_hosting(&mut self) {
        if !self.hosting_pending() {
            self.hosting_request(
                AdminMethod::Get,
                "/api/hosting".into(),
                Value::Null,
                "hosting",
            );
        }
    }

    pub(super) fn hosting_ui(&mut self, ui: &mut egui::Ui, bootstrap: &Bootstrap) {
        settings_heading(
            ui,
            "Hosts",
            "Who runs this community, and who has agreed to help.",
        );
        self.handover_ui(ui,bootstrap);
        if bootstrap.host_paused {return;}
        let Some(view) = self.hosting_view() else {
            let pending = self.hosting_pending();
            let failure = self
                .admin_ui
                .hosting
                .failure
                .as_ref()
                .map(|f| f.message.clone());
            if let (false, Some(message)) = (pending, &failure) {
                ui.label(
                    RichText::new("Relay could not load the host settings.")
                        .color(c_bad())
                        .strong(),
                );
                ui.label(
                    RichText::new(message)
                        .size(font_size(TEXT_BODY))
                        .color(c_muted()),
                );
                if ui.add(primary("Retry")).clicked() {
                    self.request_hosting();
                }
            } else {
                if should_auto_request(false, failure.is_some(), pending) {
                    self.request_hosting();
                    if self.admin_ui.hosting.failure.is_some() {
                        // Sending was refused at once; show the failure next frame.
                        ui.ctx().request_repaint();
                    }
                }
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(
                        RichText::new("Loading host settings…")
                            .size(font_size(TEXT_BODY))
                            .color(c_muted()),
                    );
                });
                ui.ctx().request_repaint_after(Duration::from_secs(1));
            }
            return;
        };
        let pending = self.hosting_pending();
        // A failed refresh keeps the last good copy, clearly marked, and blocks
        // changes made from it.
        let stale = is_stale(self.admin_ui.hosting.failure.as_ref());
        let busy = pending || stale;
        if let Some(failure) = &self.admin_ui.hosting.failure {
            let age = self
                .admin_ui
                .hosting
                .loaded
                .map(|at| at.elapsed().as_secs());
            let message = failure_message(failure, age);
            let retry = ui
                .horizontal_wrapped(|ui| {
                    ui.label(RichText::new(message).color(c_bad()));
                    if pending {
                        ui.spinner();
                        false
                    } else {
                        failure.loading && ui.add(secondary("Retry")).clicked()
                    }
                })
                .inner;
            if retry {
                self.request_hosting();
            }
            ui.add_space(6.0);
        }

        // Running now.
        card(ui, |ui| {
            ui.label(RichText::new("Running now").strong());
            ui.label(format!(
                "{} is the host. It is the only machine that accepts changes.",
                view.active_host.name
            ));
            crate::connection_path::observe(&self.login.url, ui.ctx());
            let reach = match crate::connection_path::latest(&self.login.url) {
                Some(report) => format!("Your connection to this host: {}", report.headline()),
                None => "Your connection to this host has not been measured yet.".into(),
            };
            ui.label(
                RichText::new(reach)
                    .size(font_size(TEXT_BODY))
                    .color(c_muted()),
            );
            ui.label(
                RichText::new(if view.abilities.planned_handover {
                    "Choose who can help with data copies or local voice gateways. Each member must accept their role and enable the work separately. Use Move this community to hand hosting to a prepared member."
                } else {
                    "Choose who can help with data copies or local voice gateways. Each member must accept their role and enable the work separately. Community handover is not available yet."
                })
                .size(font_size(TEXT_BODY))
                .color(c_muted()),
            );
        });

        // An offer for you.
        if let Some(mine) = view
            .candidates
            .iter()
            .find(|c| c.candidate.user_id == view.you)
        {
            let waiting = !mine.candidate.waiting_for_acceptance().is_empty();
            let approved = mine.candidate.state == CandidateState::Approved;
            if waiting || approved {
                if self.admin_ui.hosting.accept_revision != Some(view.revision) {
                    self.admin_ui.hosting.accept_revision = Some(view.revision);
                    self.admin_ui.hosting.accept_roles = [false; 2];
                    for role in &mine.candidate.accepted {
                        self.admin_ui.hosting.accept_roles[role_index(*role)] = true;
                    }
                }
                let mut answer: Option<Vec<HostRole>> = None;
                card(ui, |ui| {
                    ui.label(
                        RichText::new(if waiting {
                            "An administrator asked you to help host"
                        } else {
                            "Your host roles"
                        })
                        .strong(),
                    );
                    for role in &mine.candidate.offered {
                        let flag = &mut self.admin_ui.hosting.accept_roles[role_index(*role)];
                        ui.checkbox(flag, role.label());
                        ui.label(
                            RichText::new(role.explanation())
                                .size(font_size(TEXT_CAPTION))
                                .color(c_muted()),
                        );
                    }
                    ui.horizontal_wrapped(|ui| {
                        let picked = chosen(&self.admin_ui.hosting.accept_roles);
                        if ui
                            .add_enabled(!busy && !picked.is_empty(), primary("Save my answer"))
                            .clicked()
                        {
                            answer = Some(picked);
                        }
                        let label = if approved { "Step down" } else { "Decline" };
                        if ui.add_enabled(!busy, secondary(label)).clicked() {
                            answer = Some(Vec::new());
                        }
                    });
                    ui.label(
                        RichText::new("Accepting starts nothing by itself. Choose a copy below or enable local voice gateways for your calls. You can step down at any time.")
                            .size(font_size(TEXT_CAPTION))
                            .color(c_muted()),
                    );
                });
                if let Some(roles) = answer {
                    self.hosting_request(
                        AdminMethod::Post,
                        "/api/hosting/answer".into(),
                        json!({ "accepted": roles }),
                        ACTION,
                    );
                }
                if mine.candidate.holds(HostRole::Replica) {
                    self.copy_card(ui, busy);
                }
                if mine.candidate.holds(HostRole::VoiceGateway) {
                    card(ui, |ui| {
                        ui.label(RichText::new("Your voice gateway role is accepted").strong());
                        ui.add(egui::Label::new(RichText::new("Join a voice call and enable local voice gateways to help other people on your local network. Relay measures the complete path before selecting it; accepting this role does not start sharing by itself.").color(c_muted())).wrap(true));
                        let enabled = self.audio_applied.local_voice_gateways;
                        if ui.add(secondary(if enabled { "Stop local voice gateways" } else { "Enable local voice gateways for calls" })).clicked() {
                            self.apply_quick_audio_change(|audio| audio.local_voice_gateways = !enabled);
                        }
                        ui.label(RichText::new("This preference applies to your calls. You can also change it inside a call; video is separate.").size(font_size(TEXT_CAPTION)).color(c_muted()));
                    });
                }
            }
        }

        // Everyone with an offer, and whether they are ready.
        ui.label(RichText::new("Host candidates").strong());
        if view.candidates.is_empty() {
            ui.label(
                RichText::new(if view.can_manage {
                    "No one has been asked to host yet."
                } else {
                    "No member holds a host role yet."
                })
                .size(font_size(TEXT_BODY))
                .color(c_muted()),
            );
        }
        let mut withdraw: Option<String> = None;
        for entry in &view.candidates {
            card(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(&entry.name).strong());
                    ui.label(
                        RichText::new(state_word(entry.candidate.state))
                            .size(font_size(TEXT_BODY))
                            .color(c_muted()),
                    );
                });
                for role in &entry.candidate.offered {
                    ui.label(
                        RichText::new(role_status(entry, *role))
                            .size(font_size(TEXT_BODY))
                            .color(c_muted()),
                    );
                }
                egui::CollapsingHeader::new(format!("Is {} ready?", entry.name))
                    .id_source(("host-ready", &entry.candidate.user_id))
                    .default_open(false)
                    .show(ui, |ui| {
                        if entry.assessments.is_empty() {
                            ui.label("No role is offered, so there is nothing to judge.");
                        }
                        for assessment in &entry.assessments {
                            assessment_ui(ui, assessment);
                        }
                    });
                let open = matches!(
                    entry.candidate.state,
                    CandidateState::Pending | CandidateState::Approved
                );
                if view.can_manage && open {
                    if self.admin_ui.hosting.withdraw_confirm.as_deref()
                        == Some(entry.candidate.user_id.as_str())
                    {
                        ui.label(
                            RichText::new(format!("{} loses every host role at once.", entry.name))
                                .color(c_bad()),
                        );
                        ui.horizontal(|ui| {
                            if ui.add_enabled(!busy, primary("Confirm withdraw")).clicked() {
                                withdraw = Some(entry.candidate.user_id.clone());
                            }
                            if ui.add(secondary("Cancel")).clicked() {
                                self.admin_ui.hosting.withdraw_confirm = None;
                            }
                        });
                    } else if ui.add(secondary("Withdraw roles")).clicked() {
                        self.admin_ui.hosting.withdraw_confirm =
                            Some(entry.candidate.user_id.clone());
                    }
                }
            });
        }
        if let Some(user) = withdraw {
            self.admin_ui.hosting.withdraw_confirm = None;
            self.hosting_request(
                AdminMethod::Delete,
                format!("/api/hosting/candidates/{user}"),
                Value::Null,
                ACTION,
            );
        }

        // Offer roles.
        if view.can_manage {
            ui.add_space(6.0);
            ui.label(RichText::new("Ask a member to help host").strong());
            let mut offer: Option<(String, Vec<HostRole>)> = None;
            card(ui, |ui| {
                let selected = bootstrap
                    .members
                    .iter()
                    .find(|member| member.id == self.admin_ui.hosting.offer_member)
                    .map(|member| profile_ui::display_name(member).to_string())
                    .unwrap_or_else(|| "Choose a member".into());
                egui::ComboBox::from_id_source("host-offer-member")
                    .selected_text(selected)
                    .show_ui(ui, |ui| {
                        for member in &bootstrap.members {
                            ui.selectable_value(
                                &mut self.admin_ui.hosting.offer_member,
                                member.id.clone(),
                                profile_ui::display_name(member),
                            );
                        }
                    });
                for role in HostRole::ALL {
                    ui.checkbox(
                        &mut self.admin_ui.hosting.offer_roles[role_index(role)],
                        role.label(),
                    );
                    ui.label(
                        RichText::new(role.explanation())
                            .size(font_size(TEXT_CAPTION))
                            .color(c_muted()),
                    );
                }
                let roles = chosen(&self.admin_ui.hosting.offer_roles);
                let ready = !self.admin_ui.hosting.offer_member.is_empty() && !roles.is_empty();
                if ui
                    .add_enabled(ready && !busy, primary("Send offer"))
                    .clicked()
                {
                    offer = Some((self.admin_ui.hosting.offer_member.clone(), roles));
                }
                ui.label(
                    RichText::new("The member must accept and enable their work separately. Adding a role needs a new answer; taking one away is immediate.")
                        .size(font_size(TEXT_CAPTION))
                        .color(c_muted()),
                );
            });
            if let Some((user_id, roles)) = offer {
                self.hosting_request(
                    AdminMethod::Post,
                    "/api/hosting/offers".into(),
                    json!({ "user_id": user_id, "roles": roles }),
                    ACTION,
                );
            }
        }

        ui.horizontal(|ui| {
            if ui.add_enabled(!pending, secondary("Refresh")).clicked() {
                self.request_hosting();
            }
            if pending {
                ui.spinner();
            }
        });
        controls::divider(ui);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use relay_protocol::hosting::Roster;

    fn view_of(roster: &Roster, user: &str) -> CandidateView {
        CandidateView {
            candidate: roster.candidate(user).unwrap().clone(),
            name: "Bo".into(),
            assessments: Vec::new(),
        }
    }

    #[test]
    fn the_page_says_who_has_to_act_next() {
        let mut roster = Roster::default();
        roster
            .offer(
                "bo",
                &[HostRole::Replica, HostRole::VoiceGateway],
                "anna",
                1,
            )
            .unwrap();
        let waiting = view_of(&roster, "bo");
        assert_eq!(state_word(waiting.candidate.state), "Waiting for an answer");
        assert_eq!(
            role_status(&waiting, HostRole::Replica),
            "Data copy: waiting for Bo to accept"
        );
        roster.respond("bo", &[HostRole::Replica], 2).unwrap();
        let approved = view_of(&roster, "bo");
        assert_eq!(
            role_status(&approved, HostRole::Replica),
            "Data copy: approved"
        );
        assert_eq!(
            role_status(&approved, HostRole::VoiceGateway),
            "Voice gateway: waiting for Bo to accept"
        );
        roster.revoke("bo", 3).unwrap();
        let revoked = view_of(&roster, "bo");
        assert_eq!(state_word(revoked.candidate.state), "Withdrawn");
        assert_eq!(
            role_status(&revoked, HostRole::Replica),
            "Data copy: not active"
        );
    }

    #[test]
    fn a_failed_refresh_pauses_changes_but_a_failed_change_does_not() {
        let refresh = Failure {
            loading: true,
            message: "The server did not answer.".into(),
        };
        let change = Failure {
            loading: false,
            message: "Only an administrator can do this.".into(),
        };
        assert!(is_stale(Some(&refresh)));
        assert!(!is_stale(Some(&change)));
        assert!(!is_stale(None));
        let text = failure_message(&refresh, Some(42));
        assert!(text.contains("The server did not answer."), "{text}");
        assert!(text.contains("last copy from 42 s ago"), "{text}");
        assert!(text.contains("changes are paused"), "{text}");
        assert!(failure_message(&refresh, None).contains("last copy."));
        assert_eq!(
            failure_message(&change, Some(1)),
            "Your change was not saved: Only an administrator can do this."
        );
    }

    // ---- request state, driven through an owned channel ------------------

    use std::sync::mpsc::{sync_channel, Receiver, SyncSender};

    impl Sink for SyncSender<Command> {
        fn send(&self, command: Command) -> Result<(), String> {
            self.try_send(command)
                .map_err(|_| "Connection is busy. Please try again.".to_string())
        }
    }

    const INSTANCE: &str = "instance-1";

    fn origin() -> CommandOrigin {
        CommandOrigin {
            url: "http://127.0.0.1:4580".into(),
            server_id: "server-1".into(),
            user_id: "bo".into(),
        }
    }

    fn reply(schema: u64, you: &str, instance: &str, revision: u64) -> Value {
        json!({
            "schema": schema,
            "revision": revision,
            "active_host": {"name": "Hosts", "instance_id": instance},
            "abilities": {"planned_handover": false, "unexpected_takeover": false},
            "you": you,
            "can_manage": false,
            "candidates": []
        })
    }

    fn good(revision: u64) -> Value {
        reply(1, "bo", INSTANCE, revision)
    }

    /// A channel whose far end is gone: every send is refused at once.
    fn disconnected() -> SyncSender<Command> {
        let (sender, receiver) = sync_channel(1);
        drop(receiver);
        sender
    }

    fn connected() -> (SyncSender<Command>, Receiver<Command>) {
        sync_channel(4)
    }

    fn get(
        state: &mut State,
        sink: &impl Sink,
        origin: Option<CommandOrigin>,
        id: u64,
        requests: &mut HashMap<u64, String>,
    ) {
        state.dispatch(
            sink,
            origin,
            id,
            requests,
            Request {
                method: AdminMethod::Get,
                path: "/api/hosting".into(),
                body: Value::Null,
                key: "hosting",
            },
        );
    }

    fn accept(state: &mut State, key: &str, data: Value) -> bool {
        state.receive(key, &data, Some(origin()), INSTANCE, Instant::now())
    }

    fn revision(state: &State) -> Option<u64> {
        state.view_for(Some(&origin())).map(|view| view.revision)
    }

    #[test]
    fn an_initial_send_failure_is_a_failure_to_retry_not_an_endless_load() {
        let mut state = State::default();
        let mut requests = HashMap::new();
        assert!(should_auto_request(false, false, false));
        get(
            &mut state,
            &disconnected(),
            Some(origin()),
            1,
            &mut requests,
        );
        assert!(
            requests.is_empty(),
            "nothing is pending after a refused send"
        );
        let failure = state.failure.as_ref().expect("the refusal is recorded");
        assert!(failure.loading);
        assert_eq!(failure.message, "Connection is busy. Please try again.");
        assert!(state.view_for(Some(&origin())).is_none());
        // The page no longer loads by itself; it waits for the Retry button.
        assert!(!should_auto_request(false, true, false));
        // The same holds with no connected community at all.
        let mut offline = State::default();
        get(&mut offline, &disconnected(), None, 2, &mut requests);
        assert!(requests.is_empty());
        assert_eq!(
            offline.failure.as_ref().unwrap().message,
            "Connect to a community before changing its settings."
        );
        assert!(!should_auto_request(false, true, false));
    }

    #[test]
    fn a_refused_refresh_keeps_the_old_view_stale_and_changes_paused() {
        let mut state = State::default();
        assert!(accept(&mut state, "hosting", good(3)));
        assert_eq!(revision(&state), Some(3));
        assert!(!is_stale(state.failure.as_ref()));
        // The refresh was sent, then the server could not be reached.
        state.fail("hosting", "The server did not answer.".into());
        assert!(is_stale(state.failure.as_ref()));
        // Retry is refused when it is sent. It must not clear the failure.
        let mut requests = HashMap::new();
        get(
            &mut state,
            &disconnected(),
            Some(origin()),
            7,
            &mut requests,
        );
        assert!(requests.is_empty());
        assert!(is_stale(state.failure.as_ref()), "still paused");
        assert_eq!(revision(&state), Some(3), "the last good copy stays");
        assert_eq!(
            state.failure.as_ref().unwrap().message,
            "Connection is busy. Please try again."
        );
        // A change that is refused at send is shown too, without making the
        // view stale if it was fresh.
        let mut fresh = State::default();
        accept(&mut fresh, "hosting", good(4));
        fresh.dispatch(
            &disconnected(),
            Some(origin()),
            8,
            &mut requests,
            Request {
                method: AdminMethod::Post,
                path: "/api/hosting/answer".into(),
                body: json!({"accepted": []}),
                key: ACTION,
            },
        );
        assert!(requests.is_empty());
        assert!(fresh.failure.is_some());
        assert!(
            !is_stale(fresh.failure.as_ref()),
            "a refused change leaves the view as it was"
        );
    }

    #[test]
    fn a_malformed_view_is_rejected_and_the_last_good_copy_stays() {
        let mut state = State::default();
        // Nothing loaded yet: the page must show a failure, not load forever.
        for junk in [
            json!({"nonsense": 1}),
            json!([1, 2]),
            json!("text"),
            Value::Null,
        ] {
            assert!(!accept(&mut state, "hosting", junk));
            assert!(state.view_for(Some(&origin())).is_none());
            assert!(
                state.loaded.is_none(),
                "a bad answer is never marked loaded"
            );
            let failure = state.failure.as_ref().unwrap();
            assert!(failure.loading);
            assert_eq!(failure.message, "The server's answer could not be read.");
            assert!(!should_auto_request(false, true, false));
        }
        // With a good copy on screen the bad answer must not replace or clear it.
        assert!(accept(&mut state, "hosting", good(5)));
        let loaded = state.loaded;
        assert!(!accept(
            &mut state,
            "hosting",
            json!({"schema": 1, "you": "bo"})
        ));
        assert_eq!(revision(&state), Some(5));
        assert_eq!(state.loaded, loaded);
        assert!(is_stale(state.failure.as_ref()));
    }

    #[test]
    fn an_unsupported_schema_or_the_wrong_account_or_session_is_refused() {
        let mut state = State::default();
        assert!(accept(&mut state, "hosting", good(2)));
        for (bad, expected) in [
            (
                reply(2, "bo", INSTANCE, 9),
                "different version of the host settings",
            ),
            (
                reply(0, "bo", INSTANCE, 9),
                "different version of the host settings",
            ),
            (reply(1, "clara", INSTANCE, 9), "different account"),
            (reply(1, "bo", "instance-2", 9), "different server session"),
        ] {
            assert!(!accept(&mut state, "hosting", bad));
            let message = &state.failure.as_ref().unwrap().message;
            assert!(message.contains(expected), "{message}");
            assert_eq!(revision(&state), Some(2), "{expected}: the good copy stays");
        }
        // A view saved for another community or account is never shown here.
        let other = CommandOrigin {
            user_id: "clara".into(),
            ..origin()
        };
        assert!(state.view_for(Some(&other)).is_none());
        assert!(state.view_for(None).is_none());
    }

    #[test]
    fn an_unreadable_answer_to_a_change_is_never_reported_as_saved() {
        let mut state = State::default();
        assert!(accept(&mut state, "hosting", good(1)));
        assert!(!accept(&mut state, ACTION, json!({"oops": true})));
        let failure = state.failure.as_ref().unwrap();
        assert!(failure.loading, "the screen may no longer match the server");
        assert!(failure.message.contains("may or may not have been saved"));
        assert_eq!(revision(&state), Some(1));
        // A valid answer to a change is accepted and replaces the view.
        assert!(accept(&mut state, ACTION, good(2)));
        assert_eq!(revision(&state), Some(2));
        assert!(state.failure.is_none());
    }

    #[test]
    fn a_valid_retry_recovers_from_every_kind_of_failure() {
        let mut state = State::default();
        let mut requests = HashMap::new();
        get(
            &mut state,
            &disconnected(),
            Some(origin()),
            1,
            &mut requests,
        );
        assert!(state.failure.is_some());
        // Retry reaches the worker this time.
        let (sink, inbox) = connected();
        get(&mut state, &sink, Some(origin()), 2, &mut requests);
        assert_eq!(requests.get(&2).map(String::as_str), Some("hosting"));
        match inbox.try_recv().expect("the request was sent") {
            Command::Admin {
                method,
                path,
                request_id,
                ..
            } => {
                assert!(matches!(method, AdminMethod::Get));
                assert_eq!((path.as_str(), request_id), ("/api/hosting", 2));
            }
            _ => panic!("expected an administration request"),
        }
        // The failure stays until a valid answer arrives, so it is not hidden
        // while the request is in flight.
        assert!(state.failure.is_some());
        // The first answer is garbage: still a failure, retry again.
        assert!(!accept(&mut state, "hosting", json!({"schema": 99})));
        assert!(state.view_for(Some(&origin())).is_none());
        get(&mut state, &sink, Some(origin()), 3, &mut requests);
        assert!(accept(&mut state, "hosting", good(6)));
        assert_eq!(revision(&state), Some(6));
        assert!(state.failure.is_none());
        assert!(state.loaded.is_some());
        assert!(!is_stale(state.failure.as_ref()));
    }

    #[test]
    fn only_a_page_with_no_view_no_failure_and_no_request_loads_by_itself() {
        for (has_view, failed, pending, expected) in [
            (false, false, false, true),
            (false, false, true, false),
            (false, true, false, false),
            (false, true, true, false),
            (true, false, false, false),
            (true, true, false, false),
        ] {
            assert_eq!(should_auto_request(has_view, failed, pending), expected);
        }
    }

    #[test]
    fn the_page_no_longer_claims_that_hosts_copy_data_or_route_voice() {
        // Negative checks only: the needles are built so this test cannot match itself.
        let source = include_str!("host_ui.rs");
        assert!(!source.contains(concat!("keep copies or ", "relay voice")));
        assert!(!source.contains(concat!("\"Approved", " hosts\"")));
    }

    #[test]
    fn role_flags_map_to_the_roles_the_server_expects() {
        assert!(chosen(&[false, false]).is_empty());
        assert_eq!(chosen(&[true, false]), vec![HostRole::Replica]);
        assert_eq!(
            chosen(&[true, true]),
            vec![HostRole::Replica, HostRole::VoiceGateway]
        );
        assert_eq!(
            serde_json::to_value(chosen(&[false, true])).unwrap(),
            json!(["voice_gateway"])
        );
    }

    #[test]
    fn every_verdict_has_its_own_word_so_colour_is_never_alone() {
        let words: std::collections::HashSet<_> = [
            Verdict::Green,
            Verdict::Yellow,
            Verdict::Red,
            Verdict::Unknown,
        ]
        .into_iter()
        .map(|v| v.word())
        .collect();
        assert_eq!(words.len(), 4);
    }
}
