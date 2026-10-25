//! Guided recovery help on the Help page: pick what happened, read honest steps,
//! and open the existing page where the person can act.
//!
//! Local text only. Nothing here resets, exports, approves or contacts anything:
//! rendering never acts, and the one button opens an existing settings page
//! exactly as the page's own navigation does. Every Relay screen name used in
//! the steps is checked against the source that draws it (see the tests), so
//! the guide cannot send people to a button that does not exist.
use super::*;
use crate::settings_ui as controls;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Problem {
    SignIn,
    Device,
    History,
}

/// Existing pages the guide can open. Both are plain settings tabs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Action {
    OpenAccount,
    OpenEncryption,
}

impl Action {
    fn label(self) -> &'static str {
        match self {
            Self::OpenAccount => "Open Account",
            Self::OpenEncryption => "Open Encryption",
        }
    }
}

impl Problem {
    pub(super) const ALL: [Self; 3] = [Self::SignIn, Self::Device, Self::History];

    fn label(self) -> &'static str {
        match self {
            Self::SignIn => "Sign-in trouble",
            Self::Device => "New or lost device",
            Self::History => "Missing message history",
        }
    }

    fn steps(self) -> &'static [&'static str] {
        match self {
            Self::SignIn => &[
                "Check which account you are using. Your Relay account and an account on one community are separate and have their own passwords.",
                "Open Account to see whether you are signed in to your Relay account.",
                "If a code is requested, enter your authenticator's 6-digit code or one of your saved recovery codes.",
                "Password, authenticator and recovery codes for a Relay account are changed at the Relay account service. Once you are signed in, Account has a link to it. This guide cannot reset or bypass a sign-in.",
            ],
            Self::Device => &[
                "Sign in on the new device. It gets its own encryption identity; your old device's keys do not move over.",
                "Open Encryption, expand Your security code, and send your code or verification card to the people you chat with through a contact you already trust.",
                "They open your device in Encryption, check the code and choose Approve Device. Until then, protected conversations and calls with them cannot resume on the new device.",
                "Lost a device? On another device you still have, open Encryption, expand Advanced and choose Revoke This Device under Your registered devices.",
            ],
            Self::History => &[
                "Open Encryption and expand Backup and recovery. If you made a history backup earlier, open that file with its passphrase to read the saved messages.",
                "A backup is a read-only copy of messages this PC had already opened. It does not restore live chat, approve a device or let you send.",
                "Without a backup, messages that only a lost device could read cannot be recovered by Relay or by the server.",
                "Next time, make a history backup before you move to a new device.",
            ],
        }
    }

    fn action(self) -> Action {
        match self {
            Self::SignIn => Action::OpenAccount,
            Self::Device | Self::History => Action::OpenEncryption,
        }
    }
}

/// Shown under every guide, so no one has to find them.
pub(super) const LIMITS: [&str; 3] = [
    "Changing or resetting an account password does not restore old device keys or message history.",
    "Other devices and people you trust may need to approve a new device before group keys are shared with it. An existing group may also need the device that started its protected conversation to be online.",
    "A server backup cannot by itself recreate keys held only by a lost device. There is no server-side recovery for encryption keys.",
];

pub(super) struct Drawn {
    pub chosen: Option<Problem>,
    pub action: Option<Action>,
    pub choosers: Vec<(Problem, Rect)>,
    pub button: Option<Rect>,
}

/// Draws the section. Returns the (possibly changed) choice and a clicked action.
pub(super) fn content(ui: &mut egui::Ui, chosen: Option<Problem>) -> Drawn {
    controls::divider(ui);
    controls::section_heading(
        ui,
        "Recovery",
        "Choose what happened to see the steps. Nothing here changes your account or devices.",
    );
    let mut next = chosen;
    let mut choosers = Vec::new();
    ui.horizontal_wrapped(|ui| {
        for problem in Problem::ALL {
            let response = ui.selectable_label(chosen == Some(problem), problem.label());
            choosers.push((problem, response.rect));
            if response.clicked() {
                next = (chosen != Some(problem)).then_some(problem);
            }
        }
    });
    let mut action = None;
    let mut button = None;
    if let Some(problem) = chosen {
        ui.add_space(6.0);
        for (index, text) in problem.steps().iter().enumerate() {
            ui.add(egui::Label::new(format!("{}. {text}", index + 1)).wrap(true));
            ui.add_space(4.0);
        }
        ui.add_space(4.0);
        let response = ui.add(secondary(problem.action().label()));
        button = Some(response.rect);
        if response.clicked() {
            action = Some(problem.action());
        }
    }
    ui.add_space(10.0);
    egui::Frame::none()
        .fill(c_card())
        .rounding(6.0)
        .inner_margin(Margin::same(10.0))
        .show(ui, |ui| {
            ui.label(RichText::new("What recovery can and cannot do").strong());
            for text in LIMITS {
                ui.add(egui::Label::new(format!("• {text}")).wrap(true));
                ui.add_space(2.0);
            }
        });
    Drawn {
        chosen: next,
        action,
        choosers,
        button,
    }
}

