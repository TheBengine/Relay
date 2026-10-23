use crate::network::{CommandOrigin, Target};
use relay_protocol::{HistoryPage, Message};
use std::collections::HashSet;

pub struct UnreadNavigation {
    pub origin: CommandOrigin,
    pub target: Target,
    pub read_sequence: u64,
    pub after: u64,
    pub pending: Option<u64>,
    pub error: Option<String>,
}

impl UnreadNavigation {
    pub fn new(origin: CommandOrigin, target: Target, read_sequence: u64) -> Self {
        Self {
            origin,
            target,
            read_sequence,
            after: read_sequence,
            pending: None,
            error: None,
        }
    }

    pub fn accepts(
        &self,
        origin: &CommandOrigin,
        target: &Target,
        request_id: u64,
        current_origin: Option<&CommandOrigin>,
        current_target: Option<&Target>,
    ) -> bool {
        self.pending == Some(request_id)
            && &self.origin == origin
            && &self.target == target
            && current_origin == Some(origin)
            && current_target == Some(target)
    }

    pub fn first_incoming<'a>(&self, messages: &'a [Message]) -> Option<&'a Message> {
        messages.iter().find(|message| {
            message.sequence > self.read_sequence && message.author.id != self.origin.user_id
        })
    }
}

pub fn follow_latest(searching: bool, reading_unreads: bool) -> bool {
    !searching && !reading_unreads
}

/// A periodic latest-page refresh may contain rows older than the page opened
/// from Inbox. Keep its start stable while still accepting new replies and edits.
pub fn live_page(messages: Vec<Message>, start: Option<u64>) -> Vec<Message> {
    messages
        .into_iter()
        .filter(|message| start.is_none_or(|start| message.sequence >= start))
        .collect()
}

