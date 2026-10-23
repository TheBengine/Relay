//! R3.4: explain, above the composer, why a protected message is waiting.
//!
//! The state comes only from `Update::SendWaiting`, which the network worker
//! emits when the encryption code itself decided to wait (`SendWait`). Nothing
//! here reads error text. A wait is bound to the connection (`CommandOrigin`:
//! address, server and account) and the conversation it was reported for. A
//! later result for that same conversation and connection replaces it: a
//! delivered message, a real failure or a rate limit. The draft, the send rules
//! and the trust model are unchanged: nothing is sent, approved or copied here.
use super::*;
use crate::network::{CommandOrigin, SendWait};

/// The conversation a wait belongs to, on one connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ReadinessKey {
    pub origin: CommandOrigin,
    pub target: Target,
}

const REMEMBERED_WAITS: usize = 32;

#[derive(Default)]
pub(super) struct ConversationReadiness {
    waits: Vec<(ReadinessKey, SendWait)>,
}

impl ConversationReadiness {
    /// Records a typed wait for its own connection. Returns true only when that
    /// connection is the active one, so only then may the caller change the
    /// active send queue (pending sends and outgoing rows).
    pub(super) fn wait_reported(
        &mut self,
        active: Option<&CommandOrigin>,
        key: ReadinessKey,
        wait: SendWait,
    ) -> bool {
        let is_active = active == Some(&key.origin);
        self.waiting(key, wait);
        is_active
    }

    /// A rate limit is the newest result for its conversation; without the
    /// connection it ran on, nothing can be settled safely.
    pub(super) fn rate_limited(&mut self, origin: Option<CommandOrigin>, target: &Target) {
        if let Some(origin) = origin {
            self.settled(&ReadinessKey {
                origin,
                target: target.clone(),
            });
        }
    }

    fn waiting(&mut self, key: ReadinessKey, wait: SendWait) {
        self.waits.retain(|(known, _)| known != &key);
        self.waits.push((key, wait));
        if self.waits.len() > REMEMBERED_WAITS {
            self.waits.remove(0);
        }
    }

    /// A message was delivered, or a real error was reported, in exactly this
    /// conversation: the wait no longer describes it.
    pub(super) fn settled(&mut self, key: &ReadinessKey) {
        self.waits.retain(|(known, _)| known != key);
    }

    pub(super) fn current(&self, key: &ReadinessKey) -> Option<SendWait> {
        self.waits
            .iter()
            .find(|(known, _)| known == key)
            .map(|(_, wait)| *wait)
    }
}

/// One short explanation and the next step. No names: the worker does not
/// report who the key manager is, so the text stays honest and general.
pub(super) fn wait_text(wait: SendWait) -> (&'static str, &'static str) {
    match wait {
        SendWait::DeviceApproval => (
            "Your device is waiting to be confirmed for this conversation.",
            "Ask the person who manages its keys to confirm your device in Encryption. Your message is kept; send it again afterwards.",
        ),
        SendWait::AddingDevices => (
            "This conversation is still adding your confirmed devices.",
            "The person who manages its keys may need to be online. Your message is kept; send it again in a moment.",
        ),
        SendWait::KeyRotation => (
            "A device left this conversation, so its keys are being replaced.",
            "The person who manages its keys needs to be online to finish. Your message is kept; send it again afterwards.",
        ),
        SendWait::KeyUpdates => (
            "This conversation has key updates waiting.",
            "Open the conversation again to load them, then send. Your message is kept.",
        ),
    }
}

/// Draws the banner and returns true when Open Encryption was clicked.
pub(super) fn readiness_banner(ui: &mut egui::Ui, wait: SendWait) -> bool {
    let (headline, next) = wait_text(wait);
    let mut open = false;
    egui::Frame::none()
        .fill(c_panel())
        .stroke(Stroke::new(1.0_f32, c_line()))
        .rounding(8.0)
        .inner_margin(Margin::symmetric(12.0, 10.0))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.add(egui::Label::new(RichText::new(headline).strong()).wrap(true));
            ui.add(egui::Label::new(RichText::new(next).color(c_muted())).wrap(true));
            ui.add_space(6.0);
            open = ui.add(secondary("Open Encryption")).clicked();
        });
    ui.add_space(8.0);
    open
}

impl RelayApp {
    pub(super) fn readiness_key(&self, target: &Target) -> Option<ReadinessKey> {
        Some(ReadinessKey {
            origin: self.command_origin()?,
            target: target.clone(),
        })
    }