impl RelayApp {
    pub(super) fn recovery_help_section(&mut self, ui: &mut egui::Ui) {
        let id = egui::Id::new("recovery-help-problem");
        let chosen = ui.data_mut(|data| data.get_temp::<Problem>(id));
        let drawn = content(ui, chosen);
        ui.data_mut(|data| match drawn.chosen {
            Some(problem) => data.insert_temp(id, problem),
            None => {
                data.remove::<Problem>(id);
            }
        });
        match drawn.action {
            Some(Action::OpenAccount) => self.settings_tab = SettingsTab::Account,
            Some(Action::OpenEncryption) => {
                self.settings_tab = SettingsTab::Encryption;
                // The same refresh the settings navigation sends for this page.
                if self.session.is_some() {
                    self.send(Command::CryptoRefresh);
                }
            }
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENCRYPTION: &str = include_str!("encryption_ui.rs");
    const BACKUP: &str = include_str!("history_backup_ui.rs");
    const ACCOUNT: &str = include_str!("central_account_ui.rs");
    const AUTHENTICATOR: &str = include_str!("authenticator_ui.rs");

    fn all_text() -> Vec<&'static str> {
        Problem::ALL
            .iter()
            .flat_map(|p| p.steps().iter().copied())
            .chain(LIMITS)
            .collect()
    }

    #[test]
    fn each_guide_is_short_and_the_three_choices_are_distinct() {
        let labels: Vec<_> = Problem::ALL.iter().map(|p| p.label()).collect();
        assert_eq!(
            labels,
            [
                "Sign-in trouble",
                "New or lost device",
                "Missing message history"
            ]
        );
        for problem in Problem::ALL {
            assert!((3..=4).contains(&problem.steps().len()), "{problem:?}");
        }
        assert_eq!(Problem::SignIn.action(), Action::OpenAccount);
        assert_eq!(Problem::Device.action(), Action::OpenEncryption);
        assert_eq!(Problem::History.action(), Action::OpenEncryption);
    }

    #[test]
    fn the_limits_state_the_three_required_facts() {
        let all = LIMITS.join(" ");
        assert!(all.contains("password does not restore old device keys or message history"));
        assert!(all.contains("may need to approve a new device"));
        assert!(
            all.contains("server backup cannot by itself recreate keys held only by a lost device")
        );
    }

    #[test]
    fn screens_and_buttons_named_in_the_steps_exist_in_the_app() {
        // Every quoted name below must be drawn by the file that owns it.
        for name in [
            "Your security code",
            "Approve Device",
            "Revoke This Device",
            "Your registered devices",
            "\"Advanced\"",
            "Backup and recovery",
        ] {
            assert!(ENCRYPTION.contains(name), "{name}");
        }
        assert!(BACKUP.contains("History backup"));
        assert!(BACKUP.contains("does not confirm a new device or let it send messages"));
        assert!(ACCOUNT.contains("Manage password, authenticator and recovery"));
        assert!(AUTHENTICATOR.contains("6-digit code or recovery code"));
        // And the steps actually use those names.
        let device = Problem::Device.steps().join(" ");
        for name in [
            "Your security code",
            "Approve Device",
            "Revoke This Device",
            "Your registered devices",
            "Advanced",
        ] {
            assert!(device.contains(name), "{name}");
        }
        let history = Problem::History.steps().join(" ");
        assert!(history.contains("Backup and recovery"));
        assert!(Problem::SignIn.steps().join(" ").contains("6-digit code"));
        // The extra pages the buttons open are the real settings tabs.
        assert_eq!(Action::OpenAccount.label(), "Open Account");
        assert_eq!(Action::OpenEncryption.label(), "Open Encryption");
    }

    #[test]
    fn nothing_promises_a_recovery_the_app_cannot_do() {
        for text in all_text() {
            let lower = text.to_lowercase();
            for promise in [
                "automatic",
                "we will",
                "we can",
                "restores your",
                "will restore",
                "recover your keys",
                "export",
                "upload",
                "browser",
                "reset your password",
                "iroh",
            ] {
                assert!(!lower.contains(promise), "{promise}: {text}");
            }
            assert!(text.len() < 400, "too long to scan: {text}");
        }
        // Reset This Encryption Device is not offered as a recovery.
        assert!(!all_text()
            .iter()
            .any(|t| t.contains("Reset This Encryption Device")));
        // A backup is described honestly: it reads saved messages only.
        assert!(Problem::History
            .steps()
            .iter()
            .any(|s| s.contains("does not restore live chat")));
    }

    fn frame(ctx: &egui::Context, chosen: Option<Problem>, events: Vec<egui::Event>) -> Drawn {
        let mut result = None;
        let _ = ctx.run(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(700.0, 1600.0))),
                events,
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    result = Some(content(ui, chosen));
                });
            },
        );
        result.expect("frame ran")
    }

    fn click(at: Pos2) -> Vec<egui::Event> {
        let button = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        vec![egui::Event::PointerMoved(at), button(true), button(false)]
    }

    #[test]
    fn rendering_never_acts_and_a_click_does_only_what_its_button_says() {
        let ctx = egui::Context::default();
        configure(&ctx);
        // Nothing chosen: three choices, no steps, no action button.
        let mut page = frame(&ctx, None, Vec::new());
        for _ in 0..2 {
            page = frame(&ctx, None, Vec::new());
        }
        assert_eq!(page.chosen, None);
        assert_eq!(page.choosers.len(), 3);
        assert!(page.button.is_none() && page.action.is_none());
        // Choosing selects but does not act.
        for (problem, rect) in page.choosers.clone() {
            let picked = frame(&ctx, None, click(rect.center()));
            assert_eq!(picked.chosen, Some(problem));
            assert_eq!(picked.action, None);
            frame(&ctx, None, Vec::new());
        }
        // With a guide shown, rendering alone asks for nothing; the click asks for its own action.
        for problem in Problem::ALL {
            let mut shown = frame(&ctx, Some(problem), Vec::new());
            for _ in 0..2 {
                shown = frame(&ctx, Some(problem), Vec::new());
                assert_eq!(shown.action, None);
                assert_eq!(shown.chosen, Some(problem));
            }
            let rect = shown.button.expect("guide has its action button");
            let clicked = frame(&ctx, Some(problem), click(rect.center()));
            assert_eq!(clicked.action, Some(problem.action()));
            frame(&ctx, Some(problem), Vec::new());
        }
    }
}
