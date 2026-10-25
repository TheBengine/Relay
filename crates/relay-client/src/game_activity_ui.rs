//! Opt-in game activity: the control in Privacy & Safety, the "Playing" line
//! friends see, and the confirmed Join step.
//!
//! Nothing here runs unless the person turned the switch on. A Join button
//! never opens anything by itself: a click only asks, and a second click on
//! Open hands one strictly validated Steam lobby link to the system.
use super::*;
use crate::appearance::{TEXT_BODY, TEXT_CAPTION};
use crate::game_activity::{self as activity, Action};
use crate::settings_ui as controls;
use relay_protocol::game_activity as model;

/// Where a list row leaves the Join button the person pressed, for the next frame.
pub(super) const JOIN_REQUEST: &str = "game-activity-join-request";

#[derive(Default)]
pub(super) struct Ui {
    core: activity::State,
    ask: Option<Ask>,
    error: Option<String>,
    account: Option<(String, String)>,
    service: Option<CommandOrigin>,
    visible: bool,
    pending: Option<Pending>,
    might_be_shared: bool,
    retry_after: Option<Instant>,
}

struct Pending {
    origin: CommandOrigin,
    request_id: u64,
    action: Action,
}

impl Ui {
    fn decide(&self, settings: &activity::Settings, connected: bool, now: Instant) -> Action {
        let action = self.core.decide(settings, connected, true, now);
        if connected
            && action == Action::Nothing
            && self.might_be_shared
            && (!settings.enabled || self.core.selected.is_none())
        {
            Action::Withdraw
        } else {
            action
        }
    }

    fn complete(&mut self, result: Result<Option<model::GameActivity>, String>, now: Instant) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        match result {
            Ok(_) => {
                self.core.sent(&pending.action, now);
                self.might_be_shared = pending.action != Action::Withdraw;
                self.retry_after = None;
                self.error = None;
            }
            Err(error) => {
                self.error = Some(error);
                self.retry_after = Some(now + Duration::from_secs(5));
            }
        }
    }
}

#[derive(Clone)]
struct Ask {
    origin: CommandOrigin,
    friend: String,
    game_id: String,
    link: String,
}

impl RelayApp {
    fn sync_game_account(&mut self) {
        let current = self
            .central_account
            .account
            .as_ref()
            .map(|a| (a.issuer.clone(), a.handle.clone()));
        if self.game_ui.account != current {
            self.game_ui = Ui {
                account: current,
                ..Default::default()
            };
        }
    }

    /// Remember only the configured Friends service, bound to this Relay
    /// account. Ordinary communities cannot nominate an activity destination.
    pub(super) fn remember_game_service(&mut self, url: &str, b: &Bootstrap) {
        self.sync_game_account();
        if self.people_url().as_deref() != Some(url) {
            return;
        }
        let linked = self.central_account.account.as_ref().is_some_and(|a| {
            b.linked_identities
                .iter()
                .any(|l| l.user_id == b.user.id && l.issuer == a.issuer && l.handle == a.handle)
        });
        if !linked
            || !b.capabilities.iter().any(|c| c == "people-v1")
            || !b.capabilities.iter().any(|c| c == model::CAPABILITY)
        {
            self.game_ui.service = None;
            return;
        }
        let origin = CommandOrigin {
            url: url.into(),
            server_id: b.server_id.clone(),
            user_id: b.user.id.clone(),
        };
        if self
            .game_ui
            .service
            .as_ref()
            .is_some_and(|old| *old != origin)
        {
            let account = self.game_ui.account.clone();
            self.game_ui = Ui {
                account,
                ..Default::default()
            };
        }
        self.game_ui.service = Some(origin);
        self.game_ui.visible = b.presence.iter().any(|p| {
            p.user_id == b.user.id && matches!(p.status.as_str(), "online" | "away" | "dnd")
        });
    }

    fn game_service_origin(&self) -> Option<CommandOrigin> {
        let origin = self.game_ui.service.as_ref()?;
        if self.people_url().as_deref() != Some(origin.url.as_str()) { return None; }
        let active = self.command_origin().as_ref() == Some(origin)
            && self.connection_status == ConnectionStatus::Connected;
        let background = self.communities.iter().any(|c| {
            c.url == origin.url && c.server_id == origin.server_id && c.user.id == origin.user_id
        }) && self.community_statuses.get(&origin.url)
            == Some(&ConnectionStatus::Connected);
        (active || background).then(|| origin.clone())
    }

