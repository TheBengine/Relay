//! R4.5: the rule for sending text without Relay's end-to-end layer. No I/O.
//!
//! The community's owner can announce the `open` mode, but the announcement
//! changes nothing for anyone who has not agreed: a client sends unprotected
//! text only for the exact revision its own user accepted. Going back to
//! `protected` and later to `open` again is a new revision, so an earlier
//! agreement can never be reused.
use relay_protocol::{CommunityProtection, ProtectionLevel};

/// Where one person stands in a community with respect to the announced mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Standing {
    /// Encryption is on, as it is by default.
    Protected,
    /// The owner announced the open mode and this person has not accepted it.
    /// Their messages stay encrypted.
    NeedsDecision,
    /// The open mode is announced and this person accepted that revision.
    Accepted,
}

pub fn plain_allowed(protection: &CommunityProtection, accepted: Option<u64>) -> bool {
    protection.level == ProtectionLevel::Open && accepted == Some(protection.revision)
}

pub fn standing(protection: &CommunityProtection, accepted: Option<u64>) -> Standing {
    match protection.level {
        ProtectionLevel::Protected => Standing::Protected,
        ProtectionLevel::Open if plain_allowed(protection, accepted) => Standing::Accepted,
        ProtectionLevel::Open => Standing::NeedsDecision,
    }
}

/// Files are served as they are only while the community runs without the
/// extra layer. Reading is not a choice that needs consent, but a file that
/// has a verified key always takes the encrypted path first.
pub fn plain_files_readable(protection: &CommunityProtection) -> bool {
    protection.level == ProtectionLevel::Open
}

/// The value handed to the network worker: the accepted revision, only while it
/// still applies.
pub fn consent_for(protection: &CommunityProtection, accepted: Option<u64>) -> Option<u64> {
    plain_allowed(protection, accepted).then_some(protection.revision)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn announced(level: ProtectionLevel, revision: u64) -> CommunityProtection {
        CommunityProtection {
            level,
            revision,
            changed_by: Some("owner".into()),
        }
    }

    #[test]
    fn a_community_without_an_announcement_is_protected_and_never_sends_plain_text() {
        let none = CommunityProtection::default();
        assert_eq!(none.level, ProtectionLevel::Protected);
        for accepted in [None, Some(0), Some(7)] {
            assert!(!plain_allowed(&none, accepted));
            assert_eq!(standing(&none, accepted), Standing::Protected);
            assert_eq!(consent_for(&none, accepted), None);
        }
        // An older server's bootstrap has no such field at all.
        let bootstrap: relay_protocol::Bootstrap = serde_json::from_value(serde_json::json!({
            "server_name": "Old",
            "user": {"id": "u", "username": "u", "role": "member"},
            "channels": [], "members": [], "voice_members": []
        }))
        .unwrap();
        assert_eq!(bootstrap.protection, CommunityProtection::default());
    }

    #[test]
    fn an_announcement_alone_never_lets_text_go_out_unprotected() {
        let open = announced(ProtectionLevel::Open, 1);
        assert!(!plain_allowed(&open, None));
        assert_eq!(standing(&open, None), Standing::NeedsDecision);
        assert_eq!(consent_for(&open, None), None);
    }

    #[test]
    fn agreement_is_for_one_revision_only() {
        let open = announced(ProtectionLevel::Open, 1);
        assert!(plain_allowed(&open, Some(1)));
        assert_eq!(standing(&open, Some(1)), Standing::Accepted);
        assert_eq!(consent_for(&open, Some(1)), Some(1));
        // The owner turned it off and on again: revision 3 is a new question.
        let again = announced(ProtectionLevel::Open, 3);
        assert!(!plain_allowed(&again, Some(1)));
        assert_eq!(standing(&again, Some(1)), Standing::NeedsDecision);
        // A stored agreement for a later revision than the announced one is not valid either.
        assert!(!plain_allowed(&open, Some(2)));
    }

    #[test]
    fn switching_back_to_protected_always_ends_unprotected_sending() {
        let back = announced(ProtectionLevel::Protected, 2);
        assert!(!plain_allowed(&back, Some(2)));
        assert_eq!(standing(&back, Some(1)), Standing::Protected);
        assert_eq!(consent_for(&back, Some(2)), None);
    }

    #[test]
    fn plain_files_are_readable_only_while_the_community_is_open() {
        assert!(!plain_files_readable(&CommunityProtection::default()));
        assert!(!plain_files_readable(&announced(
            ProtectionLevel::Protected,
            4
        )));
        assert!(plain_files_readable(&announced(ProtectionLevel::Open, 1)));
    }

    #[test]
    fn the_wire_format_is_stable_and_strict() {
        let value = serde_json::to_value(announced(ProtectionLevel::Open, 4)).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"level": "open", "revision": 4, "changed_by": "owner"})
        );
        assert!(serde_json::from_value::<CommunityProtection>(
            serde_json::json!({"level": "open", "revision": 1, "extra": true})
        )
        .is_err());
        assert!(serde_json::from_value::<CommunityProtection>(
            serde_json::json!({"level": "off", "revision": 1})
        )
        .is_err());
    }
}
