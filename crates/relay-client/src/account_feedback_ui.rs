//! R5.2: account form errors shown where they belong. A known problem with one
//! field appears directly under that field; everything else stays a general
//! message at the top of the form. A server answer reaches a field only through
//! the request id of the exact `Command::Auth` that failed (`RequestFailed`);
//! untyped errors always stay general. A field error belongs to one attempt
//! (flow, server and action) and disappears when the person edits that field,
//! switches server or flow, or submits again. Passwords, tokens and field
//! contents are never stored here.
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AccountFlow {
    /// The start screen: create a server, join friends or sign in again.
    SignIn,
    /// "Join friends" while already signed in to another server.
    AnotherServer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AccountField {
    Username,
    Password,
}

/// One account form as submitted: which flow, which server, which action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct AccountAttempt {
    pub flow: AccountFlow,
    pub server: String,
    pub register: bool,
}

impl AccountAttempt {
    pub(super) fn new(flow: AccountFlow, server: &str, register: bool) -> Self {
        Self {
            flow,
            server: crate::community::normalize_url(server)
                .unwrap_or_else(|_| server.trim().to_owned()),
            register,
        }
    }
}

struct FieldError {
    attempt: AccountAttempt,
    field: AccountField,
    message: &'static str,
}

/// The server's answer that is about exactly one field. Only this is moved to
/// a field; "access denied" style answers stay general, because they do not
/// say which field is wrong.
fn known_field_error(error: &str, register: bool) -> Option<(AccountField, &'static str)> {
    (register && error == "Username is already taken").then_some((
        AccountField::Username,
        "This username is taken on this server. Choose another one.",
    ))
}

/// relay-server's account rules (`validate_auth`), checked before a new
/// account is sent so the answer can name the field.
fn new_account_problem(username: &str, password: &str) -> Option<(AccountField, &'static str)> {
    let name = username.trim();
    if name.len() < 2
        || name.len() > 32
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        Some((
            AccountField::Username,
            "Use 2–32 of a–z, 0–9, dots, dashes or underscores.",
        ))
    } else if password.len() < 10 || password.len() > 256 {
        Some((
            AccountField::Password,
            "Use a password of 10 to 256 characters.",
        ))
    } else {
        None
    }
}

/// What became of a failed request.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum AuthAnswer {
    /// Not an account request from these forms; the caller handles it.
    NotAccount(String),
    /// Shown under the field it is about.
    OnField,
    /// Shown as the general message at the top of the form.
    General(String),
}

const REMEMBERED_REQUESTS: usize = 16;

#[derive(Default)]
pub(super) struct AccountFeedback {
    /// The request whose answer is still outstanding, and its form.
    pending: Option<(u64, AccountAttempt)>,
    /// Recent account request ids, so an older answer is still shown, generally.
    issued: std::collections::VecDeque<u64>,
    field: Option<FieldError>,
    /// The frame in which the form drew the general message itself.
    general_frame: Option<u64>,
}

impl AccountFeedback {
    /// Checks what the app knows before sending. Returns false, with a field
    /// error, when the form cannot be sent yet; otherwise records the request
    /// id that `Command::Auth` will carry.
    pub(super) fn submit(
        &mut self,
        attempt: AccountAttempt,
        request_id: u64,
        username: &str,
        password: &str,
    ) -> bool {
        self.field = None;
        self.pending = None;
        let local = if username.trim().is_empty() {
            Some((AccountField::Username, "Enter a username."))
        } else if password.is_empty() {
            Some((
                AccountField::Password,
                if attempt.register {
                    "Choose a password."
                } else {
                    "Enter your password."
                },
            ))
        } else if attempt.register {
            new_account_problem(username, password)
        } else {
            None
        };
        if let Some((field, message)) = local {
            self.field = Some(FieldError {
                attempt,
                field,
                message,
            });
            return false;
        }
        self.issued.push_back(request_id);
        if self.issued.len() > REMEMBERED_REQUESTS {
            self.issued.pop_front();
        }
        self.pending = Some((request_id, attempt));
        true
    }

