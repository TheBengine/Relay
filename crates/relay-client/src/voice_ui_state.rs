//! Voice navigation policy, independent of native devices and network timing.

/// One failed call, kept only in RAM and bound to the account and displayed
/// error. Retrying is an explicit fresh join, never automatic admission repair.
pub struct Retry {
    pub origin: crate::network::CommandOrigin,
    pub channel_id: String,
    pub error: String,
}

impl Retry {
    pub fn target<'a>(
        &'a self,
        origin: Option<&crate::network::CommandOrigin>,
        bootstrap: Option<&relay_protocol::Bootstrap>,
        idle_and_connected: bool,
        error: Option<&str>,
    ) -> Option<&'a str> {
        if !idle_and_connected || origin != Some(&self.origin) || error != Some(self.error.as_str())
        {
            return None;
        }
        let bootstrap = bootstrap?;
        if bootstrap.server_id != self.origin.server_id || bootstrap.user.id != self.origin.user_id
        {
            return None;
        }
        let available = bootstrap.channels.iter().any(|channel| {
            channel.id == self.channel_id && channel.kind == "voice" && channel.can_join
        }) && !bootstrap
            .conversations
            .iter()
            .any(|room| room.channel_id == self.channel_id && room.archived);
        available.then_some(self.channel_id.as_str())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PauseAction {
    RetryCall,
    EncryptionDetails,
}

/// A live reconnect needs no device approval. Open calls never use MLS approval.
pub fn pause_action(reconnecting: bool, open: bool) -> Option<PauseAction> {
    if reconnecting {
        None
    } else if open {
        Some(PauseAction::RetryCall)
    } else {
        Some(PauseAction::EncryptionDetails)
    }
}

/// Selecting the joined or pending room is a no-op; another room replaces user intent.
pub fn should_join(current: Option<&str>, pending: Option<&str>, requested: &str) -> bool {
    current != Some(requested) && pending != Some(requested)
}

/// Only the still-requested room and exact request ID can authorize opening devices.
/// A room match alone is insufficient after a fast leave and rejoin of the same room.
pub fn accepts_join(
    pending: Option<&str>,
    expected_request: u64,
    room: &str,
    ack_request: u64,
) -> bool {
    pending == Some(room) && expected_request == ack_request
}

pub fn accepts_call_failure(
    current: Option<&str>,
    attempt: Option<u64>,
    joining: bool,
    room: &str,
    request: u64,
) -> bool {
    !joining && current == Some(room) && attempt == Some(request)
}

