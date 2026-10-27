//! Focused routing/cancellation and actual egui Friends connection rendering.
use super::*;

#[test]
fn verified_account_routes_to_shell_without_a_friends_session_but_keeps_security_gates() {
    assert!(signed_shell_allowed(true, false, false, false));
    for gates in [(false, false, false, false), (true, true, false, false),
                  (true, false, true, false), (true, false, false, true)] {
        assert!(!signed_shell_allowed(gates.0, gates.1, gates.2, gates.3));
    }
}

fn rendered_connection(error: Option<&str>, connecting: bool) -> String {
    fn text(shape: &egui::epaint::Shape, labels: &mut Vec<String>) {
        match shape {
            egui::epaint::Shape::Text(value) => labels.push(value.galley.text().to_owned()),
            egui::epaint::Shape::Vec(shapes) => for shape in shapes { text(shape, labels); },
            _ => {}
        }
    }
    let ctx = egui::Context::default();
    let mut fonts = egui::FontDefinitions::default();
    let proportional = fonts.families[&egui::FontFamily::Proportional].clone();
    fonts.families.insert(egui::FontFamily::Name("Relay Semibold".into()), proportional);
    ctx.set_fonts(fonts);
    let output = ctx.run(egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(940.0, 650.0))),
        ..Default::default()
    }, |ctx| {
        egui::CentralPanel::default().show(ctx, |ui| {
            assert!(friends_connection_contents(ui, error, connecting).is_none());
        });
    });
    let mut labels = Vec::new();
    for shape in output.shapes { text(&shape.shape, &mut labels); }
    labels.join("\n")
}

#[test]
fn friends_loading_renders_inside_the_page_without_welcome_or_community_rows() {
    let labels = rendered_connection(None, true);
    assert!(labels.contains("Friends"));
    assert!(labels.contains("Connecting to Friends"));
    assert!(labels.contains("Cancel connection"));
    assert!(!labels.contains("Welcome") && !labels.contains("Open Friends") && !labels.contains("Your servers"));
    assert!(!labels.contains("Friends on") && !labels.contains("This server"));
}

#[test]
fn friends_failure_and_cancelled_connection_keep_an_inline_retry() {
    for error in [Some("Owned Friends service unavailable"), Some("Connection cancelled."), None] {
        let labels = rendered_connection(error, false);
        assert!(labels.contains("Retry Friends"));
        if let Some(error) = error { assert!(labels.contains(error)); }
        assert!(!labels.contains("Welcome") && !labels.contains("Open Friends"));
    }
}

#[test]
fn user_navigation_cancels_background_friends_and_rejects_its_late_resume() {
    let cancel = Arc::new(AtomicBool::new(false));
    let friends = "https://owned.invalid/people";
    let mut state = State {
        entered: true, friends_requested: true,
        friends_resume: Some((friends.into(), Instant::now())),
        auto: Some(AutoEntry { queue: Default::default(), friends: false,
            started: Instant::now(), resuming: None }),
        pending: Some(Attempt {
            url: friends.into(), expected: None, request: 7, other: false, people: true,
            account: crate::central_account::Account { issuer: "https://owned.invalid/realm".into(),
                subject: "owned-account".into(), handle: "owned_user".into(), expires_at: u64::MAX },
            cancel: cancel.clone(), form_cancel: Arc::new(AtomicBool::new(false)),
            browser: None, challenge: None, authority: None, background: false, detached: false,
        }),
        ..Default::default()
    };
    state.cancel_navigation(Some("https://owned.invalid/community"));
    assert!(cancel.load(Ordering::Acquire));
    assert!(state.pending.is_none() && state.auto.is_none() && state.friends_resume.is_none());
    assert!(!state.friends_requested && !state.home);
    assert!(state.entered, "A late response must not restart automatic navigation");
    assert!(state.cancelled_navigation.contains(friends));
    // An explicit later choice for the same destination creates a fresh intent.
    state.cancel_navigation(Some(friends));
    assert!(!state.cancelled_navigation.contains(friends));
}