    /// The second click uses the Friends service's advancing server clock,
    /// including expiry, rather than trusting an old cached presence forever.
    fn friend_invitation(&self, friend: &str, game_id: &str, link: &str) -> bool {
        if self.connection_status != ConnectionStatus::Connected {
            return false;
        }
        let Some(origin) = self.command_origin() else {
            return false;
        };
        let Some(clock) = self.call_alerts.clock(&origin) else {
            return false;
        };
        self.bootstrap.as_ref().is_some_and(|b| {
            b.capabilities.iter().any(|c| c == "people-v1")
                && invitation_current(b, friend, game_id, link, clock.unix_seconds)
        })
    }

    pub(super) fn game_activity_tick(&mut self, ctx: &egui::Context) {
        self.sync_game_account();
        if let Some(b) = self.bootstrap.clone() {
            self.remember_game_service(&self.login.url.clone(), &b);
        }
        let pressed = ctx.data_mut(|data| {
            data.remove_temp::<(String, String, String)>(egui::Id::new(JOIN_REQUEST))
        });
        if let Some((friend, game_id, link)) = pressed {
            if self.friend_invitation(&friend, &game_id, &link) {
                if let Some(origin) = self.command_origin() {
                    self.game_ui.ask = Some(Ask {
                        origin,
                        friend,
                        game_id,
                        link,
                    });
                }
            } else {
                self.notice = Some((
                    "That invitation is no longer available.".into(),
                    Instant::now(),
                ));
            }
        }
        let now = Instant::now();
        let mut settings = self.prefs.game_activity.clone();
        let service = self.game_service_origin();
        settings.enabled &= self.game_ui.visible && service.is_some();
        if settings.enabled {
            self.game_ui.core.collect();
            if self.game_ui.core.should_sample(&settings, now) {
                self.game_ui.core.start_sample(activity::running_names, now);
            }
        } else {
            self.game_ui.core.stop_sampling();
        }
        if service.is_none() {
            self.game_ui.core.disconnected();
            self.game_ui.pending = None;
        }
        if self.game_ui.pending.is_none() && self.game_ui.retry_after.is_none_or(|at| now >= at) {
            let action = self.game_ui.decide(&settings, service.is_some(), now);
            if action != Action::Nothing {
                let origin = service.unwrap();
                let share = match &action {
                    Action::Publish { game_id, join } => Some(model::Share {
                        game_id: game_id.clone(),
                        join: join.clone(),
                    }),
                    _ => None,
                };
                let request_id = self.next_request();
                match self.network.send(Command::SetGameActivity {
                    origin: origin.clone(),
                    share,
                    request_id,
                }) {
                    Ok(()) => {
                        self.game_ui.might_be_shared |= matches!(action, Action::Publish { .. });
                        self.game_ui.pending = Some(Pending {
                            origin,
                            request_id,
                            action,
                        })
                    }
                    Err(error) => {
                        self.game_ui.error = Some(error);
                        self.game_ui.retry_after = Some(now + Duration::from_secs(5));
                    }
                }
            }
        }
        if !self.prefs.game_activity.enabled
            && !self.game_ui.core.is_sharing()
            && !self.game_ui.might_be_shared
            && self.game_ui.pending.is_none()
        {
            self.game_ui.core.clear();
        }
        if self.prefs.game_activity.enabled
            || self.game_ui.pending.is_some()
            || self.game_ui.core.is_sharing()
            || self.game_ui.might_be_shared
        {
            ctx.request_repaint_after(Duration::from_secs(2));
        }
        self.game_join_dialog(ctx);
    }

    /// A queued write is not a successful publication or withdrawal. Keep the
    /// last confirmed state and retry a failed stop even with sharing switched off.
    pub(super) fn game_activity_result(
        &mut self,
        origin: CommandOrigin,
        request_id: u64,
        result: Result<Option<model::GameActivity>, String>,
    ) {
        self.sync_game_account();
        if self
            .game_ui
            .pending
            .as_ref()
            .is_none_or(|p| p.origin != origin || p.request_id != request_id)
            || self.game_service_origin().as_ref() != Some(&origin)
        {
            return;
        }
        self.game_ui.complete(result, Instant::now());
    }