/// Match a displayed stream to current voice-room presence.
pub fn video_member<'a>(
    bootstrap: &'a relay_protocol::Bootstrap,
    channel: Option<&str>,
    user_id: &str,
    kind: &str,
) -> Option<&'a relay_protocol::User> {
    let channel = channel?;
    bootstrap
        .voice_members
        .iter()
        .find(|member| {
            member.user.id == user_id
                && member.channel_id == channel
                && match kind {
                    "camera" => member.camera,
                    "screen" => member.sharing,
                    _ => false,
                }
        })
        .map(|member| &member.user)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_call_failure_cannot_end_a_newer_attempt_or_another_room() {
        assert!(accepts_call_failure(
            Some("private-voice"),
            Some(93),
            false,
            "private-voice",
            93
        ));
        // Media/key refreshes keep original join 93 while their own generation
        // changes. A later explicit rejoin 94 must reject join 93's failure.
        assert!(!accepts_call_failure(
            Some("private-voice"),
            Some(94),
            false,
            "private-voice",
            93
        ));
        assert!(!accepts_call_failure(
            Some("other-room"),
            Some(93),
            false,
            "private-voice",
            93
        ));
        assert!(!accepts_call_failure(
            Some("private-voice"),
            Some(93),
            true,
            "private-voice",
            93
        ));
        assert!(!accepts_call_failure(
            None,
            Some(93),
            false,
            "private-voice",
            93
        ));
        assert!(!accepts_call_failure(
            Some("private-voice"),
            None,
            false,
            "private-voice",
            93
        ));
    }

    fn retry_fixture() -> (Retry, relay_protocol::Bootstrap) {
        let retry = Retry {
            origin: crate::network::CommandOrigin {
                url: "http://127.0.0.1:4580".into(),
                server_id: "own-community".into(),
                user_id: "own-account".into(),
            },
            channel_id: "private-voice".into(),
            error: "The call could not reconnect.".into(),
        };
        let mut bootstrap = relay_protocol::Bootstrap {
            server_id: retry.origin.server_id.clone(),
            channels: vec![relay_protocol::Channel {
                id: retry.channel_id.clone(),
                kind: "voice".into(),
                can_join: true,
                ..Default::default()
            }],
            ..Default::default()
        };
        bootstrap.user.id = retry.origin.user_id.clone();
        (retry, bootstrap)
    }

    #[test]
    fn call_retry_is_an_explicit_same_account_room_action_after_a_failure() {
        let (retry, bootstrap) = retry_fixture();
        assert_eq!(
            retry.target(
                Some(&retry.origin),
                Some(&bootstrap),
                true,
                Some(&retry.error)
            ),
            Some("private-voice")
        );
        // Display-only policy offers nothing while already joining/connected,
        // offline or busy; it never initiates an automatic second join.
        assert_eq!(
            retry.target(
                Some(&retry.origin),
                Some(&bootstrap),
                false,
                Some(&retry.error)
            ),
            None
        );
        for error in [None, Some("Upload failed"), Some("Sign in again")] {
            assert_eq!(
                retry.target(Some(&retry.origin), Some(&bootstrap), true, error),
                None
            );
        }
        assert_eq!(
            retry.target(None, Some(&bootstrap), true, Some(&retry.error)),
            None
        );
        assert_eq!(
            retry.target(Some(&retry.origin), None, true, Some(&retry.error)),
            None
        );
    }

    #[test]
    fn call_retry_cannot_cross_accounts_communities_or_addresses() {
        let (retry, bootstrap) = retry_fixture();
        for field in 0..3 {
            let mut origin = retry.origin.clone();
            match field {
                0 => origin.user_id.push_str("-other"),
                1 => origin.server_id.push_str("-other"),
                _ => origin.url.push_str("-other"),
            }
            assert_eq!(
                retry.target(Some(&origin), Some(&bootstrap), true, Some(&retry.error)),
                None
            );
        }
        for field in 0..2 {
            let mut bootstrap = bootstrap.clone();
            if field == 0 {
                bootstrap.user.id.push_str("-other");
            } else {
                bootstrap.server_id.push_str("-other");
            }
            assert_eq!(
                retry.target(
                    Some(&retry.origin),
                    Some(&bootstrap),
                    true,
                    Some(&retry.error)
                ),
                None
            );
        }
    }

    #[test]
    fn call_retry_revalidates_private_room_access_and_archival() {
        let (retry, mut bootstrap) = retry_fixture();
        for (kind, can_join) in [("text", true), ("voice", false)] {
            bootstrap.channels[0].kind = kind.into();
            bootstrap.channels[0].can_join = can_join;
            assert_eq!(
                retry.target(
                    Some(&retry.origin),
                    Some(&bootstrap),
                    true,
                    Some(&retry.error)
                ),
                None
            );
        }
        bootstrap.channels[0].kind = "voice".into();
        bootstrap.channels[0].can_join = true;
        bootstrap
            .conversations
            .push(relay_protocol::ConversationInfo {
                channel_id: retry.channel_id.clone(),
                kind: "group_voice".into(),
                archived: true,
                ..Default::default()
            });
        assert_eq!(
            retry.target(
                Some(&retry.origin),
                Some(&bootstrap),
                true,
                Some(&retry.error)
            ),
            None
        );
        bootstrap.conversations[0].archived = false;
        assert_eq!(
            retry.target(
                Some(&retry.origin),
                Some(&bootstrap),
                true,
                Some(&retry.error)
            ),
            Some("private-voice")
        );
        bootstrap.channels.clear();
        assert_eq!(
            retry.target(
                Some(&retry.origin),
                Some(&bootstrap),
                true,
                Some(&retry.error)
            ),
            None
        );
    }

    #[test]
    fn reconnects_and_open_call_errors_never_request_device_verification() {
        assert_eq!(pause_action(true, true), None);
        assert_eq!(pause_action(true, false), None);
        assert_eq!(pause_action(false, true), Some(PauseAction::RetryCall));
        assert_eq!(
            pause_action(false, false),
            Some(PauseAction::EncryptionDetails)
        );
    }

    #[test]
    fn selecting_a_voice_room_while_idle_starts_one_join() {
        assert!(should_join(None, None, "lounge"));
        // Once that request is pending, further room clicks cannot duplicate it.
        assert!(!should_join(None, Some("lounge"), "lounge"));
    }

    #[test]
    fn selecting_the_joined_room_does_not_disconnect_or_reopen_devices() {
        assert!(!should_join(Some("lounge"), None, "lounge"));
    }

    #[test]
    fn selecting_another_room_replaces_a_different_pending_join() {
        assert!(should_join(Some("lounge"), None, "squad"));
        assert!(!should_join(Some("lounge"), Some("squad"), "squad"));
        assert!(!should_join(Some("lounge"), Some("squad"), "lounge"));
        assert!(should_join(None, Some("squad"), "another-room"));
        assert!(!accepts_join(Some("another-room"), 44, "squad", 43));
    }

    #[test]
    fn current_matching_acknowledgement_allows_device_start() {
        assert!(accepts_join(Some("lounge"), 42, "lounge", 42));
    }

    #[test]
    fn leaving_before_the_acknowledgement_cancels_device_start() {
        // The completed request may still produce a ticket after cancellation.
        assert!(!accepts_join(None, 42, "lounge", 42));
        assert!(!accepts_join(None, 43, "lounge", 42));
    }

    #[test]
    fn old_room_acknowledgement_cannot_replace_the_new_room_request() {
        assert!(!accepts_join(Some("squad"), 44, "lounge", 43));
        // Even an incorrectly matching request ID cannot authorize another room.
        assert!(!accepts_join(Some("squad"), 44, "lounge", 44));
    }

    #[test]
    fn stale_same_room_acknowledgement_cannot_open_devices_after_rejoin() {
        // Join 42 was cancelled; the user intentionally requested room "lounge" again.
        assert!(!accepts_join(Some("lounge"), 44, "lounge", 42));
        assert!(accepts_join(Some("lounge"), 44, "lounge", 44));
        assert!(!accepts_join(Some("lounge"), 44, "lounge", 45));
    }

    fn video_bootstrap() -> relay_protocol::Bootstrap {
        relay_protocol::Bootstrap {
            voice_members: vec![relay_protocol::VoiceMember {
                user: relay_protocol::User {
                    id: "friend".into(),
                    username: "Current name".into(),
                    ..Default::default()
                },
                channel_id: "private-room".into(),
                sharing: true,
                camera: true,
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn stopped_stream_cannot_return_from_a_cached_frame_after_roster_update() {
        let mut bootstrap = video_bootstrap();
        let cached_frames = [("screen", 17), ("camera", 18)];
        let visible = |bootstrap: &relay_protocol::Bootstrap| {
            cached_frames
                .iter()
                .filter(|(kind, _)| {
                    video_member(bootstrap, Some("private-room"), "friend", kind).is_some()
                })
                .map(|(_, timestamp)| *timestamp)
                .collect::<Vec<_>>()
        };
        assert_eq!(visible(&bootstrap), [17, 18]);
        // A bootstrap removes the old screen tile. A late frame/snapshot is then
        // read before the SDK's unpublish event arrives; it must not recreate it.
        bootstrap.voice_members[0].sharing = false;
        assert_eq!(visible(&bootstrap), [18], "stopped screen must stay absent");
        bootstrap.voice_members[0].camera = false;
        assert!(
            visible(&bootstrap).is_empty(),
            "stopped camera must stay absent"
        );
        bootstrap.voice_members.clear();
        assert!(
            visible(&bootstrap).is_empty(),
            "departed peer must stay absent"
        );
    }

    #[test]
    fn video_source_flags_are_independent_and_unknown_kinds_are_rejected() {
        let mut bootstrap = video_bootstrap();
        for (sharing, camera) in [(true, false), (false, true), (false, false), (true, true)] {
            bootstrap.voice_members[0].sharing = sharing;
            bootstrap.voice_members[0].camera = camera;
            assert_eq!(
                video_member(&bootstrap, Some("private-room"), "friend", "screen").is_some(),
                sharing
            );
            assert_eq!(
                video_member(&bootstrap, Some("private-room"), "friend", "camera").is_some(),
                camera
            );
            for unknown in ["", "application-audio", "other-source"] {
                assert!(
                    video_member(&bootstrap, Some("private-room"), "friend", unknown).is_none()
                );
            }
        }
    }

    #[test]
    fn video_requires_current_call_and_peer_even_if_cached_media_is_valid() {
        let bootstrap = video_bootstrap();
        for (channel, peer) in [
            (None, "friend"),
            (Some("another-room"), "friend"),
            (Some("private-room"), "removed-friend"),
        ] {
            assert!(video_member(&bootstrap, channel, peer, "camera").is_none());
        }
        let user = video_member(&bootstrap, Some("private-room"), "friend", "camera").unwrap();
        assert_eq!(user.username, "Current name");
    }
}