/// Validate the bounded ascending transport page before decrypting or hiding
/// control messages. Global sequence gaps are valid; a stalled cursor is not.
pub fn next_after(
    page: &HistoryPage,
    target: &Target,
    user_id: &str,
    after: u64,
) -> Result<Option<u64>, String> {
    if page.messages.len() > 100
        || after > i64::MAX as u64
        || page.latest_sequence > i64::MAX as u64
    {
        return Err("Invalid unread history page limit or cursor.".into());
    }
    let mut cursor = after;
    let mut ids = HashSet::new();
    for message in &page.messages {
        let routing_matches = match target {
            Target::Channel(id) => {
                message.channel_id.as_ref() == Some(id) && message.recipient_id.is_none()
            }
            Target::Direct(id) => {
                message.channel_id.is_none()
                    && ((message.author.id == user_id && message.recipient_id.as_ref() == Some(id))
                        || (message.author.id == *id
                            && message.recipient_id.as_deref() == Some(user_id)))
            }
        };
        if !routing_matches
            || message.sequence <= cursor
            || message.sequence > page.latest_sequence
            || message.id.is_empty()
            || !ids.insert(&message.id)
        {
            return Err("The unread history page has invalid routing or ordering.".into());
        }
        cursor = message.sequence;
    }
    if cursor < page.latest_sequence {
        if cursor == after {
            return Err("Unread history did not advance. Try again.".into());
        }
        Ok(Some(cursor))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn origin() -> CommandOrigin {
        CommandOrigin {
            url: "http://fixture".into(),
            server_id: "server".into(),
            user_id: "me".into(),
        }
    }
    fn row(sequence: u64, author: &str) -> Message {
        Message {
            id: format!("m{sequence}"),
            sequence,
            channel_id: Some("room".into()),
            author: relay_protocol::User {
                id: author.into(),
                ..Default::default()
            },
            ..Default::default()
        }
    }
    fn page(messages: Vec<Message>, latest_sequence: u64) -> HistoryPage {
        HistoryPage {
            messages,
            latest_sequence,
            ..Default::default()
        }
    }
    #[test]
    fn scans_beyond_a_full_page_of_own_messages_to_first_actual_reply() {
        let mut nav = UnreadNavigation::new(origin(), Target::Channel("room".into()), 20);
        let own = page((21..=120).map(|n| row(n, "me")).collect(), 1000);
        assert!(nav.first_incoming(&own.messages).is_none());
        nav.after = next_after(&own, &nav.target, "me", nav.after)
            .unwrap()
            .unwrap();
        let replies = page(
            vec![row(121, "me"), row(190, "peer"), row(500, "peer")],
            1000,
        );
        assert_eq!(nav.first_incoming(&replies.messages).unwrap().sequence, 190);
        assert_eq!(nav.read_sequence, 20);
    }
    #[test]
    fn periodic_latest_refresh_preserves_unread_start_and_accepts_new_replies_and_edits() {
        let mut messages = vec![row(9, "peer"), row(13, "peer")];
        let mut refreshed = row(9, "peer");
        refreshed.content = "edited".into();
        let incoming = live_page(
            vec![row(1, "me"), row(8, "me"), refreshed, row(14, "peer")],
            Some(9),
        );
        crate::timeline::merge(&mut messages, incoming, false);
        assert_eq!(
            messages.iter().map(|m| m.sequence).collect::<Vec<_>>(),
            [9, 13, 14]
        );
        assert_eq!(messages[0].content, "edited");
        assert_eq!(live_page(vec![row(1, "me")], None).len(), 1);
    }

    #[test]
    fn opening_unreads_does_not_stick_to_the_bottom_when_rows_and_media_arrive() {
        use eframe::egui;
        for reading_unreads in [true, false] {
            let ctx = egui::Context::default();
            let mut last_offset = 0.0;
            for (frame, height) in [0.0, 1400.0, 1800.0].into_iter().enumerate() {
                let _ = ctx.run(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(940.0, 650.0),
                        )),
                        time: Some(frame as f64),
                        ..Default::default()
                    },
                    |ctx| {
                        egui::CentralPanel::default().show(ctx, |ui| {
                            let mut scroll = egui::ScrollArea::vertical()
                                .id_source("unread-inbox-test")
                                .stick_to_bottom(follow_latest(false, reading_unreads))
                                .animated(false);
                            if frame == 1 && reading_unreads {
                                scroll = scroll.vertical_scroll_offset(0.0);
                            }
                            let output = scroll.show(ui, |ui| {
                                ui.add_space(height);
                            });
                            last_offset = output.state.offset.y;
                        });
                    },
                );
                if reading_unreads {
                    assert!(last_offset <= 1.0, "frame {frame} moved to {last_offset}");
                }
            }
            if !reading_unreads {
                assert!(last_offset > 1000.0);
            }
        }
    }

    #[test]
    fn first_read_floor_zero_still_opens_the_first_incoming_message() {
        let nav = UnreadNavigation::new(origin(), Target::Channel("room".into()), 0);
        assert_eq!(
            nav.first_incoming(&[row(1, "me"), row(2, "peer")])
                .unwrap()
                .sequence,
            2
        );
    }
    #[test]
    fn raw_cursor_survives_a_page_of_hidden_controls() {
        let raw = page(vec![row(25, "peer"), row(40, "peer")], 90);
        let next = next_after(&raw, &Target::Channel("room".into()), "me", 20).unwrap();
        let mut visible = raw;
        visible.messages.clear();
        assert_eq!(next, Some(40));
        assert!(
            UnreadNavigation::new(origin(), Target::Channel("room".into()), 20)
                .first_incoming(&visible.messages)
                .is_none()
        );
    }
    #[test]
    fn request_origin_and_target_must_all_match_after_switch_or_retry() {
        let mut nav = UnreadNavigation::new(origin(), Target::Channel("room".into()), 20);
        nav.pending = Some(8);
        assert!(nav.accepts(
            &nav.origin,
            &nav.target,
            8,
            Some(&nav.origin),
            Some(&nav.target)
        ));
        for field in ["url", "server", "user"] {
            let mut changed = origin();
            match field {
                "url" => changed.url = "other".into(),
                "server" => changed.server_id = "other".into(),
                _ => changed.user_id = "other".into(),
            }
            assert!(!nav.accepts(
                &nav.origin,
                &nav.target,
                8,
                Some(&changed),
                Some(&nav.target)
            ));
        }
        assert!(!nav.accepts(
            &nav.origin,
            &nav.target,
            7,
            Some(&nav.origin),
            Some(&nav.target)
        ));
        assert!(!nav.accepts(
            &nav.origin,
            &nav.target,
            8,
            Some(&nav.origin),
            Some(&Target::Channel("other".into()))
        ));
        nav.pending = None;
        assert!(!nav.accepts(
            &nav.origin,
            &nav.target,
            8,
            Some(&nav.origin),
            Some(&nav.target)
        ));
    }
    #[test]
    fn rejects_stalled_reversed_duplicate_or_oversized_transport_pages() {
        let target = Target::Channel("room".into());
        for bad in [
            page(vec![], 40),
            page(vec![row(20, "peer")], 40),
            page(vec![row(30, "peer"), row(29, "peer")], 40),
            page(vec![row(30, "peer"), row(30, "peer")], 40),
            page((21..=121).map(|n| row(n, "peer")).collect(), 121),
            page(vec![row(41, "peer")], 40),
        ] {
            assert!(next_after(&bad, &target, "me", 20).is_err());
        }
        assert_eq!(
            next_after(&page(vec![], 10), &target, "me", 20).unwrap(),
            None
        );
        assert_eq!(
            next_after(&page(vec![row(30, "peer")], 30), &target, "me", 20).unwrap(),
            None
        );
    }
    #[test]
    fn rejects_wrong_channel_and_nonparticipant_direct_rows_before_decrypt() {
        let target = Target::Channel("other".into());
        assert!(next_after(&page(vec![row(30, "peer")], 30), &target, "me", 20).is_err());
        let target = Target::Direct("peer".into());
        let mut direct = row(30, "peer");
        direct.channel_id = None;
        direct.recipient_id = Some("me".into());
        assert!(next_after(&page(vec![direct.clone()], 30), &target, "me", 20).is_ok());
        direct.recipient_id = Some("stranger".into());
        assert!(next_after(&page(vec![direct], 30), &target, "me", 20).is_err());
    }
}