    /// `RequestFailed` for `request_id`. Only the outstanding request of the
    /// form may mark a field; an older account request is shown generally.
    pub(super) fn failed(&mut self, request_id: u64, error: String) -> AuthAnswer {
        if !self.issued.contains(&request_id) {
            return AuthAnswer::NotAccount(error);
        }
        let Some((_, attempt)) = self.pending.take_if(|(id, _)| *id == request_id) else {
            return AuthAnswer::General(error);
        };
        match known_field_error(&error, attempt.register) {
            Some((field, message)) => {
                self.field = Some(FieldError {
                    attempt,
                    field,
                    message,
                });
                AuthAnswer::OnField
            }
            None => AuthAnswer::General(error),
        }
    }

    /// Signed in: nothing from this form applies any longer.
    pub(super) fn finished(&mut self) {
        self.pending = None;
        self.field = None;
    }

    /// The person changed the field. Its error no longer describes it, and an
    /// answer still on its way was given for the old text, so it stays general.
    pub(super) fn edited(&mut self, field: AccountField) {
        self.pending = None;
        if self
            .field
            .as_ref()
            .is_some_and(|error| error.field == field)
        {
            self.field = None;
        }
    }

    /// The form now shows `current`. Anything bound to another flow, server or
    /// action is dropped, so it cannot return when the person switches back.
    pub(super) fn showing(&mut self, current: &AccountAttempt) {
        if self
            .pending
            .as_ref()
            .is_some_and(|(_, attempt)| attempt != current)
        {
            self.pending = None;
        }
        if self
            .field
            .as_ref()
            .is_some_and(|error| &error.attempt != current)
        {
            self.field = None;
        }
    }

    /// The error for this field, only for the form it was reported for.
    pub(super) fn field_error(
        &self,
        current: &AccountAttempt,
        field: AccountField,
    ) -> Option<&'static str> {
        self.field
            .as_ref()
            .filter(|error| error.field == field && &error.attempt == current)
            .map(|error| error.message)
    }

    pub(super) fn drew_general(&self, ctx: &egui::Context) -> bool {
        self.general_frame == Some(ctx.frame_nr())
    }
}

/// The app's normal labelled field, with the error drawn right under it.
pub(super) fn account_field(
    ui: &mut egui::Ui,
    feedback: &mut AccountFeedback,
    current: &AccountAttempt,
    which: AccountField,
    value: &mut String,
) -> egui::Response {
    let (label, password) = match which {
        AccountField::Username => ("USERNAME", false),
        AccountField::Password => ("PASSWORD", true),
    };
    feedback.showing(current);
    let response = field(ui, label, value, password);
    if response.changed() {
        feedback.edited(which);
    }
    if let Some(message) = feedback.field_error(current, which) {
        ui.painter().rect_stroke(
            response.rect.expand(1.0),
            Rounding::same(4.0),
            Stroke::new(1.5_f32, c_bad()),
        );
        ui.add_space(4.0);
        ui.add(
            egui::Label::new(RichText::new(message).size(font_size(13.0)).color(c_bad()))
                .wrap(true),
        );
    }
    response
}

