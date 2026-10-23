//! R6.4, help part: a short Help page for new users. Four topics with a few
//! steps each, longer explanations folded away, and buttons that open the
//! existing flows. Rendering never starts a server, connects or contacts the
//! network; only an explicit button click does what that button names.
use super::*;
use crate::settings_ui as controls;

/// What the viewer can do now; decides which steps and buttons apply.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct HelpViewer {
    pub signed_in: bool,
    pub can_invite: bool,
}

/// UI intentions the page can ask for. The app maps each to an existing flow.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum HelpAction {
    InviteFriends,
    JoinFriends,
    OpenEncryption,
    KeyboardShortcuts,
}

impl HelpAction {
    fn label(self) -> &'static str {
        match self {
            Self::InviteFriends => "Invite friends",
            Self::JoinFriends => "Join friends",
            Self::OpenEncryption => "Open Encryption",
            Self::KeyboardShortcuts => "Keyboard shortcuts",
        }
    }
}

struct Topic {
    title: &'static str,
    steps: Vec<&'static str>,
    more: &'static str,
    action: Option<HelpAction>,
}

fn topics(viewer: HelpViewer) -> [Topic; 4] {
    let host = if !viewer.signed_in {
        Topic {
            title: "Create a server and invite friends",
            steps: vec![
                "On the start screen, choose Create a server.",
                "Create your account for the new server.",
                "Relay then opens Invite friends so you can send an invitation.",
            ],
            more: "Your server runs on this PC, so keep Relay open while friends use it. The Invite friends guide explains how friends reach it and lets you copy the invitation.",
            action: None,
        }
    } else if viewer.can_invite {
        Topic {
            title: "Create a server and invite friends",
            steps: vec![
                "Choose Invite friends.",
                "Copy the invitation and send it in any chat.",
                "Your friend chooses Join friends in Relay and pastes it.",
            ],
            more: "A server you host runs on this PC, so keep Relay open while friends use it. The Invite friends guide explains how friends reach it.",
            action: Some(HelpAction::InviteFriends),
        }
    } else {
        Topic {
            title: "Create a server and invite friends",
            steps: vec![
                "This account cannot invite people to this server.",
                "Ask the host or an admin for an invitation to share.",
                "Friends you invite then choose Join friends in Relay and paste it.",
            ],
            more: "The host decides who may invite. Your own server runs on your PC while Relay is open.",
            action: None,
        }
    };
    let join = Topic {
        title: "Join a friend's server",
        steps: vec![
            "Ask your friend for an invitation. It starts with relay://join.",
            if viewer.signed_in {
                "Choose Join friends and paste the invitation."
            } else {
                "On the start screen, choose Join friends and paste the invitation."
            },
            "Relay checks the server, then you create your own account there.",
        ],
        more: "You do not need to start a server to join. If the invitation does not work, ask your friend for a new one.",
        action: viewer.signed_in.then_some(HelpAction::JoinFriends),
    };
    let encryption = Topic {
        title: "Encryption and confirming devices",
        steps: if viewer.signed_in {
            vec![
                "Open Encryption and choose Verify next to the person to see their devices.",
                "Get their security code or verification card through a contact you already trust, and paste it.",
                "When it matches their device, choose Approve Device.",
            ]
        } else {
            vec![
                "Private messages and calls are end-to-end encrypted.",
                "After you sign in, Encryption shows the devices to confirm.",
            ]
        },
        more: "Verify only opens the person's devices; nothing is approved until you choose Approve Device, and a name alone does not prove who someone is. When someone gets a new device, check the new security code with them again. An earlier approval does not cover a replacement device.",
        action: viewer.signed_in.then_some(HelpAction::OpenEncryption),
    };
    let shortcuts = Topic {
        title: "Keyboard shortcuts",
        steps: vec![
            "Ctrl + K finds a conversation.",
            "Ctrl + F searches message history.",
            "Esc closes settings or a dialog.",
        ],
        more: "Keybinds also lets you set push to talk.",
        action: Some(HelpAction::KeyboardShortcuts),
    };
    [host, join, encryption, shortcuts]
}