    /// Hook, above the composer of the open conversation.
    pub(super) fn composer_readiness(&mut self, ui: &mut egui::Ui) {
        let Some(wait) = self
            .current
            .as_ref()
            .and_then(|target| self.readiness_key(target))
            .and_then(|key| self.readiness.current(&key))
        else {
            return;
        };
        if readiness_banner(ui, wait) {
            self.settings_tab = SettingsTab::Encryption;
            self.settings_open = true;
            self.send(Command::CryptoRefresh);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin(server: &str, account: &str) -> CommandOrigin {
        CommandOrigin {
            url: format!("http://{server}.test:4580"),
            server_id: server.into(),
            user_id: account.into(),
        }
    }

    fn key(server: &str, account: &str, target: Target) -> ReadinessKey {
        ReadinessKey {
            origin: origin(server, account),
            target,
        }
    }

    fn general() -> Target {
        Target::Channel("general".into())
    }

    #[test]
    fn a_wait_belongs_to_one_server_account_and_conversation() {
        let mut readiness = ConversationReadiness::default();
        let here = key("server-a", "maria", general());
        readiness.waiting(here.clone(), SendWait::DeviceApproval);
        assert_eq!(readiness.current(&here), Some(SendWait::DeviceApproval));
        for other in [
            key("server-b", "maria", general()),
            key("server-a", "maria-second-account", general()),
            key("server-a", "maria", Target::Channel("random".into())),
            key("server-a", "maria", Target::Direct("general".into())),
        ] {
            assert_eq!(readiness.current(&other), None, "{other:?}");
        }
    }

    #[test]
    fn only_the_same_conversation_settles_a_wait() {
        let mut readiness = ConversationReadiness::default();
        let here = key("server-a", "maria", general());
        let elsewhere = key("server-a", "maria", Target::Channel("random".into()));
        readiness.waiting(here.clone(), SendWait::AddingDevices);
        readiness.waiting(elsewhere.clone(), SendWait::KeyRotation);
        // A delivered message in another conversation is unrelated.
        readiness.settled(&elsewhere);
        assert_eq!(readiness.current(&here), Some(SendWait::AddingDevices));
        // A newer state for the same conversation replaces the older one.
        readiness.waiting(here.clone(), SendWait::KeyUpdates);
        assert_eq!(readiness.current(&here), Some(SendWait::KeyUpdates));
        readiness.settled(&here);
        assert_eq!(readiness.current(&here), None);
    }

    #[test]
    fn a_rate_limit_for_the_same_conversation_supersedes_the_wait() {
        let mut readiness = ConversationReadiness::default();
        let here = key("server-a", "maria", general());
        let elsewhere = key("server-a", "maria", Target::Channel("random".into()));
        readiness.waiting(here.clone(), SendWait::DeviceApproval);
        readiness.waiting(elsewhere.clone(), SendWait::KeyRotation);
        // A foreign connection's rate limit for the same target settles nothing.
        readiness.rate_limited(Some(origin("server-b", "maria")), &general());
        readiness.rate_limited(Some(origin("server-a", "other")), &general());
        // Without the connection it ran on, nothing is settled.
        readiness.rate_limited(None, &general());
        assert_eq!(readiness.current(&here), Some(SendWait::DeviceApproval));
        // Keys became ready and the retry was rate-limited: the wait is stale.
        readiness.rate_limited(Some(origin("server-a", "maria")), &general());
        assert_eq!(readiness.current(&here), None);
        assert_eq!(readiness.current(&elsewhere), Some(SendWait::KeyRotation));
    }

    #[test]
    fn only_a_wait_from_the_active_connection_may_touch_the_send_queue() {
        let mut readiness = ConversationReadiness::default();
        let active = origin("server-a", "maria");
        // An old server or account reports a wait for the same target id.
        for old in [
            origin("server-b", "maria"),
            origin("server-a", "old-account"),
        ] {
            let touch = readiness.wait_reported(
                Some(&active),
                ReadinessKey {
                    origin: old.clone(),
                    target: general(),
                },
                SendWait::KeyUpdates,
            );
            assert!(!touch, "{old:?} must not change the active send queue");
            assert_eq!(
                readiness.current(&ReadinessKey {
                    origin: active.clone(),
                    target: general()
                }),
                None,
                "and it is not shown for the active connection"
            );
        }
        // Signed out: no active connection, so nothing active is touched.
        assert!(!readiness.wait_reported(
            None,
            key("server-a", "maria", general()),
            SendWait::KeyUpdates
        ));
        assert!(readiness.wait_reported(
            Some(&active),
            key("server-a", "maria", general()),
            SendWait::DeviceApproval
        ));
    }

    #[test]
    fn remembered_waits_are_bounded() {
        let mut readiness = ConversationReadiness::default();
        for n in 0..(REMEMBERED_WAITS + 5) {
            let target = Target::Channel(format!("c{n}"));
            readiness.waiting(key("s", "a", target), SendWait::KeyUpdates);
        }
        assert_eq!(readiness.waits.len(), REMEMBERED_WAITS);
        assert_eq!(
            readiness.current(&key("s", "a", Target::Channel("c0".into()))),
            None
        );
    }

    /// The real composer shape: a banner, then the draft editor below it.
    fn frame(
        ctx: &egui::Context,
        size: Vec2,
        wait: Option<SendWait>,
        draft: &mut String,
        events: Vec<egui::Event>,
    ) -> (Option<Rect>, Rect, bool, Rect) {
        let mut result = (None, Rect::NOTHING, false, Rect::NOTHING);
        let _ = ctx.run(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size)),
                events,
                ..Default::default()
            },
            |ctx| {
                egui::TopBottomPanel::bottom("composer-test").show(ctx, |ui| {
                    let top = ui.cursor().top();
                    let clicked = wait.is_some_and(|wait| readiness_banner(ui, wait));
                    let banner = wait.map(|_| {
                        Rect::from_x_y_ranges(ui.max_rect().x_range(), top..=ui.cursor().top())
                    });
                    let editor = ui.add(egui::TextEdit::multiline(draft)).rect;
                    result = (banner, editor, clicked, ui.clip_rect());
                });
            },
        );
        result
    }

    #[test]
    fn banner_sits_above_the_kept_draft_in_small_windows_and_large_text() {
        for (size, text) in [
            (Vec2::new(940.0, 650.0), 1.0),
            (Vec2::new(320.0, 480.0), 1.4),
        ] {
            UI_FONT_SCALE.with(|scale| scale.set(text));
            for wait in [
                SendWait::DeviceApproval,
                SendWait::AddingDevices,
                SendWait::KeyRotation,
                SendWait::KeyUpdates,
            ] {
                let ctx = egui::Context::default();
                configure(&ctx);
                let mut draft = String::from("my unsent message");
                let mut drawn = frame(&ctx, size, Some(wait), &mut draft, Vec::new());
                for _ in 0..2 {
                    drawn = frame(&ctx, size, Some(wait), &mut draft, Vec::new());
                }
                let (banner, editor, clicked, clip) = drawn;
                let banner = banner.expect("banner drawn");
                assert!(!clicked, "rendering alone never opens anything");
                assert!(banner.bottom() <= editor.top() + 0.5, "{size:?}");
                assert!(banner.right() <= clip.right() + 0.5, "{size:?}");
                assert_eq!(draft, "my unsent message", "the draft is kept");
            }
            UI_FONT_SCALE.with(|scale| scale.set(1.0));
        }
    }

    #[test]
    fn open_encryption_needs_a_click_and_no_wait_draws_nothing() {
        let ctx = egui::Context::default();
        configure(&ctx);
        let size = Vec2::new(700.0, 500.0);
        let mut draft = String::new();
        let (banner, editor, clicked, _) = frame(&ctx, size, None, &mut draft, Vec::new());
        assert!(banner.is_none() && !clicked && editor.height() > 0.0);
        frame(
            &ctx,
            size,
            Some(SendWait::DeviceApproval),
            &mut draft,
            Vec::new(),
        );
        let (banner, ..) = frame(
            &ctx,
            size,
            Some(SendWait::DeviceApproval),
            &mut draft,
            Vec::new(),
        );
        // The button is the last row of the banner.
        let at = Pos2::new(
            banner.unwrap().left() + 70.0,
            banner.unwrap().bottom() - 26.0,
        );
        let press = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        let (_, _, clicked, _) = frame(
            &ctx,
            size,
            Some(SendWait::DeviceApproval),
            &mut draft,
            vec![egui::Event::PointerMoved(at), press(true), press(false)],
        );
        assert!(clicked);
    }
}
