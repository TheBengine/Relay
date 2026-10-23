use super::*;

const ACTIVE_BYTES: usize = 32 * 1024 * 1024;
const IDLE_BYTES: usize = 8 * 1024 * 1024;
const IDLE_AFTER: Duration = Duration::from_secs(60);

impl RelayApp {
    pub(super) fn preview_activity(&mut self, ctx: &egui::Context) {
        if ctx.input(|i| i.focused && !i.events.is_empty()) {
            self.preview_activity_at = Instant::now();
        }
    }
    pub(super) fn trim_preview_cache(&mut self, ctx: &egui::Context) {
        let minimized = ctx.input(|i| i.viewport().minimized.unwrap_or(false));
        PreviewCache {
            attachments: &mut self.chat_media,
            errors: &mut self.chat_media_errors,
            online: &mut self.online_gifs,
            open_original: self.media_preview.as_deref(),
        }
        .trim(ctx, self.preview_activity_at, Instant::now(), minimized);
    }
    pub(super) fn request_original_preview(&mut self, ctx: &egui::Context, id: &str) {
        if ctx.input(|i| i.viewport().minimized.unwrap_or(false))
            || self.chat_media_pending.contains(id)
            || self.chat_media_errors.contains_key(id)
            || !self
                .chat_media
                .get(id)
                .is_some_and(AnimatedImage::is_thumbnail)
        {
            return;
        }
        let attachment = self
            .messages
            .iter()
            .chain(&self.search_messages)
            .flat_map(|message| &message.attachments)
            .find(|attachment| attachment.id == id)
            .cloned();
        let (Some(attachment), Some(origin)) = (attachment, self.command_origin()) else {
            return;
        };
        match self.network.send(Command::FetchChatMedia {
            origin,
            attachment,
            original: true,
        }) {
            Ok(()) => {
                self.chat_media_pending.insert(id.to_owned());
            }
            Err(error) => {
                self.chat_media_errors.insert(id.to_owned(), error);
            }
        }
    }
}

struct PreviewCache<'a> {
    attachments: &'a mut HashMap<String, AnimatedImage>,
    errors: &'a mut HashMap<String, String>,
    online: &'a mut online_gif_ui::GifBrowser,
    open_original: Option<&'a str>,
}
impl PreviewCache<'_> {
    fn trim(self, ctx: &egui::Context, activity_at: Instant, now: Instant, minimized: bool) {
        if minimized {
            for image in self.attachments.values_mut() {
                image.suspend();
            }
            self.online.suspend_message_animations();
        }
        let idle = minimized || now.duration_since(activity_at) >= IDLE_AFTER;
        let target = if idle { IDLE_BYTES } else { ACTIVE_BYTES };
        let mut total = self
            .attachments
            .values()
            .map(AnimatedImage::decoded_bytes)
            .sum::<usize>()
            + self.online.cached_message_bytes();
        while total > target {
            // Under the soft idle target keep previews actually drawn this frame.
            // The hard active budget still applies, except for the open original.
            let keep_visible = idle && total <= ACTIVE_BYTES && !minimized;
            let attachment = self
                .attachments
                .iter()
                .filter(|(id, image)| {
                    self.open_original != Some(id.as_str())
                        && !(keep_visible
                            && image
                                .last_visible()
                                .is_some_and(|at| now.duration_since(at) < Duration::from_secs(1)))
                })
                .min_by_key(|(_, image)| image.last_visible())
                .map(|(id, image)| (id.clone(), image.last_visible()));
            let online = self.online.oldest_cached_message(keep_visible, now);
            let remove_attachment = match (&attachment, &online) {
                (Some((_, a)), Some((_, b))) => a <= b,
                (Some(_), None) => true,
                _ => false,
            };
            let removed = if remove_attachment {
                let (id, last_visible) = attachment.unwrap();
                let removed = self
                    .attachments
                    .remove(&id)
                    .map_or(0, |image| image.decoded_bytes());
                if last_visible.is_some_and(|at| now.duration_since(at) < Duration::from_secs(1))
                    && !minimized
                {
                    self.errors.insert(
                        id,
                        "Preview cache limit reached. Download to view this file.".into(),
                    );
                }
                removed
            } else if let Some((id, last_visible)) = online {
                self.online.evict_cached_message(
                    &id,
                    !minimized
                        && last_visible
                            .is_some_and(|at| now.duration_since(at) < Duration::from_secs(1)),
                )
            } else {
                break;
            };
            total = total.saturating_sub(removed);
        }
        if !idle && total > IDLE_BYTES {
            // One actual transition deadline. Once the idle budget is handled,
            // input/results/animation wake the UI; the cache needs no interval.
            ctx.request_repaint_after(IDLE_AFTER.saturating_sub(now.duration_since(activity_at)));
        }
    }
}

#[cfg(test)]
#[path = "preview_cache_ui/tests.rs"]
mod tests;