/// Draws the page and returns the clicked action, plus each button's rect.
pub(super) fn help_content(
    ui: &mut egui::Ui,
    viewer: HelpViewer,
) -> (Option<HelpAction>, Vec<(HelpAction, Rect)>) {
    controls::page_heading(
        ui,
        "Help",
        "Short answers to get you started. Open a topic for more detail.",
    );
    let mut clicked = None;
    let mut buttons = Vec::new();
    for (index, topic) in topics(viewer).into_iter().enumerate() {
        if index > 0 {
            controls::divider(ui);
        }
        controls::section_heading(ui, topic.title, "");
        for (step, text) in topic.steps.iter().enumerate() {
            ui.add(egui::Label::new(format!("{}. {text}", step + 1)).wrap(true));
            ui.add_space(4.0);
        }
        egui::CollapsingHeader::new("More")
            .id_source(("help-more", index))
            .show(ui, |ui| controls::description(ui, topic.more));
        if let Some(action) = topic.action {
            ui.add_space(8.0);
            let response = ui.add(secondary(action.label()));
            buttons.push((action, response.rect));
            if response.clicked() {
                clicked = Some(action);
            }
        }
    }
    (clicked, buttons)
}

impl RelayApp {
    fn help_viewer(&self) -> HelpViewer {
        let signed_in = self.session.is_some();
        HelpViewer {
            signed_in,
            can_invite: signed_in
                && self
                    .bootstrap
                    .as_ref()
                    .is_some_and(|b| b.has_permission(PERMISSION_INVITES)),
        }
    }