    fn game_join_dialog(&mut self, ctx: &egui::Context) {
        let Some(ask) = self.game_ui.ask.clone() else {
            return;
        };
        let Some(game) = model::find(&ask.game_id) else {
            self.game_ui.ask = None;
            return;
        };
        let name = self
            .bootstrap
            .as_ref()
            .and_then(|b| b.members.iter().find(|m| m.id == ask.friend))
            .map(|m| profile_ui::display_name(m).to_owned())
            .unwrap_or_else(|| "A friend".into());
        let (mut open, mut go) = (true, false);
        let window = desktop_ui::dialog(ctx, "Join game")
            .id(egui::Id::new("game-activity-join"))
            .open(&mut open)
            .default_width(420.0)
            .collapsible(false);
        let mut cancel = false;
        if host_setup_ui::show_with_header(ctx, window, "Join game", |ui| {
            ui.label(format!(
                "{name} shared a Steam invitation for {}.",
                game.name
            ));
            ui.label(
                RichText::new("Relay opens it only if you press Open. Your computer then hands this link to Steam, which may start the game. Relay starts nothing else.")
                    .size(font_size(TEXT_BODY))
                    .color(c_muted()),
            );
            ui.label(
                RichText::new(&ask.link)
                    .size(font_size(TEXT_CAPTION))
                    .color(c_muted()),
            );
            ui.horizontal(|ui| {
                cancel = ui.add(secondary("Cancel")).clicked();
                go = ui.add(primary("Open")).clicked();
            });
        }) {
            open = false;
        }
        if go {
            // Checked again against what the service shows right now.
            if self.command_origin().as_ref() == Some(&ask.origin)
                && self.friend_invitation(&ask.friend, &ask.game_id, &ask.link)
            {
                ctx.open_url(egui::OpenUrl::new_tab(&ask.link));
            } else {
                self.notice = Some((
                    "That invitation is no longer available.".into(),
                    Instant::now(),
                ));
            }
            open = false;
        }
        if !open || cancel {
            self.game_ui.ask = None;
        }
    }

    /// The setting, in Privacy & Safety.
    pub(super) fn game_activity_settings(&mut self, ui: &mut egui::Ui) {
        controls::section_heading(
            ui,
            "Game activity",
            "Let your friends see which game you are playing. Off unless you turn it on.",
        );
        let before = self.prefs.game_activity.clone();
        controls::switch_row(
            ui,
            "game-activity",
            "Show the game I'm playing to my friends",
            "Relay compares the names of running programs with a short built-in list of games every 15 seconds. Only the game you choose is shared, only with your accepted friends on the Friends service, and only while it runs. Program lists, file locations and window titles never leave this PC. It stops when you turn this off, go invisible or close Relay (it also lapses about two minutes after it was last renewed).",
            &mut self.prefs.game_activity.enabled,
        );
        if !self.prefs.game_activity.enabled {
            if self.game_ui.might_be_shared || self.game_ui.pending.is_some() {
                controls::description(ui, "Nothing is checked. Stopping the shared status; waiting for Friends to confirm.");
            } else {
                controls::description(ui, "Off: nothing is checked. Any last status expires within two minutes if Friends cannot be reached.");
            }
            if let Some(error) = &self.game_ui.error {
                ui.label(
                    RichText::new(format!(
                        "Stopping was not confirmed: {error}. Relay will retry."
                    ))
                    .color(c_bad()),
                );
            }
            if before != self.prefs.game_activity {
                self.save_preferences();
            }
            return;
        }
        let connected = self.game_service_origin().is_some();
        if !connected {
            controls::description(
                ui,
                "Sign in to Friends to share (an older service may not support this). Once connected, you can keep any community selected.",
            );
        }
        if connected && !self.game_ui.visible {
            controls::description(ui, "Paused while you are invisible or offline on Friends.");
        }
        let running = self.game_ui.core.running.clone();
        if running.is_empty() {
            controls::description(ui, "No supported game is running.");
        } else {
            controls::description(ui, "Games Relay recognizes right now:");
            for id in running {
                if let Some(game) = model::find(id) {
                    let mut chosen = self.game_ui.core.selected.as_deref() == Some(id);
                    if ui.checkbox(&mut chosen, game.name).changed() {
                        self.game_ui.core.selected = chosen.then(|| id.to_owned());
                    }
                }
            }
        }
        if let Some(game) = self
            .game_ui
            .core
            .shared_game(Instant::now())
            .filter(|_| self.game_ui.visible)
            .and_then(model::find)
        {
            ui.label(
                RichText::new(format!("Friends can see: {}", game.name))
                    .size(font_size(TEXT_BODY))
                    .color(c_good()),
            );
        }
        if self.game_ui.might_be_shared && ui.add(secondary("Stop sharing now")).clicked() {
            self.prefs.game_activity.enabled = false;
        }
        controls::divider(ui);
        controls::switch_row(
            ui,
            "game-activity-join",
            "Show a Join button",
            "Relay cannot find your lobby by itself. Friends get a Join button only for a Steam lobby link you paste below, for the same game, and they must confirm before anything opens.",
            &mut self.prefs.game_activity.share_join,
        );
        if self.prefs.game_activity.share_join {
            ui.add(
                egui::TextEdit::singleline(&mut self.game_ui.core.join_text)
                    .hint_text("steam://joinlobby/<game>/<lobby>/<user>")
                    .char_limit(100)
                    .desired_width(f32::INFINITY),
            );
            let text = self.game_ui.core.join_text.trim().to_owned();
            if !text.is_empty() {
                let valid = self
                    .game_ui
                    .core
                    .selected
                    .as_deref()
                    .and_then(model::find)
                    .is_some_and(|game| model::join_link(game, &text).is_some());
                ui.label(
                    RichText::new(if valid {
                        "This link will be shown to your friends while the game runs."
                    } else {
                        "Not shared: use a Steam lobby link (steam://joinlobby/…) for the game you chose."
                    })
                    .size(font_size(TEXT_CAPTION))
                    .color(if valid { c_good() } else { c_bad() }),
                );
                if ui.add(secondary("Clear link")).clicked() {
                    self.game_ui.core.join_text.clear();
                }
            }
        }
        if let Some(error) = &self.game_ui.error {
            ui.label(
                RichText::new(error)
                    .size(font_size(TEXT_BODY))
                    .color(c_bad()),
            );
        }
        if before != self.prefs.game_activity {
            self.game_ui.error = None;
            self.save_preferences();
        }
    }
}