/// The top of the form: a general error, or that the new local server is ready.
pub(super) fn form_status(
    ui: &mut egui::Ui,
    feedback: &mut AccountFeedback,
    error: Option<&str>,
    local_ready: bool,
) {
    let (text, color) = match (error, local_ready) {
        (Some(error), _) => {
            feedback.general_frame = Some(ui.ctx().frame_nr());
            (error, c_bad())
        }
        (None, true) => (
            "Your server is running on this PC. Create the first account to manage it.",
            c_good(),
        ),
        (None, false) => return,
    };
    egui::Frame::none()
        .fill(c_panel())
        .stroke(Stroke::new(1.0_f32, color))
        .rounding(6.0)
        .inner_margin(Margin::same(10.0))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.add(egui::Label::new(RichText::new(text).color(color)).wrap(true));
        });
    ui.add_space(8.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOME: &str = "http://127.0.0.1:4580";
    const FRIEND: &str = "http://100.70.1.2:4580";

    fn sign_up(server: &str) -> AccountAttempt {
        AccountAttempt::new(AccountFlow::SignIn, server, true)
    }

    /// relay-server's answer when a new account's username exists.
    fn taken() -> String {
        "Username is already taken".into()
    }

    #[test]
    fn a_known_failure_lands_on_its_field_for_that_request_only() {
        let mut feedback = AccountFeedback::default();
        let attempt = sign_up(HOME);
        assert!(feedback.submit(attempt.clone(), 7, "maria", "long enough secret"));
        assert_eq!(feedback.failed(7, taken()), AuthAnswer::OnField);
        assert!(feedback
            .field_error(&attempt, AccountField::Username)
            .is_some());
        assert!(feedback
            .field_error(&attempt, AccountField::Password)
            .is_none());
        // Same form, other server or other flow, or sign in instead: not shown.
        for other in [
            sign_up(FRIEND),
            AccountAttempt::new(AccountFlow::AnotherServer, HOME, true),
            AccountAttempt::new(AccountFlow::SignIn, HOME, false),
        ] {
            assert!(feedback
                .field_error(&other, AccountField::Username)
                .is_none());
        }
        // A normalised address is the same server.
        assert!(feedback
            .field_error(&sign_up("http://127.0.0.1:4580/"), AccountField::Username)
            .is_some());
    }

    #[test]
    fn unknown_ambiguous_and_unrelated_failures_stay_general() {
        let mut feedback = AccountFeedback::default();
        let attempt = AccountAttempt::new(AccountFlow::SignIn, HOME, false);
        for (id, error) in [
            (1, "Access denied"),
            (2, "Invalid credentials"),
            (3, "Unable to reach the server."),
            // "Taken" only means the username when creating an account.
            (4, "Username is already taken"),
        ] {
            assert!(feedback.submit(attempt.clone(), id, "maria", "secret"));
            assert_eq!(
                feedback.failed(id, error.into()),
                AuthAnswer::General(error.into())
            );
            assert!(feedback
                .field_error(&attempt, AccountField::Username)
                .is_none());
            assert!(feedback
                .field_error(&attempt, AccountField::Password)
                .is_none());
        }
        // Another feature's request id is handed back untouched, even while an
        // account request is outstanding.
        assert!(feedback.submit(sign_up(HOME), 5, "maria", "long enough secret"));
        assert_eq!(
            feedback.failed(99, taken()),
            AuthAnswer::NotAccount(taken())
        );
        assert!(feedback
            .field_error(&sign_up(HOME), AccountField::Username)
            .is_none());
    }

    #[test]
    fn an_answer_for_text_edited_while_waiting_stays_general() {
        for edited in [AccountField::Username, AccountField::Password] {
            let mut feedback = AccountFeedback::default();
            let attempt = sign_up(HOME);
            assert!(feedback.submit(attempt.clone(), 41, "old-name", "long enough secret"));
            // The person edits while request 41 is still on its way.
            feedback.edited(edited);
            assert_eq!(feedback.failed(41, taken()), AuthAnswer::General(taken()));
            assert!(feedback
                .field_error(&attempt, AccountField::Username)
                .is_none());
        }
    }

    #[test]
    fn switching_form_and_back_never_brings_an_old_error_back() {
        let mut feedback = AccountFeedback::default();
        let home = sign_up(HOME);
        assert!(feedback.submit(home.clone(), 1, "maria", "long enough secret"));
        assert_eq!(feedback.failed(1, taken()), AuthAnswer::OnField);
        feedback.showing(&sign_up(FRIEND));
        feedback.showing(&home);
        assert!(feedback
            .field_error(&home, AccountField::Username)
            .is_none());
        // An outstanding answer is also given up when the form changes.
        assert!(feedback.submit(home.clone(), 2, "maria", "long enough secret"));
        feedback.showing(&AccountAttempt::new(AccountFlow::SignIn, HOME, false));
        feedback.showing(&home);
        assert_eq!(feedback.failed(2, taken()), AuthAnswer::General(taken()));
        assert!(feedback
            .field_error(&home, AccountField::Username)
            .is_none());
    }

    #[test]
    fn an_old_answer_cannot_mark_a_new_request() {
        let mut feedback = AccountFeedback::default();
        let first = sign_up(HOME);
        assert!(feedback.submit(first.clone(), 1, "maria", "long enough secret"));
        // A second submit before the first answer: the first answer is general.
        let second = sign_up(HOME);
        assert!(feedback.submit(second.clone(), 2, "maria", "long enough secret"));
        assert_eq!(feedback.failed(1, taken()), AuthAnswer::General(taken()));
        assert!(feedback
            .field_error(&second, AccountField::Username)
            .is_none());
        // The current request still owns its own answer.
        assert_eq!(feedback.failed(2, taken()), AuthAnswer::OnField);
        // Signing in finishes the form; a late failure is only general.
        assert!(feedback.submit(sign_up(FRIEND), 3, "maria", "long enough secret"));
        feedback.finished();
        assert_eq!(feedback.failed(3, taken()), AuthAnswer::General(taken()));
        assert!(feedback
            .field_error(&sign_up(FRIEND), AccountField::Username)
            .is_none());
    }

    #[test]
    fn empty_fields_and_new_account_rules_are_marked_before_sending() {
        let mut feedback = AccountFeedback::default();
        let attempt = sign_up(HOME);
        assert!(!feedback.submit(attempt.clone(), 1, "  ", "secret"));
        assert!(feedback
            .field_error(&attempt, AccountField::Username)
            .is_some());
        feedback.edited(AccountField::Password);
        assert!(feedback
            .field_error(&attempt, AccountField::Username)
            .is_some());
        feedback.edited(AccountField::Username);
        assert!(feedback
            .field_error(&attempt, AccountField::Username)
            .is_none());
        assert!(!feedback.submit(attempt.clone(), 2, "maria", ""));
        assert!(feedback
            .field_error(&attempt, AccountField::Password)
            .is_some());
        // A new account follows relay-server's rules before it is sent.
        assert!(!feedback.submit(attempt.clone(), 3, "maria", "short"));
        assert!(feedback
            .field_error(&attempt, AccountField::Password)
            .is_some());
        assert!(!feedback.submit(attempt.clone(), 4, "m", "long enough secret"));
        assert!(feedback
            .field_error(&attempt, AccountField::Username)
            .is_some());
        assert!(!feedback.submit(attempt.clone(), 5, "maria smith", "long enough secret"));
        assert!(feedback
            .field_error(&attempt, AccountField::Username)
            .is_some());
        // Nothing was sent, so none of those ids is an account request.
        for id in 1..=5 {
            assert_eq!(
                feedback.failed(id, taken()),
                AuthAnswer::NotAccount(taken())
            );
        }
        // Signing in to an existing account sends what the person typed.
        let sign_in = AccountAttempt::new(AccountFlow::SignIn, HOME, false);
        assert!(feedback.submit(sign_in, 6, "maria", "short"));
    }

    struct Drawn {
        username: Rect,
        password: Rect,
        message: Option<Rect>,
        status: Rect,
        clip: Rect,
        username_text: String,
    }

    /// Three frames; `events[n]` is the input of frame n.
    fn draw(
        size: Vec2,
        text: f32,
        feedback: &mut AccountFeedback,
        events: [Vec<egui::Event>; 3],
    ) -> Drawn {
        UI_FONT_SCALE.with(|scale| scale.set(text));
        let ctx = egui::Context::default();
        configure(&ctx);
        let attempt = sign_up(HOME);
        let mut username = String::from("maria");
        let mut password = String::from("long enough secret");
        let mut drawn = None;
        for input in events {
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size)),
                    events: input,
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        egui::ScrollArea::vertical().show(ui, |ui| {
                            let top = ui.cursor().top();
                            form_status(ui, feedback, Some("Unable to reach the server."), false);
                            let status = Rect::from_x_y_ranges(
                                ui.max_rect().x_range(),
                                top..=ui.cursor().top(),
                            );
                            let name = account_field(
                                ui,
                                feedback,
                                &attempt,
                                AccountField::Username,
                                &mut username,
                            );
                            let after = ui.cursor().top();
                            let pass = account_field(
                                ui,
                                feedback,
                                &attempt,
                                AccountField::Password,
                                &mut password,
                            );
                            let message = (after > name.rect.bottom() + 6.0).then(|| {
                                Rect::from_x_y_ranges(
                                    name.rect.x_range(),
                                    name.rect.bottom()..=after,
                                )
                            });
                            drawn = Some(Drawn {
                                username: name.rect,
                                password: pass.rect,
                                message,
                                status,
                                clip: ui.clip_rect(),
                                username_text: username.clone(),
                            });
                        });
                    });
                },
            );
        }
        UI_FONT_SCALE.with(|scale| scale.set(1.0));
        drawn.expect("drawn")
    }

    #[test]
    fn the_message_sits_under_its_field_in_small_windows_and_large_text() {
        for (size, text) in [
            (Vec2::new(940.0, 650.0), 1.0),
            (Vec2::new(320.0, 480.0), 1.0),
            (Vec2::new(320.0, 480.0), 1.4),
        ] {
            let mut feedback = AccountFeedback::default();
            assert!(feedback.submit(sign_up(HOME), 1, "maria", "long enough secret"));
            assert_eq!(feedback.failed(1, taken()), AuthAnswer::OnField);
            let drawn = draw(size, text, &mut feedback, Default::default());
            let message = drawn.message.expect("field message is drawn");
            // Directly under the username field, before the next field, and
            // no further away than one field's height.
            assert!(message.top() >= drawn.username.bottom() - 0.5);
            assert!(message.bottom() <= drawn.password.top());
            assert!(message.top() - drawn.username.bottom() <= drawn.username.height());
            assert!(
                drawn.username.right() <= drawn.clip.right() + 0.5,
                "{size:?}"
            );
            // The general message is at the top, above the fields.
            assert!(drawn.status.bottom() <= drawn.username.top());
            assert!(drawn.status.height() > 0.0);
            assert_eq!(drawn.username_text, "maria", "input is kept");
        }
    }

    #[test]
    fn typing_in_the_field_removes_its_message_and_keeps_the_text() {
        let mut feedback = AccountFeedback::default();
        assert!(feedback.submit(sign_up(HOME), 1, "maria", "long enough secret"));
        assert_eq!(feedback.failed(1, taken()), AuthAnswer::OnField);
        let before = draw(
            Vec2::new(500.0, 600.0),
            1.0,
            &mut feedback,
            Default::default(),
        );
        let at = before.username.center();
        let drawn = draw(
            Vec2::new(500.0, 600.0),
            1.0,
            &mut feedback,
            [
                Vec::new(),
                vec![
                    egui::Event::PointerMoved(at),
                    egui::Event::PointerButton {
                        pos: at,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                    egui::Event::PointerButton {
                        pos: at,
                        button: egui::PointerButton::Primary,
                        pressed: false,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
                vec![egui::Event::Text("2".into())],
            ],
        );
        assert_eq!(drawn.username_text.len(), 6, "typed into the kept text");
        assert!(drawn.username_text.contains('2'));
        assert!(feedback
            .field_error(&sign_up(HOME), AccountField::Username)
            .is_none());
    }
}
