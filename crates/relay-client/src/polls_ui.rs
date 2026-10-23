const POLL_PREFIX: &str = "relay-poll:v1:";
const POLL_VOTE_PREFIX: &str = "relay-poll-vote:";
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct PollContent {
    question: String,
    options: Vec<String>,
    multiple: bool,
}
pub(super) fn poll_question(content: &str) -> Option<String> {
    PollContent::parse(content).map(|poll| poll.question)
}

impl Default for PollContent {
    fn default() -> Self {
        Self {
            question: String::new(),
            options: vec![String::new(), String::new()],
            multiple: false,
        }
    }
}
impl PollContent {
    fn valid(&self) -> bool {
        !self.question.trim().is_empty()
            && self.question.chars().count() <= 300
            && (2..=10).contains(&self.options.len())
            && self
                .options
                .iter()
                .all(|o| !o.trim().is_empty() && o.chars().count() <= 100)
            && {
                let mut unique = HashSet::new();
                self.options
                    .iter()
                    .all(|o| unique.insert(o.trim().to_lowercase()))
            }
    }
    fn parse(content: &str) -> Option<Self> {
        if content.len() > 8192 {
            return None;
        }
        let poll: Self = serde_json::from_str(content.strip_prefix(POLL_PREFIX)?).ok()?;
        poll.valid().then_some(poll)
    }
    fn counts(&self, reactions: &[Reaction]) -> Vec<HashSet<String>> {
        let mut counts = vec![HashSet::new(); self.options.len()];
        let mut used = HashSet::new();
        for (index, count) in counts.iter_mut().enumerate() {
            let key = format!("{POLL_VOTE_PREFIX}{index}");
            for reaction in reactions.iter().filter(|r| r.emoji == key) {
                for user in &reaction.user_ids {
                    if !user.is_empty() && (self.multiple || used.insert(user.clone())) {
                        count.insert(user.clone());
                    }
                }
            }
        }
        counts
    }
}
impl RelayApp {
    pub(super) fn poll_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut draft) = self.admin_ui.poll_draft.clone() else {
            return;
        };
        let mut open = true;
        let mut create = false;
        let window = desktop_ui::dialog(ctx, "Create Poll")
            .id(egui::Id::new("create-poll"))
            .open(&mut open)
            .collapsible(false)
            .default_width(420.0);
        // The shared header (title, Close, Escape) stays above the scrolling body.
        if host_setup_ui::show_with_header(ctx, window, "Create Poll", |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut draft.question)
                    .char_limit(300)
                    .hint_text("Ask a question")
                    .desired_width(f32::INFINITY),
            );
            let mut remove = None;
            for (index, option) in draft.options.iter_mut().enumerate() {
                ui.push_id(index, |ui| {
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(option)
                                .char_limit(100)
                                .hint_text(format!("Answer {}", index + 1)),
                        );
                        if ui.small_button("Remove").clicked() {
                            remove = Some(index);
                        }
                    });
                });
            }
            if let Some(index) = remove {
                if draft.options.len() > 2 {
                    draft.options.remove(index);
                }
            }
            if draft.options.len() < 10 && ui.button("Add Answer").clicked() {
                draft.options.push(String::new());
            }
            ui.checkbox(&mut draft.multiple, "Allow multiple answers");
            ui.label(
                RichText::new("The question, answers and votes use end-to-end encryption.")
                    .color(c_muted()),
            );
            create = ui
                .add_enabled(
                    draft.valid() && self.pending_sends.is_empty(),
                    primary("Create Poll"),
                )
                .clicked();
        }) {
            open = false;
            // Closing never creates the poll.
            create = false;
        }
        if create {
            if !self.draft.trim().is_empty() || !self.attachments.is_empty() || self.reply.is_some()
            {
                self.error =
                    Some("Send or clear your current draft before creating a poll.".into());
            } else {
                self.draft = format!("{POLL_PREFIX}{}", serde_json::to_string(&draft).unwrap());
                self.submit_composer();
                if self
                    .current
                    .as_ref()
                    .is_some_and(|t| self.pending_sends.contains(t))
                {
                    open = false;
                }
            }
        }
        self.admin_ui.poll_draft = if open { Some(draft) } else { None };
    }
    pub(super) fn open_poll(&mut self) {
        self.admin_ui.poll_draft = Some(PollContent::default());
    }
    pub(super) fn poll_view(&mut self, ui: &mut egui::Ui, message: &Message) -> bool {
        let Some(poll) = PollContent::parse(&message.content) else {
            return false;
        };
        let counts = poll.counts(&message.reactions);
        let own = self
            .session
            .as_ref()
            .map(|s| s.user.id.clone())
            .unwrap_or_default();
        let all: HashSet<_> = counts.iter().flat_map(|c| c.iter().cloned()).collect();
        let can_vote = self.current.as_ref().is_some_and(|target| match target {
            Target::Channel(id) => self
                .bootstrap
                .as_ref()
                .is_some_and(|b| b.channels.iter().any(|c| c.id == *id && c.can_send)),
            Target::Direct(id) => !self.is_blocked(id),
        });
        egui::Frame::none()
            .fill(c_card())
            .rounding(8.0)
            .inner_margin(Margin::same(14.0))
            .show(ui, |ui| {
                ui.label(RichText::new(&poll.question).size(font_size(17.0)).strong());
                ui.label(if poll.multiple {
                    "Select one or more answers"
                } else {
                    "Select one answer"
                });
                for (index, option) in poll.options.iter().enumerate() {
                    let selected = counts[index].contains(&own);
                    let percent = if all.is_empty() {
                        0.0
                    } else {
                        counts[index].len() as f32 / all.len() as f32
                    };
                    if ui
                        .add_enabled(
                            can_vote,
                            egui::SelectableLabel::new(
                                selected,
                                format!("{option} · {} votes", counts[index].len()),
                            ),
                        )
                        .clicked()
                    {
                        if !poll.multiple && !selected {
                            for reaction in &message.reactions {
                                if reaction.emoji.starts_with(POLL_VOTE_PREFIX)
                                    && reaction.user_ids.contains(&own)
                                {
                                    self.react_to_message(message, &reaction.emoji);
                                }
                            }
                        }
                        self.react_to_message(message, &format!("{POLL_VOTE_PREFIX}{index}"));
                    }
                    ui.add(
                        egui::ProgressBar::new(percent)
                            .desired_width(ui.available_width().min(340.0))
                            .show_percentage(),
                    );
                }
                ui.label(format!("{} voters", all.len()));
            });
        true
    }
    pub(super) fn is_poll_vote(emoji: &str) -> bool {
        emoji.starts_with(POLL_VOTE_PREFIX)
    }
}
#[cfg(test)]
mod poll_tests {
    use super::*;
    #[test]
    fn bounded_payloads_and_authenticated_vote_projection_count_each_member_once() {
        let poll = PollContent {
            question: "Where?".into(),
            options: vec!["Here".into(), "There".into()],
            multiple: false,
        };
        assert!(poll.valid());
        let rows = vec![
            Reaction {
                emoji: format!("{POLL_VOTE_PREFIX}0"),
                user_ids: vec!["alice".into(), "alice".into()],
            },
            Reaction {
                emoji: format!("{POLL_VOTE_PREFIX}1"),
                user_ids: vec!["alice".into(), "bob".into()],
            },
            Reaction {
                emoji: format!("{POLL_VOTE_PREFIX}99"),
                user_ids: vec!["mallory".into()],
            },
        ];
        let counts = poll.counts(&rows);
        assert_eq!(counts[0].len(), 1);
        assert_eq!(counts[1].len(), 1);
        assert!(counts[1].contains("bob"));
        let mut multi = poll.clone();
        multi.multiple = true;
        assert_eq!(multi.counts(&rows)[1].len(), 2);
        multi.options = vec!["Yes".into(), " yes ".into()];
        assert!(!multi.valid());
        assert!(PollContent::parse(&format!(
            "{POLL_PREFIX}{}",
            serde_json::to_string(&poll).unwrap()
        ))
        .is_some());
        assert!(PollContent::parse(&"x".repeat(8193)).is_none());
    }
}