/// A friend's invitation is current only if the Friends service shows it to
/// this account right now: an accepted, unblocked friend who is online, playing
/// that game, with exactly this canonical link for that game.
fn invitation_current(b: &Bootstrap, friend: &str, game_id: &str, link: &str, now: u64) -> bool {
    let Some(game) = model::find(game_id) else {
        return false;
    };
    b.social.friends.iter().any(|id| id == friend)
        && !b.social.blocked.iter().any(|id| id == friend)
        && model::join_link(game, link).as_deref() == Some(link)
        && b.presence.iter().any(|p| {
            p.user_id == friend
                && matches!(p.status.as_str(), "online" | "away" | "dnd")
                && p.activity.as_ref().is_some_and(|a| {
                    a.expires_at > now && a.game_id == game_id && a.join.as_deref() == Some(link)
                })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINK: &str = "steam://joinlobby/730/1234567890/76561198000000000";

    fn friends_world() -> Bootstrap {
        let mut b = Bootstrap::default();
        b.social.friends.push("pal".into());
        b.presence.push(UserPresence {
            user_id: "pal".into(),
            status: "online".into(),
            activity: Some(model::GameActivity {
                game_id: "counter-strike-2".into(),
                name: "Counter-Strike 2".into(),
                join: Some(LINK.into()),
                expires_at: 200,
            }),
            ..Default::default()
        });
        b
    }

    #[test]
    fn a_join_opens_only_for_a_current_friends_exact_invitation() {
        let b = friends_world();
        assert!(invitation_current(&b, "pal", "counter-strike-2", LINK, 100));
        assert!(!invitation_current(
            &b,
            "pal",
            "counter-strike-2",
            LINK,
            200
        ));
        let mut invisible = b.clone();
        invisible.presence[0].status = "invisible".into();
        assert!(!invitation_current(
            &invisible,
            "pal",
            "counter-strike-2",
            LINK,
            100
        ));
        // Someone who is not a friend, or has been blocked, is never opened.
        assert!(!invitation_current(
            &b,
            "stranger",
            "counter-strike-2",
            LINK,
            100
        ));
        let mut blocked = b.clone();
        blocked.social.blocked.push("pal".into());
        assert!(!invitation_current(
            &blocked,
            "pal",
            "counter-strike-2",
            LINK,
            100
        ));
        // A link that is not what the service shows now, or not for that game.
        assert!(!invitation_current(
            &b,
            "pal",
            "counter-strike-2",
            "steam://joinlobby/730/9/9",
            100
        ));
        assert!(!invitation_current(&b, "pal", "dota-2", LINK, 100));
        assert!(!invitation_current(
            &b,
            "pal",
            "counter-strike-2",
            &format!(" {LINK}"),
            100
        ));
        for hostile in ["steam://run/730", "https://example.com", "calc.exe"] {
            let mut other = b.clone();
            other.presence[0].activity.as_mut().unwrap().join = Some(hostile.into());
            assert!(!invitation_current(
                &other,
                "pal",
                "counter-strike-2",
                hostile,
                100
            ));
        }
        // Gone offline, stopped playing, or no longer sharing a link.
        let mut offline = b.clone();
        offline.presence[0].status = "offline".into();
        assert!(!invitation_current(
            &offline,
            "pal",
            "counter-strike-2",
            LINK,
            100
        ));
        let mut stopped = b.clone();
        stopped.presence[0].activity = None;
        assert!(!invitation_current(
            &stopped,
            "pal",
            "counter-strike-2",
            LINK,
            100
        ));
        let mut no_link = b;
        no_link.presence[0].activity.as_mut().unwrap().join = None;
        assert!(!invitation_current(
            &no_link,
            "pal",
            "counter-strike-2",
            LINK,
            100
        ));
    }
    #[test]
    fn failed_stop_retains_confirmed_activity_and_retries_after_sending_is_disabled() {
        let now = Instant::now();
        let mut ui = Ui::default();
        let on = activity::Settings {
            enabled: true,
            share_join: false,
        };
        ui.core.accept(vec!["terraria"]);
        let publish = ui.core.decide(&on, true, true, now);
        ui.core.sent(&publish, now);
        ui.might_be_shared = true;
        let off = activity::Settings::default();
        ui.core.stop_sampling();
        assert!(ui.core.running.is_empty() && ui.core.join_text.is_empty());
        let action = ui.core.decide(&off, true, true, now);
        assert_eq!(action, Action::Withdraw);
        let origin = CommandOrigin {
            url: "https://accounts.invalid/people".into(),
            server_id: "friends".into(),
            user_id: "me".into(),
        };
        ui.pending = Some(Pending {
            origin: origin.clone(),
            request_id: 1,
            action: action.clone(),
        });
        ui.complete(Err("offline".into()), now);
        assert!(ui.core.is_sharing()); // service has not confirmed removal
        assert_eq!(ui.retry_after, Some(now + Duration::from_secs(5)));
        assert_eq!(
            ui.core
                .decide(&off, true, true, now + Duration::from_secs(5)),
            Action::Withdraw
        );
        ui.pending = Some(Pending {
            origin,
            request_id: 2,
            action,
        });
        ui.complete(Ok(None), now + Duration::from_secs(5));
        assert!(!ui.core.is_sharing());
        assert!(!ui.might_be_shared);
        assert!(ui.error.is_none() && ui.retry_after.is_none());
        assert_eq!(ui.core.decide(&off, true, true, now), Action::Nothing);
    }
    #[test]
    fn lost_first_publication_acknowledgement_still_requires_a_stop() {
        let now = Instant::now();
        let mut ui = Ui::default();
        let origin = CommandOrigin {
            url: "https://accounts.invalid/people".into(),
            server_id: "friends".into(),
            user_id: "me".into(),
        };
        ui.might_be_shared = true; // queued write may have reached Friends
        ui.pending = Some(Pending {
            origin,
            request_id: 9,
            action: Action::Publish {
                game_id: "terraria".into(),
                join: None,
            },
        });
        ui.complete(Err("reply lost".into()), now);
        assert!(!ui.core.is_sharing());
        assert!(ui.might_be_shared);
        assert_eq!(
            ui.decide(&activity::Settings::default(), true, now),
            Action::Withdraw
        );
        assert_eq!(
            ui.decide(&activity::Settings::default(), false, now),
            Action::Nothing
        );
    }
}
