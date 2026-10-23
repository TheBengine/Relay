use relay_protocol::Message;
use std::collections::{HashMap, HashSet};

pub const MAX_MESSAGES: usize = 600;

/// Keep a newly opened latest page at its end while media and row heights settle.
/// A restored reading position or an explicit user interaction takes priority.
#[derive(Default)]
pub struct BottomFollow {
    enabled: bool,
}

impl BottomFollow {
    pub fn begin(&mut self, has_saved_position: bool) {
        self.enabled = !has_saved_position;
    }

    pub fn cancel(&mut self) {
        self.enabled = false;
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn align(&mut self, height_changed: bool, requested: bool) -> bool {
        self.enabled |= requested;
        self.enabled && (height_changed || requested)
    }
}

/// Merge overlapping pages and edits by immutable message ID. Pages never duplicate rows.
pub fn merge(messages: &mut Vec<Message>, incoming: Vec<Message>, keep_oldest: bool) -> bool {
    let mut positions: HashMap<_, _> = messages
        .iter()
        .enumerate()
        .map(|(index, message)| (message.id.clone(), index))
        .collect();
    for message in incoming {
        if let Some(index) = positions.get(&message.id).copied() {
            messages[index] = message;
        } else {
            positions.insert(message.id.clone(), messages.len());
            messages.push(message);
        }
    }
    messages.sort_by(|a, b| {
        (a.sequence, &a.created_at, &a.id).cmp(&(b.sequence, &b.created_at, &b.id))
    });
    let trimmed = messages.len() > MAX_MESSAGES;
    if trimmed {
        if keep_oldest {
            messages.truncate(MAX_MESSAGES);
        } else {
            messages.drain(..messages.len() - MAX_MESSAGES);
        }
    }
    trimmed
}

/// Locate the incoming unread boundary in the full timeline, before virtualization.
/// Own messages never move the boundary or the persisted read cursor.
pub fn first_unread_index<'a>(
    mut messages: impl Iterator<Item = &'a Message>,
    current_user_id: &str,
    read_sequence: u64,
) -> Option<usize> {
    if read_sequence == 0 {
        return None;
    }
    messages.position(|message| {
        message.sequence > read_sequence && message.author.id != current_user_id
    })
}

/// Layout only rows near the viewport, including overscan for keyboard and wheel movement.
pub fn visible_range(heights: &[f32], top: f32, bottom: f32) -> std::ops::Range<usize> {
    let mut y = 0.0;
    let mut first = heights.len();
    let mut last = heights.len();
    for (index, height) in heights.iter().enumerate() {
        let end = y + height;
        if first == heights.len() && end >= top {
            first = index;
        }
        if y > bottom {
            last = index;
            break;
        }
        y = end;
    }
    first.saturating_sub(2)..(last + 2).min(heights.len())
}

#[derive(Default)]
pub struct RowHeights {
    width: u32,
    heights: HashMap<String, f32>,
}