    pub(super) fn help_page(&mut self, ui: &mut egui::Ui) {
        let viewer = self.help_viewer();
        let (clicked, _) = help_content(ui, viewer);
        match clicked {
            Some(HelpAction::InviteFriends) if viewer.can_invite => {
                self.close_settings();
                // Same as "Invite people" in the server menu.
                if crate::local_host::is_local_url(&self.login.url) {
                    self.open_friend_sharing();
                } else if let Some(origin) = self.command_origin() {
                    self.send(Command::Invite {
                        origin,
                        request_id: None,
                    });
                }
            }
            Some(HelpAction::JoinFriends) if viewer.signed_in => {
                self.close_settings();
                self.open_community_dialog();
            }
            Some(HelpAction::OpenEncryption) if viewer.signed_in => {
                self.settings_tab = SettingsTab::Encryption;
                self.send(Command::CryptoRefresh);
            }
            Some(HelpAction::KeyboardShortcuts) => self.settings_tab = SettingsTab::Keybinds,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The buttons shown for this viewer, in page order.
    fn help_actions(viewer: HelpViewer) -> Vec<HelpAction> {
        topics(viewer)
            .iter()
            .filter_map(|topic| topic.action)
            .collect()
    }

    const SIGNED_OUT: HelpViewer = HelpViewer {
        signed_in: false,
        can_invite: false,
    };
    const MEMBER: HelpViewer = HelpViewer {
        signed_in: true,
        can_invite: false,
    };
    const HOST: HelpViewer = HelpViewer {
        signed_in: true,
        can_invite: true,
    };

    #[test]
    fn actions_match_what_the_viewer_can_do() {
        use HelpAction::*;
        assert_eq!(help_actions(SIGNED_OUT), [KeyboardShortcuts]);
        assert_eq!(
            help_actions(MEMBER),
            [JoinFriends, OpenEncryption, KeyboardShortcuts]
        );
        assert_eq!(
            help_actions(HOST),
            [
                InviteFriends,
                JoinFriends,
                OpenEncryption,
                KeyboardShortcuts
            ]
        );
    }

    #[test]
    fn every_topic_is_short_and_names_existing_buttons() {
        for viewer in [SIGNED_OUT, MEMBER, HOST] {
            for topic in topics(viewer) {
                assert!((2..=3).contains(&topic.steps.len()), "{}", topic.title);
                // No promises about features main does not have.
                for text in topic.steps.iter().chain([&topic.more]) {
                    let lower = text.to_lowercase();
                    for promise in ["iroh", "automatic", "update", "without a code", "qr"] {
                        assert!(!lower.contains(promise), "{text}");
                    }
                }
            }
        }
        // The buttons the steps tell people to choose are the app's labels.
        let host = topics(HOST);
        assert!(host[0].steps[0].contains(HelpAction::InviteFriends.label()));
        assert!(topics(MEMBER)[1].steps[1].contains(HelpAction::JoinFriends.label()));
        assert!(topics(SIGNED_OUT)[0].steps[0].contains("Create a server"));
        assert!(topics(MEMBER)[0].steps[1].contains("Ask the host"));
        // Approval finishes with Approve Device; Verify only opens the devices.
        let encryption = &topics(MEMBER)[2];
        assert!(encryption.steps.last().unwrap().contains("Approve Device"));
        assert!(encryption.steps.iter().any(|step| step.contains("trust")));
        assert!(encryption
            .more
            .contains("nothing is approved until you choose Approve Device"));
    }

    struct Frame {
        clicked: Option<HelpAction>,
        buttons: Vec<(HelpAction, Rect)>,
        content: Rect,
        clip: Rect,
    }

    fn frame(
        ctx: &egui::Context,
        size: Vec2,
        viewer: HelpViewer,
        events: Vec<egui::Event>,
    ) -> Frame {
        let mut result = None;
        let _ = ctx.run(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size)),
                events,
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        let (clicked, buttons) = help_content(ui, viewer);
                        result = Some(Frame {
                            clicked,
                            buttons,
                            content: ui.min_rect(),
                            clip: ui.clip_rect(),
                        });
                    });
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
    fn page_fits_small_windows_and_large_text_and_scrolls() {
        for (size, text) in [
            (Vec2::new(900.0, 700.0), 1.0),
            (Vec2::new(320.0, 480.0), 1.0),
            (Vec2::new(320.0, 480.0), 1.4),
        ] {
            let ctx = egui::Context::default();
            configure(&ctx);
            crate::appearance::AppearanceSettings {
                font_scale: text,
                ..Default::default()
            }
            .apply(&ctx, None);
            let mut last = None;
            for _ in 0..3 {
                last = Some(frame(&ctx, size, HOST, Vec::new()));
            }
            let page = last.unwrap();
            assert!(
                page.content.right() <= page.clip.right() + 0.5,
                "{size:?} x{text}: {:?} wider than {:?}",
                page.content,
                page.clip
            );
            assert_eq!(page.buttons.len(), 4);
            if size.y < 600.0 {
                // Long content scrolls instead of being cut off.
                assert!(page.content.bottom() > page.clip.bottom(), "{size:?}");
            }
        }
    }

    #[test]
    fn rendering_asks_for_nothing_and_a_click_asks_for_its_own_action() {
        for viewer in [SIGNED_OUT, MEMBER, HOST] {
            let ctx = egui::Context::default();
            configure(&ctx);
            let mut page = frame(&ctx, Vec2::new(900.0, 2000.0), viewer, Vec::new());
            for _ in 0..3 {
                page = frame(&ctx, Vec2::new(900.0, 2000.0), viewer, Vec::new());
                assert_eq!(page.clicked, None, "rendering alone never acts");
            }
            let shown: Vec<_> = page.buttons.iter().map(|(action, _)| *action).collect();
            assert_eq!(shown, help_actions(viewer));
            for (action, rect) in page.buttons.clone() {
                let clicked = frame(&ctx, Vec2::new(900.0, 2000.0), viewer, click(rect.center()));
                assert_eq!(clicked.clicked, Some(action));
                frame(&ctx, Vec2::new(900.0, 2000.0), viewer, Vec::new());
            }
        }
    }
}
