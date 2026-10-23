//! Message previews preserve formatting and keep spoilers hidden until requested.
use super::*;

fn preview_job(
    content: &str,
    revealed: bool,
    code_background: Color32,
    custom: &[crate::emoji::CustomEmoji],
) -> (egui::text::LayoutJob, bool, crate::emoji::Inline) {
    let mut job = egui::text::LayoutJob::default();
    let mut inline = crate::emoji::Inline::default();
    let mut has_spoilers = false;
    let base_size = font_size(if crate::emoji::emoji_only_with_custom(content, custom) {
        30.0
    } else {
        15.0
    });
    for span in crate::message_format::parse(content) {
        has_spoilers |= span.style.spoiler;
        let hidden = span.style.spoiler && !revealed;
        let mut text = if hidden { "[spoiler]" } else { &span.text };
        let size = span.style.text_size(base_size);
        if span.style.quote && !hidden {
            if let Some(rest) = text.strip_prefix("│ ") {
                inline.quote_bar(&mut job, size, c_muted());
                text = rest;
            }
        }
        inline.append_with_custom(
            &mut job,
            text,
            egui::TextFormat {
                font_id: if span.style.code {
                    FontId::monospace(size)
                } else if span.style.bold {
                    FontId::new(size, egui::FontFamily::Name("Relay Semibold".into()))
                } else {
                    FontId::proportional(size)
                },
                color: if hidden || span.style.small || span.style.quote {
                    c_muted()
                } else {
                    c_text()
                },
                background: if hidden {
                    c_line()
                } else if span.style.code {
                    code_background
                } else {
                    Color32::TRANSPARENT
                },
                italics: span.style.italic,
                underline: if span.style.underline {
                    Stroke::new(1.0_f32, c_text())
                } else {
                    Stroke::NONE
                },
                strikethrough: if span.style.strike {
                    Stroke::new(1.0_f32, c_text())
                } else {
                    Stroke::NONE
                },
                ..Default::default()
            },
            !hidden && !span.style.code,
            custom,
        );
    }
    (job, has_spoilers, inline)
}

impl RelayApp {
    pub(super) fn delivery_preview(
        &mut self,
        ui: &mut egui::Ui,
        message: &Message,
        color: Color32,
        confirmed: bool,
    ) {
        let custom = self.inline_community_emoji(&message.content, false);
        render_delivery_content(
            ui,
            &message.content,
            &custom,
            (color, confirmed),
            &mut self.online_gifs,
            &self.prefs,
        );
    }

    pub(super) fn message_preview(&mut self, ui: &mut egui::Ui, message: &Message) {
        if let Some(question) = admin_ui::poll_question(&message.content) {
            ui.label(format!("Poll: {question}"));
            return;
        }
        let id = ui.id().with(("preview-spoilers", &message.id));
        let revealed = ui.data(|data| data.get_temp::<bool>(id).unwrap_or(false));
        let custom = self.inline_community_emoji(&message.content, revealed);
        let (mut job, has_spoilers, inline) = preview_job(
            &message.content,
            revealed,
            ui.visuals().code_bg_color,
            &custom,
        );
        job.wrap.max_width = ui.available_width().max(1.0);
        let galley = ui.painter().layout_job(job);
        let response =
            ui.add(egui::Label::new(egui::WidgetText::Galley(galley.clone())).wrap(true));
        response
            .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, &inline.accessible));
        inline.paint(ui, &galley, response.rect.min);
        if has_spoilers
            && ui
                .small_button(if revealed {
                    "Hide spoilers"
                } else {
                    "Show spoilers"
                })
                .clicked()
        {
            ui.data_mut(|data| data.insert_temp(id, !revealed));
        }
    }
}

pub(super) fn render_delivery_content(
    ui: &mut egui::Ui,
    content: &str,
    custom: &[crate::emoji::CustomEmoji],
    delivery: (Color32, bool),
    gifs: &mut online_gif_ui::GifBrowser,
    prefs: &Preferences,
) {
    if gifs.draw_message(ui, content, prefs) {
        return;
    }
    let (color, confirmed) = delivery;
    let (mut job, _, inline) = delivery_job(
        content,
        ui.visuals().code_bg_color,
        custom,
        color,
        confirmed,
    );
    job.wrap.max_width = ui.available_width().max(1.0);
    let galley = ui.painter().layout_job(job);
    let response = ui.add(egui::Label::new(egui::WidgetText::Galley(galley.clone())).wrap(true));
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, &inline.accessible));
    inline.paint(ui, &galley, response.rect.min);
}

