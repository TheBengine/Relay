//! Following Windows audio device changes without touching saved choices.
//! A chosen device that is unplugged falls back to the Windows default until
//! it returns; "System default" follows Windows when the default changes.
use crate::audio_settings::AudioSettings;

/// What Windows reported since the last check.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Change {
    pub default_input: bool,
    pub default_output: bool,
    /// A device was added, removed, enabled or disabled.
    pub devices: bool,
}

impl Change {
    pub(crate) fn any(self) -> bool {
        self.default_input || self.default_output || self.devices
    }

    pub(crate) fn merge(&mut self, other: Change) {
        self.default_input |= other.default_input;
        self.default_output |= other.default_output;
        self.devices |= other.devices;
    }
}

/// Windows reports a default change per role and sometimes without a new
/// device; keep only defaults whose name really changed.
pub(crate) fn real_default_changes(
    change: Change,
    before: (&Option<String>, &Option<String>),
    after: (&Option<String>, &Option<String>),
) -> Change {
    Change {
        default_input: change.default_input && before.0 != after.0,
        default_output: change.default_output && before.1 != after.1,
        devices: change.devices,
    }
}

/// The device names to open: the saved choice when it is present, otherwise
/// the Windows default (None). The saved choice itself is never changed.
pub(crate) fn effective(
    saved: &AudioSettings,
    inputs: &[String],
    outputs: &[String],
) -> AudioSettings {
    let present = |choice: &Option<String>, names: &[String]| {
        choice.clone().filter(|name| names.iter().any(|known| known == name))
    };
    let mut settings = saved.clone();
    settings.input_device = present(&saved.input_device, inputs);
    settings.output_device = present(&saved.output_device, outputs);
    settings
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Follow {
    Nothing,
    /// Reopen only the audio devices; the call, mute, deafen and volumes stay.
    Reopen { notice: Option<String> },
}

/// Decide whether the running devices must be reopened. `running` is what the
/// audio engine currently opens (input, output).
pub(crate) fn decide(
    change: Change,
    saved: &AudioSettings,
    running: (&Option<String>, &Option<String>),
    inputs: &[String],
    outputs: &[String],
) -> Follow {
    if !change.any() {
        return Follow::Nothing;
    }
    let wanted = effective(saved, inputs, outputs);
    let input_moved = &wanted.input_device != running.0;
    let output_moved = &wanted.output_device != running.1;
    let default_moved = (change.default_input && wanted.input_device.is_none())
        || (change.default_output && wanted.output_device.is_none());
    if !(input_moved || output_moved || default_moved) {
        return Follow::Nothing;
    }
    let mut notices = Vec::new();
    for (saved, wanted, running, kind) in [
        (&saved.input_device, &wanted.input_device, running.0, "microphone"),
        (&saved.output_device, &wanted.output_device, running.1, "speaker or headset"),
    ] {
        if let Some(name) = saved {
            if wanted.is_none() && running.is_some() {
                notices.push(format!("“{name}” was unplugged; using the Windows default {kind} until it is back."));
            } else if wanted.is_some() && running.is_none() {
                notices.push(format!("“{name}” is back and in use again."));
            }
        }
    }
    if notices.is_empty() && default_moved {
        notices.push("Windows changed the default audio device; Relay switched with it.".into());
    }
    Follow::Reopen {
        notice: (!notices.is_empty()).then(|| notices.join(" ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| name.to_string()).collect()
    }

    fn saved(input: Option<&str>, output: Option<&str>) -> AudioSettings {
        AudioSettings {
            input_device: input.map(Into::into),
            output_device: output.map(Into::into),
            ..Default::default()
        }
    }

    #[test]
    fn effective_choice_falls_back_without_changing_the_saved_one() {
        let chosen = saved(Some("USB Headset"), Some("USB Headset"));
        let plugged = effective(&chosen, &names(&["USB Headset", "Laptop mic"]), &names(&["USB Headset"]));
        assert_eq!(plugged.input_device.as_deref(), Some("USB Headset"));
        let unplugged = effective(&chosen, &names(&["Laptop mic"]), &names(&["Speakers"]));
        assert_eq!((unplugged.input_device, unplugged.output_device), (None, None));
        assert_eq!(chosen.input_device.as_deref(), Some("USB Headset"), "saved choice untouched");
    }

    #[test]
    fn unplug_falls_back_and_replug_returns() {
        let chosen = saved(Some("USB Headset"), None);
        let usb = Some("USB Headset".to_string());
        let default = None;
        let unplug = decide(
            Change { devices: true, ..Default::default() },
            &chosen,
            (&usb, &default),
            &names(&["Laptop mic"]),
            &names(&["Speakers"]),
        );
        let Follow::Reopen { notice: Some(text) } = unplug else { panic!("{unplug:?}") };
        assert!(text.contains("unplugged") && text.contains("USB Headset"));
        let replug = decide(
            Change { devices: true, ..Default::default() },
            &chosen,
            (&default, &default),
            &names(&["Laptop mic", "USB Headset"]),
            &names(&["Speakers"]),
        );
        let Follow::Reopen { notice: Some(text) } = replug else { panic!("{replug:?}") };
        assert!(text.contains("is back"));
    }

    #[test]
    fn system_default_follows_windows_and_explicit_choices_ignore_it() {
        let default = None;
        let follow = decide(
            Change { default_output: true, ..Default::default() },
            &saved(None, None),
            (&default, &default),
            &names(&["Mic"]),
            &names(&["Speakers", "Headset"]),
        );
        assert!(matches!(follow, Follow::Reopen { notice: Some(ref text) } if text.contains("default")));
        let headset = Some("Headset".to_string());
        assert_eq!(
            decide(
                Change { default_output: true, ..Default::default() },
                &saved(None, Some("Headset")),
                (&default, &headset),
                &names(&["Mic"]),
                &names(&["Speakers", "Headset"]),
            ),
            Follow::Nothing,
            "an explicit, present output stays put when the Windows default moves"
        );
    }

    #[test]
    fn a_default_notification_without_a_new_default_is_ignored() {
        let speakers = Some("Speakers".to_string());
        let headset = Some("Headset".to_string());
        let mic = Some("Mic".to_string());
        let same = real_default_changes(
            Change { default_output: true, ..Default::default() },
            (&mic, &speakers),
            (&mic, &speakers),
        );
        assert!(!same.any());
        let moved = real_default_changes(
            Change { default_output: true, default_input: true, ..Default::default() },
            (&mic, &speakers),
            (&mic, &headset),
        );
        assert_eq!(moved, Change { default_output: true, ..Default::default() });
    }

    #[test]
    fn unplugging_input_and_output_together_names_both() {
        let chosen = saved(Some("USB Headset"), Some("USB Headset"));
        let usb = Some("USB Headset".to_string());
        let follow = decide(Change { devices: true, ..Default::default() }, &chosen, (&usb, &usb), &names(&["Mic"]), &names(&["Speakers"]));
        let Follow::Reopen { notice: Some(text) } = follow else { panic!("{follow:?}") };
        assert!(text.contains("microphone") && text.contains("speaker or headset"));
    }

    #[test]
    fn unrelated_changes_do_nothing() {
        let default = None;
        let chosen = saved(None, None);
        assert_eq!(decide(Change::default(), &chosen, (&default, &default), &[], &[]), Follow::Nothing);
        // Some other device was added; the defaults in use did not move.
        assert_eq!(
            decide(Change { devices: true, ..Default::default() }, &chosen, (&default, &default), &names(&["Mic", "New mic"]), &[]),
            Follow::Nothing
        );
        let mut change = Change::default();
        change.merge(Change { default_input: true, ..Default::default() });
        assert!(change.any() && change.default_input && !change.devices);
    }
}
