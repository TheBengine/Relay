//! Validated invitation ingress and one volatile UI-bound pending slot.
//! No network, profiles, preferences, registry, media or cryptographic approval.
use crate::invitation::Invitation;

const INVALID: &str =
    "This invitation could not be opened. Open Relay and paste it in Join friends.";

/// Only static explanations reach this UI, never a bearer link or raw argv.
pub fn failed(message: &'static str) -> ! {
    eprintln!("{message}");
    #[cfg(windows)]
    unsafe {
        use windows::{
            core::PCWSTR,
            Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONINFORMATION, MB_OK},
        };
        let text: Vec<u16> = message.encode_utf16().chain([0]).collect();
        let title: Vec<u16> = "Relay – Open invitation"
            .encode_utf16()
            .chain([0])
            .collect();
        let _ = MessageBoxW(
            None,
            PCWSTR(text.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_ICONINFORMATION | MB_OK,
        );
    }
    std::process::exit(2)
}

// An invitation now also carries its extra routes; it is boxed at the one place it is held.
#[allow(clippy::large_enum_variant)]
pub enum Intent {
    Ordinary,
    Invitation(Invitation),
}

#[derive(Default)]
pub struct Launch {
    pub initial: Option<Invitation>,
    #[cfg(windows)]
    pub inbox: Option<crate::single_instance::Inbox>,
    #[cfg(windows)]
    pub register_protocol: bool,
}

pub fn prefill(state: &mut crate::connection_setup::SetupState, invitation: Invitation) {
    *state = Default::default();
    state.invitation_text = invitation.link();
}

pub fn parse(args: &[String]) -> Result<Intent, &'static str> {
    if !args
        .iter()
        .skip(1)
        .any(|arg| arg.starts_with("--open-invitation"))
    {
        return Ok(Intent::Ordinary);
    }
    // A browser supplies one opaque capability, never arbitrary Relay flags.
    if args.len() != 3 || args[1] != "--open-invitation" {
        return Err(INVALID);
    }
    let value = &args[2];
    if value.len() > 4096 || value.chars().any(char::is_control) {
        return Err(INVALID);
    }
    let invitation = Invitation::parse(value).map_err(|_| INVALID)?;
    if invitation.link().len() > 4096 {
        return Err(INVALID);
    }
    Ok(Intent::Invitation(invitation))
}

/// Only personal default invocations share the main UI. Explicit QA/storage
/// scopes retain their existing isolation and never publish a URL handler.
pub fn personal(args: &[String], storage_override: bool) -> Result<bool, String> {
    let explicit = crate::profile_paths::requested_root(args)?.is_some();
    let qa = args.iter().skip(1).any(|arg| {
        arg.starts_with("--capture")
            || arg.starts_with("--native-render-probe")
            || arg.starts_with("--dev-profile-owner")
    });
    Ok(!explicit && !storage_override && !qa)
}