impl RowHeights {
    pub fn prepare(&mut self, width: f32, messages: &[Message]) {
        self.prepare_iter(width, messages.iter());
    }
    pub fn prepare_iter<'a>(&mut self, width: f32, messages: impl Iterator<Item = &'a Message>) {
        let width = width.round().max(0.0) as u32;
        if self.width != width {
            self.width = width;
            self.heights.clear();
        }
        if self.heights.len() > MAX_MESSAGES * 2 {
            let ids: HashSet<_> = messages.map(|message| message.id.as_str()).collect();
            self.heights.retain(|id, _| ids.contains(id.as_str()));
        }
    }
    pub fn estimate(&self, message: &Message) -> f32 {
        self.heights.get(&message.id).copied().unwrap_or_else(|| {
            let width = self.width.saturating_sub(100).max(120) as usize;
            let lines = message
                .content
                .lines()
                .map(|line| (line.chars().count() * 8).div_ceil(width).max(1))
                .sum::<usize>();
            65.0 + lines as f32 * 21.0
                + message.attachments.len() as f32 * 90.0
                + if message.reply_to_id.is_some() {
                    30.0
                } else {
                    0.0
                }
        })
    }
    pub fn record(&mut self, id: &str, height: f32) -> bool {
        let height = height.max(1.0);
        let changed = self
            .heights
            .get(id)
            .is_none_or(|old| (old - height).abs() > 0.5);
        self.heights.insert(id.into(), height);
        changed
    }
    pub fn invalidate(&mut self, id: &str) {
        self.heights.remove(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui;
    fn message(sequence: u64, text: &str) -> Message {
        Message {
            id: sequence.to_string(),
            sequence,
            content: text.into(),
            ..Default::default()
        }
    }
    #[test]
    fn overlapping_pages_and_edits_do_not_duplicate_or_reorder_messages() {
        let mut messages = vec![message(4, "old"), message(5, "fifth")];
        merge(
            &mut messages,
            vec![
                message(2, "second"),
                message(3, "third"),
                message(4, "edited"),
            ],
            true,
        );
        assert_eq!(
            messages.iter().map(|m| m.sequence).collect::<Vec<_>>(),
            [2, 3, 4, 5]
        );
        assert_eq!(messages[2].content, "edited");
    }
    #[test]
    fn paging_is_bounded_without_discarding_the_end_the_user_is_reading() {
        let all = (1..=1000).map(|id| message(id, "text")).collect::<Vec<_>>();
        let mut recent = Vec::new();
        assert!(merge(&mut recent, all.clone(), false));
        assert_eq!(recent.first().unwrap().sequence, 401);
        let mut older = Vec::new();
        assert!(merge(&mut older, all, true));
        assert_eq!(older.last().unwrap().sequence, 600);
    }
    #[test]
    fn variable_height_virtualization_has_overscan_and_handles_empty_history() {
        assert_eq!(visible_range(&[], 0.0, 100.0), 0..0);
        let heights = vec![20.0; 1000];
        let range = visible_range(&heights, 6000.0, 6400.0);
        assert!(range.start <= 300 && range.end >= 320);
        assert!(range.len() < 30);
    }

    #[test]
    fn late_media_growth_keeps_latest_message_visible_without_scroll_animation() {
        let context = egui::Context::default();
        let mut follow = BottomFollow::default();
        follow.begin(false);
        let mut previous_height = 0.0_f32;
        for height in [140.0_f32, 140.0, 360.0, 360.0] {
            let mut bottom = None;
            let _ = context.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(400.0, 240.0),
                    )),
                    ..Default::default()
                },
                |context| {
                    egui::CentralPanel::default().show(context, |ui| {
                        let output = egui::ScrollArea::vertical()
                            .id_source("late-media")
                            .max_height(100.0)
                            .animated(false)
                            .stick_to_bottom(true)
                            .show(ui, |ui| {
                                ui.add_space(height);
                                ui.label("Newest message");
                                if follow.align(height != previous_height, false) {
                                    ui.scroll_to_cursor(Some(egui::Align::BOTTOM));
                                }
                            });
                        bottom = Some((
                            output.state.offset.y + output.inner_rect.height(),
                            output.content_size.y,
                        ));
                    });
                },
            );
            let (viewport_bottom, content_bottom) = bottom.unwrap();
            assert!(viewport_bottom >= content_bottom - 1.0);
            previous_height = height;
        }
    }

    #[test]
    fn saved_reading_position_and_manual_navigation_cancel_initial_bottom_follow() {
        let mut follow = BottomFollow::default();
        follow.begin(true);
        assert!(!follow.align(true, false));
        follow.begin(false);
        assert!(follow.align(true, false));
        follow.cancel();
        assert!(!follow.align(true, false));
        assert!(follow.align(false, true));
        assert!(!follow.align(false, false));
        assert!(follow.align(true, false));
    }

    fn authored(sequence: u64, author: &str) -> Message {
        let mut result = message(sequence, "text");
        result.author.id = author.into();
        result
    }

    #[test]
    fn own_pending_and_confirmed_messages_do_not_start_an_unread_boundary() {
        let rows = [authored(12, "peer"), authored(13, "me"), authored(0, "me")];
        assert_eq!(first_unread_index(rows.iter(), "me", 12), None);
    }

    #[test]
    fn incoming_unread_boundary_survives_own_messages_on_either_side() {
        let rows = [
            authored(12, "peer"),
            authored(13, "me"),
            authored(14, "peer"),
            authored(15, "me"),
            authored(16, "peer"),
        ];
        assert_eq!(first_unread_index(rows.iter(), "me", 12), Some(2));
        assert_eq!(first_unread_index(rows.iter(), "me", 14), Some(4));
        assert_eq!(first_unread_index(rows.iter(), "me", 16), None);
    }

    #[test]
    fn virtualized_view_does_not_repeat_a_boundary_for_later_unread_rows() {
        let rows: Vec<_> = (1..=100).map(|seq| authored(seq, "peer")).collect();
        let boundary = first_unread_index(rows.iter(), "me", 10).unwrap();
        let visible = visible_range(&vec![20.0; rows.len()], 1400.0, 1600.0);
        assert_eq!(boundary, 10);
        assert!(!visible.contains(&boundary));
        assert_eq!(rows[boundary].sequence, 11);
    }

    #[test]
    fn absent_read_cursor_and_fully_read_reopened_history_have_no_boundary() {
        let rows = [authored(12, "peer"), authored(13, "me")];
        assert_eq!(first_unread_index(rows.iter(), "me", 0), None);
        assert_eq!(first_unread_index(rows.iter(), "me", 13), None);
    }
}
