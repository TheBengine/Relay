//! Suppress identical presentation snapshots, after authentication/reconciliation.
//! Never suppress message deltas or a requested conversation load.
use super::Update;
use sha2::{Digest, Sha256};
use std::io::{self, Write};

#[derive(Default)]
pub(super) struct Snapshots {
    bootstrap: Option<[u8; 32]>,
    crypto: Option<[u8; 32]>,
}
struct HashWriter(Sha256);
impl Write for HashWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl Snapshots {
    pub(super) fn should_emit(&mut self, update: &Update) -> bool {
        let (slot, digest) = match update {
            Update::Connected { .. } | Update::AccountLoginComplete { .. } => {
                *self = Self::default();
                return true;
            }
            Update::Bootstrap(b) => {
                let mut writer = HashWriter(Sha256::new());
                if serde_json::to_writer(&mut writer, b).is_err() {
                    return true;
                }
                (&mut self.bootstrap, writer.0.finalize().into())
            }
            Update::CryptoStatus { status } => {
                let mut hash = Sha256::new();
                // Public UI state only. Length prefixes separate arbitrary strings.
                let mut text = |value: &str| {
                    hash.update((value.len() as u64).to_le_bytes());
                    hash.update(value.as_bytes());
                };
                text(&status.own_device_id);
                text(&status.own_fingerprint);
                text(&status.status);
                match &status.problem {
                    Some(problem) => {
                        text("problem-present");
                        text(problem);
                    }
                    None => text("no-problem"),
                }
                for peers in [&status.peers, &status.own_devices] {
                    text(&peers.len().to_string());
                    for peer in peers {
                        for value in [
                            &peer.user_id,
                            &peer.name,
                            &peer.device_id,
                            &peer.fingerprint,
                        ] {
                            text(value);
                        }
                        text(if peer.verified {
                            "verified"
                        } else {
                            "unverified"
                        });
                        text(if peer.changed { "changed" } else { "unchanged" });
                    }
                }
                // These marks change independently of the existing approval.
                // In particular, a manual comparison upgrades invitation trust.
                for devices in [&status.via_invitation, &status.invitation_unconfirmed] {
                    text(&devices.len().to_string());
                    for device in devices {
                        text(device);
                    }
                }
                text(if status.supported {
                    "supported"
                } else {
                    "unsupported"
                });
                if let Some(h) = &status.history {
                    text(&format!(
                        "{}/{}/{}/{}/{}",
                        h.archived_bytes, h.byte_limit, h.segments, h.segment_limit, h.records
                    ));
                } else {
                    text("no-history");
                }
                (&mut self.crypto, hash.finalize().into())
            }
            _ => return true,
        };
        if *slot == Some(digest) {
            return false;
        }
        *slot = Some(digest);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use relay_protocol::Bootstrap;
    #[test]
    fn unchanged_snapshots_are_quiet_but_permissions_and_reads_are_not() {
        let mut gate = Snapshots::default();
        let mut bootstrap = Bootstrap::default();
        assert!(gate.should_emit(&Update::Bootstrap(bootstrap.clone())));
        assert!(!gate.should_emit(&Update::Bootstrap(bootstrap.clone())));
        bootstrap.latest_change_sequence = 1;
        assert!(gate.should_emit(&Update::Bootstrap(bootstrap)));
        assert!(gate.should_emit(&Update::Busy(false)));
        assert!(gate.should_emit(&Update::Busy(false)));
    }

    #[test]
    fn crypto_problem_changes_are_emitted_even_with_unchanged_verification_and_diagnostic() {
        let mut gate = Snapshots::default();
        let mut status = crate::e2ee::CryptoUiStatus {
            status: "Device checks cached".into(),
            supported: true,
            ..Default::default()
        };
        assert!(gate.should_emit(&Update::CryptoStatus {
            status: status.clone()
        }));
        assert!(!gate.should_emit(&Update::CryptoStatus {
            status: status.clone()
        }));
        // Some("") still differs from None: the diagnostic is not its tag.
        status.problem = Some(String::new());
        assert!(gate.should_emit(&Update::CryptoStatus {
            status: status.clone()
        }));
        assert!(!gate.should_emit(&Update::CryptoStatus {
            status: status.clone()
        }));
        status.problem = Some("Conversation access needs attention".into());
        assert!(gate.should_emit(&Update::CryptoStatus {
            status: status.clone()
        }));
        status.problem = None;
        assert!(gate.should_emit(&Update::CryptoStatus { status }));
    }

    #[test]
    fn invitation_marks_and_own_device_changes_reach_the_ui() {
        let mut gate = Snapshots::default();
        let mut status = crate::e2ee::CryptoUiStatus {
            own_device_id: "own-device".into(),
            via_invitation: vec!["friend-device".into()],
            ..Default::default()
        };
        let mut observe = |status: &crate::e2ee::CryptoUiStatus| {
            gate.should_emit(&Update::CryptoStatus {
                status: status.clone(),
            })
        };
        assert!(observe(&status));
        assert!(!observe(&status));
        // Verification itself and the diagnostic are unchanged after comparing.
        status.via_invitation.clear();
        assert!(observe(&status));
        assert!(!observe(&status));
        status.invitation_unconfirmed.push("friend-device".into());
        assert!(observe(&status));
        assert!(!observe(&status));
        status.invitation_unconfirmed.clear();
        assert!(observe(&status));
        status.via_invitation.push("another-device".into());
        assert!(observe(&status));
        status.own_device_id = "replacement-device".into();
        assert!(observe(&status));
        assert!(!observe(&status));
    }
}