pub const STORAGE_OVERRIDES: &[&str] = &[
    "RELAY_PROFILE_DIR",
    "RELAY_CONFIG_DIR",
    "RELAY_CRYPTO_DIR",
    "RELAY_SERVER_DATA_DIR",
];

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Offer {
    Accepted,
    Busy,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Form<'a> {
    Empty,
    Invitation(&'a Invitation),
    Text(&'a str),
    Other,
}

impl Form<'_> {
    fn matches(self, invitation: &Invitation) -> bool {
        match self {
            Self::Invitation(current) => current == invitation,
            Self::Text(text) => Invitation::parse(text).is_ok_and(|current| current == *invitation),
            Self::Empty | Self::Other => false,
        }
    }

    fn blocks(self, invitation: &Invitation) -> bool {
        self != Self::Empty && !self.matches(invitation)
    }
}

#[derive(Default)]
pub struct Pending {
    invitation: Option<Invitation>,
}

impl Pending {
    pub fn initial(invitation: Option<Invitation>) -> Self {
        Self { invitation }
    }

    pub fn is_pending(&self) -> bool {
        self.invitation.is_some()
    }

    /// The caller supplies only the visible form. A different pending link or
    /// user-edited form is never overwritten; an updater cannot ACK a lost link.
    pub fn offer(&mut self, invitation: Invitation, updating: bool, form: Form<'_>) -> Offer {
        if updating || form.blocks(&invitation) {
            return Offer::Busy;
        }
        if let Some(current) = &self.invitation {
            return if current == &invitation {
                Offer::Accepted
            } else {
                Offer::Busy
            };
        }
        if !form.matches(&invitation) {
            self.invitation = Some(invitation);
        }
        Offer::Accepted
    }

    /// Deferred across authentication/resume; Connected may reset setup forms
    /// without clearing this separate slot. No repeated network/UI timer.
    pub fn take_ready(&mut self, busy: bool, updating: bool, form: Form<'_>) -> Option<Invitation> {
        let invitation = self.invitation.as_ref()?;
        if busy || updating || form.blocks(invitation) {
            return None;
        }
        let current = self.invitation.take();
        if form.matches(current.as_ref().unwrap()) {
            None
        } else {
            current
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const ID: &str = "4be44aef-8c98-4db6-a9d5-81c2eb311330";

    fn invitation(code: &str) -> Invitation {
        Invitation::new("http://100.70.1.2:4580", ID, code).unwrap()
    }

    fn arguments(values: &[&str]) -> Vec<String> {
        std::iter::once("relay.exe")
            .chain(values.iter().copied())
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn dedicated_mode_validates_both_versions_without_exposing_capabilities() {
        for invite in [
            invitation("private-bearer"),
            invitation("private-bearer")
                .with_network_share("https://login.tailscale.com/admin/invite/private-share")
                .unwrap(),
        ] {
            let link = invite.link();
            let Intent::Invitation(parsed) =
                parse(&arguments(&["--open-invitation", &link])).unwrap()
            else {
                panic!("Expected invitation")
            };
            assert!(parsed == invite);
            for values in [
                vec!["--open-invitation"],
                vec!["--open-invitation", &link, "--profile-dir", "C:\\private"],
                vec!["--profile-dir", "C:\\private", "--open-invitation", &link],
                vec!["--open-invitation", &link, "--open-invitation", &link],
                vec!["--open-invitation=private-bearer"],
                vec!["--open-invitation-extra", &link],
            ] {
                let error = parse(&arguments(&values)).err().unwrap();
                assert!(!error.contains("private-bearer") && !error.contains("private-share"));
            }
        }
        assert!(matches!(
            parse(&arguments(&["--settings", "voice"])),
            Ok(Intent::Ordinary)
        ));
    }

    #[test]
    fn ingress_rejects_controls_oversize_unexpected_flags_and_url_shapes() {
        let valid = invitation("secret").link();
        for link in [
            valid.replace("relay://", "https://"),
            valid.replace("//join?", "//other?"),
            valid.replace("//join?", "//join:9?"),
            valid.replace("//join?", "//user@join?"),
            valid.replace("//join?", "//join/path?"),
            format!("{valid}\n"),
            format!("{valid}\0"),
            format!("{valid}\t"),
            format!("{valid} --server http://other.example"),
            "x".repeat(4097),
        ] {
            assert!(parse(&arguments(&["--open-invitation", &link])).is_err());
        }
        // Exact bound is accepted; over the bound fails before URL parsing.
        let padding = 4096 - valid.len();
        let padded = format!("{}{}", " ".repeat(padding), valid);
        assert!(matches!(
            parse(&arguments(&["--open-invitation", &padded])),
            Ok(Intent::Invitation(_))
        ));
        assert!(parse(&arguments(&["--open-invitation", &(padded + " ")])).is_err());
    }

    #[test]
    fn storage_and_test_profiles_never_share_the_personal_instance() {
        assert!(personal(&arguments(&[]), false).unwrap());
        assert!(!personal(&arguments(&[]), true).unwrap());
        let path = std::env::temp_dir().join("relay-launch-profile");
        assert!(!personal(
            &arguments(&["--profile-dir", path.to_str().unwrap()]),
            false
        )
        .unwrap());
        for flag in [
            "--capture",
            "--capture-authenticated",
            "--native-render-probe",
            "--dev-profile-owner",
        ] {
            assert!(!personal(&arguments(&[flag]), false).unwrap());
        }
        for values in [
            vec!["--profile-dir"],
            vec!["--profile-dir=invalid"],
            vec!["--profile-dir", "relative"],
        ] {
            assert!(personal(&arguments(&values), false).is_err());
        }
        assert_eq!(STORAGE_OVERRIDES.len(), 4);
    }

    #[test]
    fn one_pending_slot_survives_resume_and_never_replaces_other_input() {
        let first = invitation("first");
        let second = invitation("second");
        let mut pending = Pending::initial(Some(first.clone()));
        assert!(pending.take_ready(true, false, Form::Empty).is_none());
        assert!(pending.is_pending());
        assert!(pending.offer(second.clone(), false, Form::Empty) == Offer::Busy);
        assert!(pending.offer(first.clone(), false, Form::Empty) == Offer::Accepted);
        assert!(pending.take_ready(false, true, Form::Empty).is_none());
        assert!(pending.take_ready(false, false, Form::Other).is_none());
        assert!(pending
            .take_ready(false, false, Form::Text("my edited form"))
            .is_none());
        assert!(pending.take_ready(false, false, Form::Empty).unwrap() == first);
        assert!(!pending.is_pending());
        assert!(pending.offer(second, true, Form::Empty) == Offer::Busy);
        assert!(!pending.is_pending());
    }

    #[test]
    fn same_invitation_does_not_reset_an_active_join_or_leak_into_another_form() {
        let invite = invitation("first");
        let mut pending = Pending::default();
        assert!(pending.offer(invite.clone(), false, Form::Invitation(&invite)) == Offer::Accepted);
        assert!(!pending.is_pending());
        assert!(
            pending.offer(invite.clone(), false, Form::Text(&invite.link())) == Offer::Accepted
        );
        assert!(!pending.is_pending());
        assert!(
            pending.offer(invite.clone(), false, Form::Text("different user input")) == Offer::Busy
        );
        assert!(pending.offer(invite, false, Form::Other) == Offer::Busy);
        assert!(!pending.is_pending());
    }

    #[test]
    fn prefill_requires_explicit_join_and_discards_previous_health_generation() {
        let old = invitation("old");
        let next = invitation("new");
        let mut state = crate::connection_setup::SetupState::default();
        state.invitation = Some(old.clone());
        state.private_connection_started = true;
        state.begin_check(&old.server, 19, false).unwrap();
        prefill(&mut state, next.clone());
        assert!(Invitation::parse(&state.invitation_text).unwrap() == next);
        assert!(state.invitation.is_none() && state.pending.is_none() && state.result.is_none());
        assert!(!state.private_connection_started && state.saved_server_id.is_none());
        state.accept(
            &old.server,
            &old.server,
            19,
            Ok(crate::connection_setup::ServerSummary {
                ok: true,
                server_name: "Old result".into(),
                server_id: old.server_id,
                protocol_version: relay_protocol::API_PROTOCOL_VERSION,
                rtc_enabled: true,
                setup_required: None,
            }),
        );
        assert!(state.result.is_none() && !state.verified_for(&old.server));
    }
}
