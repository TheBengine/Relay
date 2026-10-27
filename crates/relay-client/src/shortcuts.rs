//! Keyboard shortcuts, listed once.
//!
//! The Keybinds page shows this table, and the voice shortcuts below are
//! handled from the same entries, so a listed shortcut cannot silently do
//! nothing. In-app shortcuts work while Relay's window is focused. Push to
//! talk and the F8 whisper key are read system-wide by the voice worker and are
//! listed with that scope.
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Action {
    /// Handled elsewhere (conversation list, search, pins, settings, composer).
    Existing,
    ToggleMute,
    ToggleDeafen,
    ToggleOverlay,
    ToggleLanProfile,
    NextSquad,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Scope {
    /// Only while the Relay window has focus.
    InApp,
    /// Read by the voice worker while you are in a call, even in a game.
    SystemWide,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Shortcut {
    pub action: Action,
    pub description: &'static str,
    pub keys: &'static str,
    pub scope: Scope,
    /// Only for shortcuts this module handles.
    pub binding: Option<(egui::Modifiers, egui::Key)>,
}

const CTRL_SHIFT: egui::Modifiers = egui::Modifiers {
    alt: false,
    ctrl: false,
    shift: true,
    mac_cmd: false,
    command: true,
};

const fn existing(description: &'static str, keys: &'static str) -> Shortcut {
    Shortcut { action: Action::Existing, description, keys, scope: Scope::InApp, binding: None }
}

const fn voice(action: Action, description: &'static str, keys: &'static str, key: egui::Key) -> Shortcut {
    Shortcut { action, description, keys, scope: Scope::InApp, binding: Some((CTRL_SHIFT, key)) }
}

pub(super) const ALL: [Shortcut; 15] = [
    existing("Find a conversation", "Ctrl + K"),
    existing("Search message history", "Ctrl + F"),
    existing("Open pinned messages", "Ctrl + P"),
    existing("Open settings", "Ctrl + ,"),
    existing("Show keyboard shortcuts", "Ctrl + /"),
    existing("Send a message", "Enter"),
    existing("Add a new line", "Shift + Enter"),
    existing("Close settings or a dialog", "Esc"),
    voice(Action::ToggleMute, "Mute or unmute your microphone", "Ctrl + Shift + M", egui::Key::M),
    voice(Action::ToggleDeafen, "Deafen or undeafen", "Ctrl + Shift + D", egui::Key::D),
    voice(Action::ToggleOverlay, "Show or hide the voice overlay", "Ctrl + Shift + O", egui::Key::O),
    voice(Action::ToggleLanProfile, "Turn the LAN profile on or off", "Ctrl + Shift + L", egui::Key::L),
    voice(Action::NextSquad, "Whisper to the next squad", "Ctrl + Shift + S", egui::Key::S),
    Shortcut {
        action: Action::Existing,
        description: "Push to talk (the key chosen above)",
        keys: "Your key",
        scope: Scope::SystemWide,
        binding: None,
    },
    Shortcut {
        action: Action::Existing,
        description: "Whisper to the chosen person or squad (when whisper is on)",
        keys: "Hold F8",
        scope: Scope::SystemWide,
        binding: None,
    },
];

pub(super) fn scope_label(scope: Scope) -> &'static str {
    match scope {
        Scope::InApp => "While Relay is focused",
        Scope::SystemWide => "Anywhere, while in a call",
    }
}

/// Which voice action, if any, was pressed this frame. Consumes the key.
pub(super) fn pressed(ctx: &egui::Context) -> Option<Action> {
    ALL.iter().find_map(|shortcut| {
        let (modifiers, key) = shortcut.binding?;
        ctx.input_mut(|input| input.consume_key(modifiers, key))
            .then_some(shortcut.action)
    })
}

impl RelayApp {
    /// Voice shortcuts. Ignored while a dialog is open; mute and deafen need a call.
    pub(super) fn handle_voice_shortcuts(&mut self, ctx: &egui::Context) {
        if self.session.is_none() || self.modal_open() {
            return;
        }
        let Some(action) = pressed(ctx) else {
            return;
        };
        match action {
            Action::ToggleMute if self.voice_channel.is_some() => self.toggle_mute(),
            Action::ToggleDeafen if self.voice_channel.is_some() => self.toggle_deafen(),
            Action::ToggleMute | Action::ToggleDeafen => {
                self.notice = Some(("Join a voice channel to use this shortcut".into(), Instant::now()));
            }
            Action::ToggleOverlay => {
                self.apply_quick_audio_change(|audio| audio.overlay_enabled = !audio.overlay_enabled);
                let on = self.prefs.audio.overlay_enabled;
                self.notice = Some((
                    if on { "Voice overlay on" } else { "Voice overlay off" }.into(),
                    Instant::now(),
                ));
            }
            Action::ToggleLanProfile => {
                let on = self.lan_profile_on();
                self.set_lan_profile(!on);
            }
            Action::NextSquad => self.whisper_next_squad(),
            Action::Existing => {}
        }
    }

    /// The Keybinds page list, drawn from `ALL`.
    pub(super) fn shortcut_list(&self, ui: &mut egui::Ui) {
        for shortcut in ALL {
            egui::Frame::none()
                .fill(c_panel())
                .rounding(4.0)
                .inner_margin(Margin::same(12.0))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.label(shortcut.description);
                            if shortcut.binding.is_some() || shortcut.scope == Scope::SystemWide {
                                ui.label(
                                    RichText::new(scope_label(shortcut.scope))
                                        .size(font_size(crate::appearance::TEXT_CAPTION))
                                        .color(c_muted()),
                                );
                            }
                        });
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.label(RichText::new(shortcut.keys).monospace().color(c_muted()));
                        });
                    });
                });
            ui.add_space(8.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_handled_shortcut_is_listed_once_and_bindings_do_not_collide() {
        let bound: Vec<_> = ALL.iter().filter_map(|s| s.binding).collect();
        for (i, a) in bound.iter().enumerate() {
            for b in &bound[i + 1..] {
                assert_ne!(a, b);
            }
        }
        for action in [
            Action::ToggleMute,
            Action::ToggleDeafen,
            Action::ToggleOverlay,
            Action::ToggleLanProfile,
            Action::NextSquad,
        ] {
            let entries: Vec<_> = ALL.iter().filter(|s| s.action == action).collect();
            assert_eq!(entries.len(), 1, "{action:?}");
            assert!(entries[0].binding.is_some());
            assert!(entries[0].keys.starts_with("Ctrl + Shift + "));
        }
        // The existing Ctrl shortcuts stay without Shift, so nothing is shadowed.
        for keys in ["Ctrl + K", "Ctrl + F", "Ctrl + P"] {
            assert!(ALL.iter().any(|s| s.keys == keys && s.binding.is_none()));
        }
    }

    #[test]
    fn a_ctrl_shift_press_is_found_and_consumed_once() {
        let ctx = egui::Context::default();
        // What Windows reports for Ctrl + Shift.
        let windows = egui::Modifiers { alt: false, ctrl: true, shift: true, mac_cmd: false, command: true };
        let press = |key| egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: windows,
        };
        let mut found = None;
        let _ = ctx.run(
            egui::RawInput { events: vec![press(egui::Key::M)], modifiers: windows, ..Default::default() },
            |ctx| {
                found = pressed(ctx);
                assert_eq!(pressed(ctx), None, "a key is consumed once");
            },
        );
        assert_eq!(found, Some(Action::ToggleMute));
        let plain = egui::Event::Key {
            key: egui::Key::M,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        let _ = ctx.run(egui::RawInput { events: vec![plain], ..Default::default() }, |ctx| {
            assert_eq!(pressed(ctx), None, "typing M must not mute");
        });
    }

    #[test]
    fn system_wide_keys_say_where_they_work() {
        assert!(ALL.iter().any(|s| s.keys == "Hold F8" && s.scope == Scope::SystemWide));
        assert_eq!(scope_label(Scope::InApp), "While Relay is focused");
    }
}