fn delivery_job(
    content: &str,
    code_background: Color32,
    custom: &[crate::emoji::CustomEmoji],
    color: Color32,
    confirmed: bool,
) -> (egui::text::LayoutJob, bool, crate::emoji::Inline) {
    let (mut job, spoilers, mut inline) = preview_job(content, false, code_background, custom);
    for section in &mut job.sections {
        if section.format.color != Color32::TRANSPARENT {
            section.format.color = color;
        }
        if section.format.underline != Stroke::NONE {
            section.format.underline.color = color;
        }
        if section.format.strikethrough != Stroke::NONE {
            section.format.strikethrough.color = color;
        }
    }
    if !confirmed {
        inline.tint_images(Color32::from_white_alpha(140));
    }
    (job, spoilers, inline)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emoji_link_labels_keep_accessible_text_while_code_and_spoilers_stay_literal() {
        let (job, spoilers, inline) = preview_job(
            "[Guide 👩🏽‍💻](https://example.com) `😭` ||[Secret 😭](https://example.com/private)||",
            false,
            Color32::BLACK,
            &[],
        );
        assert!(spoilers);
        assert!(job.text.starts_with("Guide \u{2003}"));
        assert!(job.text.contains("😭"));
        assert_eq!(job.text.matches('\u{2003}').count(), 1);
        assert_eq!(inline.accessible, "Guide 👩🏽‍💻 😭 [spoiler]");
        assert!(!inline.accessible.contains("Secret"));
        assert!(!inline.accessible.contains("private"));
    }

    #[test]
    fn optimistic_delivery_keeps_formatting_and_never_exposes_spoilers() {
        let color = Color32::from_rgb(160, 30, 30);
        let content = "**Ready** ||hidden surprise|| __underlined__ 😭\n> A quote";
        let (job, spoilers, inline) = delivery_job(content, Color32::BLACK, &[], color, false);
        assert!(spoilers);
        assert!(!job.text.contains("hidden surprise"));
        assert!(!inline.accessible.contains("hidden surprise"));
        assert!(!job.text.contains("**"));
        assert!(inline.accessible.contains("😭"));
        assert!(job
            .sections
            .iter()
            .any(|section| section.format.underline.color == color
                && section.format.underline != Stroke::NONE));
        assert!(job
            .sections
            .iter()
            .all(|section| section.format.color == color
                || section.format.color == Color32::TRANSPARENT));
    }

    #[test]
    fn community_emoji_preview_keeps_spoilers_hidden_and_code_literal() {
        let emoji = crate::emoji::CustomEmoji {
            token: "<emoji:squad:owned>".into(),
            name: "squad".into(),
            texture: None,
        };
        let content = "<emoji:squad:owned> ||<emoji:squad:owned>|| `<emoji:squad:owned>`";
        let (hidden, _, inline) =
            preview_job(content, false, Color32::BLACK, std::slice::from_ref(&emoji));
        assert_eq!(hidden.text.matches("<emoji:squad:owned>").count(), 1);
        assert_eq!(inline.accessible, ":squad: [spoiler] <emoji:squad:owned>");
        let (revealed, _, inline) =
            preview_job(content, true, Color32::BLACK, std::slice::from_ref(&emoji));
        assert_eq!(revealed.text.matches("<emoji:squad:owned>").count(), 1);
        assert_eq!(inline.accessible, ":squad: :squad: <emoji:squad:owned>");
    }

    #[test]
    fn hidden_preview_does_not_expose_spoiler_text_or_urls_to_copy_or_accessibility() {
        let content = "**Plan** ||private surprise https://example.com/secret|| `code`";
        let (hidden, has_spoilers, _) = preview_job(content, false, Color32::BLACK, &[]);
        assert!(has_spoilers);
        assert!(!hidden.text.contains("private surprise"));
        assert!(!hidden.text.contains("example.com"));
        assert!(hidden.text.contains("[spoiler]"));
        assert!(!hidden.text.contains("**"));
        let (revealed, _, _) = preview_job(content, true, Color32::BLACK, &[]);
        assert!(revealed.text.contains("private surprise"));
        assert!(revealed.text.contains("example.com"));
    }
}
