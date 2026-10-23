use super::*;
use crate::online_gifs::{self, Gif, Loader, Request, ResultItem};

#[cfg(test)]
#[path = "online_gif_ui/tests.rs"]
mod tests;

fn giphy_attribution(ui: &mut egui::Ui) {
    let bytes: &[u8] = if ui.visuals().dark_mode {
        include_bytes!("../assets/giphy-dark.png")
    } else {
        include_bytes!("../assets/giphy-light.png")
    };
    let id = egui::Id::new(("giphy-attribution", ui.visuals().dark_mode));
    let texture = ui
        .ctx()
        .data(|data| data.get_temp::<egui::TextureHandle>(id))
        .unwrap_or_else(|| {
            let image = image::load_from_memory(bytes)
                .expect("bundled GIPHY attribution")
                .into_rgba8();
            let texture = ui.ctx().load_texture(
                "Powered By GIPHY",
                egui::ColorImage::from_rgba_unmultiplied(
                    [image.width() as usize, image.height() as usize],
                    image.as_raw(),
                ),
                egui::TextureOptions::LINEAR,
            );
            ui.ctx()
                .data_mut(|data| data.insert_temp(id, texture.clone()));
            texture
        });
    let response = ui.add(
        egui::Image::new((
            texture.id(),
            texture.size_vec2() * (120.0 / texture.size_vec2().x),
        ))
        .sense(Sense::click()),
    );
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Link, "Powered By GIPHY"));
    if response.clicked() {
        ui.ctx()
            .open_url(egui::OpenUrl::new_tab("https://giphy.com"));
    }
}

fn gif_link_menu(ui: &mut egui::Ui, link: &str) {
    if ui.button("Copy GIF link").clicked() {
        ui.output_mut(|output| output.copied_text = link.to_owned());
        ui.close_menu();
    }
    if ui.button("Open original").clicked() {
        ui.ctx().open_url(egui::OpenUrl::new_tab(link));
        ui.close_menu();
    }
}

fn gif_preview_actions(
    response: &egui::Response,
    id: &str,
    link: &str,
    preview: &mut Option<(String, String)>,
) {
    response
        .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, "Open GIF preview"));
    if response.clicked() {
        *preview = Some((id.to_owned(), link.to_owned()));
    }
    response.context_menu(|ui| {
        if ui.button("View GIF").clicked() {
            *preview = Some((id.to_owned(), link.to_owned()));
            ui.close_menu();
        }
        gif_link_menu(ui, link);
    });
}

pub(super) fn message_context_menu(
    ui: &mut egui::Ui,
    response: &egui::Response,
    media: &[Rect],
    mut contents: impl FnMut(&mut egui::Ui),
) {
    if media.is_empty() {
        response.context_menu(contents);
        return;
    }
    // egui upgrades a context-menu response to Sense::click. Registering the
    // whole row after its images would put it in front of their click targets.
    let mut regions = vec![response.rect];
    for cutout in media {
        let mut remaining = Vec::new();
        for region in regions {
            let cutout = cutout.expand(1.0).intersect(region);
            if !cutout.is_positive() {
                remaining.push(region);
                continue;
            }
            remaining.extend(
                [
                    Rect::from_min_max(region.min, Pos2::new(region.right(), cutout.top())),
                    Rect::from_min_max(Pos2::new(region.left(), cutout.bottom()), region.max),
                    Rect::from_min_max(
                        Pos2::new(region.left(), cutout.top()),
                        Pos2::new(cutout.left(), cutout.bottom()),
                    ),
                    Rect::from_min_max(
                        Pos2::new(cutout.right(), cutout.top()),
                        Pos2::new(region.right(), cutout.bottom()),
                    ),
                ]
                .into_iter()
                .filter(Rect::is_positive),
            );
        }
        regions = remaining;
    }
    for (index, rect) in regions.into_iter().enumerate() {
        ui.interact(
            rect,
            response.id.with(("around-media", index)),
            Sense::click(),
        )
        .context_menu(&mut contents);
    }
}

#[derive(Default)]
pub(super) struct GifBrowser {
    key: String,
    loader: Option<Loader>,
    offline: bool,
    connection: online_gifs::ConnectionCheck,
    favorites: bool,
    query: String,
    generation: u64,
    offset: usize,
    changed_at: Option<Instant>,
    busy: bool,
    initialized: bool,
    error: Option<String>,
    gifs: Vec<Gif>,
    more: bool,
    previews: HashMap<String, egui::TextureHandle>,
    preview_pending: HashSet<String>,
    preview_errors: HashSet<String>,
    hover: Option<(String, AnimatedImage)>,
    hover_pending: HashSet<String>,
    hover_errors: HashSet<String>,
    messages: HashMap<String, AnimatedImage>,
    message_pending: HashSet<String>,
    message_errors: HashMap<String, String>,
    message_preview: Option<(String, String)>,
    scope: Option<(CommandOrigin, Option<Target>)>,
}

impl GifBrowser {
    pub(super) fn message_preview_open(&self) -> bool {
        self.message_preview.is_some()
    }

    pub(super) fn close_message_preview(&mut self) -> bool {
        self.message_preview.take().is_some()
    }

    fn clear_message_cache(&mut self) {
        self.close_message_preview();
        self.messages.clear();
        self.message_pending.clear();
        self.message_errors.clear();
    }

    pub(super) fn suspend_message_animations(&mut self) {
        for image in self.messages.values_mut() {
            image.suspend();
        }
    }
    pub(super) fn cached_message_bytes(&self) -> usize {
        self.messages
            .values()
            .map(AnimatedImage::decoded_bytes)
            .sum()
    }
    pub(super) fn oldest_cached_message(
        &self,
        keep_visible: bool,
        now: Instant,
    ) -> Option<(String, Option<Instant>)> {
        self.messages
            .iter()
            .filter(|(_, image)| {
                !keep_visible
                    || !image
                        .last_visible()
                        .is_some_and(|at| now.duration_since(at) < Duration::from_secs(1))
            })
            .filter(|(id, _)| {
                self.message_preview
                    .as_ref()
                    .is_none_or(|(url, _)| url != *id)
            })
            .min_by_key(|(_, image)| image.last_visible())
            .map(|(id, image)| (id.clone(), image.last_visible()))
    }
    pub(super) fn evict_cached_message(&mut self, id: &str, visible: bool) -> usize {
        let bytes = self
            .messages
            .remove(id)
            .map_or(0, |image| image.decoded_bytes());
        if visible {
            self.message_errors.insert(
                id.to_owned(),
                "Load this GIF again to continue viewing it.".into(),
            );
        }
        bytes
    }

    #[cfg(test)]
    pub(super) fn cached_fixture(
        entries: Vec<(String, AnimatedImage)>,
        preview: Option<&str>,
    ) -> Self {
        Self {
            messages: entries.into_iter().collect(),
            message_preview: preview.map(|url| (url.into(), "Fixture original".into())),
            ..Default::default()
        }
    }

    pub(super) fn close_picker(&mut self) {
        self.invalidate();
        self.changed_at = None;
        self.release_idle_loader();
    }

    fn release_idle_loader(&mut self) {
        // Cached frames need no HTTP worker. Keep requests for the current
        // conversation alive until their results have been consumed.
        if self.message_pending.is_empty() {
            self.loader = None;
        }
    }

    fn configure(&mut self, local_key: &str) {
        let key = online_gifs::configured_key(local_key);
        if self.key != key {
            self.key = key;
            self.loader = None;
            self.clear_message_cache();
            self.invalidate();
        }
    }

    fn ensure(&mut self, local_key: &str, ctx: &egui::Context) {
        self.configure(local_key);
        if self.loader.is_none() && !self.key.is_empty() {
            self.loader = Some(Loader::new(self.key.clone(), ctx.clone()));
        }
    }

    fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        if let Some(loader) = &self.loader {
            loader.invalidate(self.generation);
        }
        self.changed_at = Some(Instant::now());
        self.busy = false;
        self.error = None;
        self.gifs.clear();
        self.previews.clear();
        self.initialized = false;
        self.preview_pending.clear();
        self.preview_errors.clear();
        self.hover = None;
        self.hover_pending.clear();
        self.hover_errors.clear();
    }

    fn poll(&mut self, ctx: &egui::Context) {
        let Some(loader) = &self.loader else {
            return;
        };
        for item in loader.rx.try_iter() {
            match item {
                ResultItem::Hover(generation, id, result) if generation == self.generation => {
                    self.hover_pending.remove(&id);
                    match result {
                        Ok(media) => self.hover = Some((id, AnimatedImage::new(media))),
                        Err(_) => {
                            self.hover_errors.insert(id);
                        }
                    }
                }
                ResultItem::Browse(generation, result) if generation == self.generation => {
                    self.busy = false;
                    self.initialized = true;
                    match result {
                        Ok(page) => {
                            self.gifs = page.gifs;
                            self.more = page.more;
                        }
                        Err(error) => self.error = Some(error),
                    }
                }
                ResultItem::Preview(generation, id, result) if generation == self.generation => {
                    self.preview_pending.remove(&id);
                    match result {
                        Ok(media) => {
                            if let Some(frame) = media.frames.first() {
                                if frame.image.size[0].saturating_mul(frame.image.size[1])
                                    > 256 * 256
                                {
                                    self.preview_errors.insert(id);
                                    continue;
                                }
                                self.previews.insert(
                                    id.clone(),
                                    ctx.load_texture(
                                        format!("giphy-preview-{id}"),
                                        frame.image.clone(),
                                        egui::TextureOptions::LINEAR,
                                    ),
                                );
                            }
                        }
                        Err(_) => {
                            self.preview_errors.insert(id);
                        }
                    }
                }
                ResultItem::Message(id, result) if self.message_pending.remove(&id) => match result
                {
                    Ok(media) => {
                        while self.messages.len() >= 4
                            || self
                                .messages
                                .values()
                                .map(AnimatedImage::decoded_bytes)
                                .sum::<usize>()
                                + media.decoded_bytes
                                > 32 * 1024 * 1024
                        {
                            let Some(oldest) = self
                                .messages
                                .iter()
                                .min_by_key(|(_, image)| image.last_used())
                                .map(|(id, _)| id.clone())
                            else {
                                break;
                            };
                            self.messages.remove(&oldest);
                            self.message_errors.insert(
                                oldest,
                                "Load this GIF again to continue viewing it.".into(),
                            );
                        }
                        self.messages.insert(id, AnimatedImage::new(media));
                    }
                    Err(error) => {
                        self.message_errors.insert(id, error);
                    }
                },
                _ => {}
            }
        }
    }
}

impl RelayApp {
    pub(super) fn local_gif_tile(&mut self, ui: &mut egui::Ui, path: &PathBuf) -> egui::Response {
        let (rect, response) = ui.allocate_exact_size(Vec2::new(144.0, 100.0), Sense::click());
        let hovered = response.hovered() && ui.is_rect_visible(rect);
        let animate = hovered
            && !self.prefs.reduced_motion
            && self.prefs.appearance.gif_autoplay
            && ui
                .ctx()
                .input(|input| input.focused && !input.viewport().minimized.unwrap_or(false));
        let cached = self
            .gif_hover
            .as_ref()
            .is_some_and(|(cached, _)| cached == path);
        if animate
            && !cached
            && self.gif_hover_pending.is_none()
            && !self.gif_hover_errors.contains(path)
            && self
                .gif_previews
                .as_ref()
                .is_some_and(|loader| loader.request_animation(path.clone()))
        {
            self.gif_hover_pending = Some(path.clone());
        }
        let image = if animate && cached {
            self.gif_hover.as_mut().map(|(_, image)| {
                (
                    image.texture(ui.ctx(), hovered, self.prefs.reduced_motion),
                    image.dimensions(),
                )
            })
        } else {
            self.gif_textures
                .get(path)
                .map(|texture| (texture.id(), texture.size_vec2()))
        };
        ui.painter().rect_filled(rect, 6.0, c_panel());
        if let Some((texture, dimensions)) = image {
            let scale = (rect.width() / dimensions.x).min(rect.height() / dimensions.y);
            ui.painter().image(
                texture,
                Rect::from_center_size(rect.center(), dimensions * scale),
                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                Color32::WHITE,
            );
        } else {
            if ui.is_rect_visible(rect)
                && !self.gif_pending.contains(path)
                && !self.gif_errors.contains_key(path)
                && self.gif_textures.len() < 64
                && self
                    .gif_previews
                    .as_ref()
                    .is_some_and(|loader| loader.request(path.clone()))
            {
                self.gif_pending.insert(path.clone());
            }
            ui.painter().text(
                rect.center(),
                Align2::CENTER_CENTER,
                "GIF",
                FontId::proportional(font_size(14.0)),
                c_muted(),
            );
        }
        if hovered || response.has_focus() {
            ui.painter()
                .rect_stroke(rect, 6.0, Stroke::new(2.0_f32, c_accent()));
        }
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, format!("Send GIF: {name}"))
        });
        response.on_hover_text(format!("Send {name}"))
    }

    pub(super) fn start_gif_upload(&mut self, target: Target, path: PathBuf) {
        let Some(origin) = self.command_origin() else {
            return;
        };
        if let Some(id) = self.queue_upload(target.clone(), path) {
            self.gif_upload_sends.insert(id, (origin, target));
        }
    }

    pub(super) fn send_gif_attachment(&mut self, attachment: Attachment) -> bool {
        let Some(target) = &self.current else {
            return false;
        };
        if self.pending_sends.contains(target)
            || self.cooldowns.remaining(target, Instant::now()).is_some()
        {
            return false;
        }
        let saved = std::mem::take(&mut self.draft);
        let attachments = std::mem::replace(&mut self.attachments, vec![attachment]);
        self.submit_composer();
        self.draft = saved;
        self.attachments = attachments;
        self.defer_preferences();
        self.current
            .as_ref()
            .is_some_and(|target| self.pending_sends.contains(target))
    }

    pub(super) fn online_gifs_tick(&mut self, ctx: &egui::Context) {
        self.online_gifs.connection.poll(&self.prefs.giphy_api_key);
        self.online_gifs.poll(ctx);
        let scope = self
            .command_origin()
            .map(|origin| (origin, self.current.clone()));
        if self.online_gifs.scope != scope {
            self.online_gifs.scope = scope;
            self.online_gifs.clear_message_cache();
        }
        if !self.gif_picker || self.online_gifs.offline {
            self.online_gifs.release_idle_loader();
        }
    }

    pub(super) fn online_gif_tabs(&mut self, ui: &mut egui::Ui) -> bool {
        let was_offline = self.online_gifs.offline;
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.online_gifs.offline, false, "Discover GIFs");
            ui.selectable_value(&mut self.online_gifs.offline, true, "From this PC");
        });
        if was_offline != self.online_gifs.offline {
            self.online_gifs.offset = 0;
            self.online_gifs.invalidate();
        }
        !self.online_gifs.offline
    }

    pub(super) fn online_gif_setup(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("GIPHY application key").color(c_muted()));
        if ui
            .add(
                egui::TextEdit::singleline(&mut self.prefs.giphy_api_key)
                    .id_source("giphy-application-key")
                    .password(true)
                    .char_limit(256)
                    .hint_text("Paste your API key here")
                    .desired_width(ui.available_width().min(420.0)),
            )
            .changed()
        {
            self.defer_preferences();
        }
        self.online_gifs.connection.poll(&self.prefs.giphy_api_key);
        let has_key = !online_gifs::configured_key(&self.prefs.giphy_api_key).is_empty();
        let checking = self.online_gifs.connection.state == online_gifs::ConnectionState::Checking;
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(has_key && !checking, secondary("Test connection"))
                .clicked()
            {
                self.online_gifs
                    .connection
                    .start(&self.prefs.giphy_api_key, ui.ctx());
            }
            if !self.prefs.giphy_api_key.is_empty() && ui.button("Clear local key").clicked() {
                self.prefs.giphy_api_key.clear();
                self.online_gifs.connection.poll(&self.prefs.giphy_api_key);
                self.defer_preferences();
            }
        });
        match &self.online_gifs.connection.state {
            online_gifs::ConnectionState::Checking => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Checking GIPHY…");
                });
                ui.ctx().request_repaint_after(Duration::from_millis(100));
            }
            online_gifs::ConnectionState::Ready => {
                ui.label(RichText::new("Connected. Online GIF search is ready.").color(c_good()));
            }
            online_gifs::ConnectionState::Failed(error) => {
                ui.label(RichText::new(error).color(c_bad()));
            }
            online_gifs::ConnectionState::Idle => {
                ui.label(
                    RichText::new(if has_key {
                        "An application key is available. Test it to check the connection."
                    } else {
                        "An application key is needed to search online."
                    })
                    .color(c_muted()),
                );
            }
        }
        ui.add_space(6.0);
        ui.hyperlink_to(
            "Create a GIPHY API key",
            "https://developers.giphy.com/dashboard/",
        );
        ui.add(egui::Label::new(RichText::new("Choose API in the developer dashboard, create a key for Relay, paste it above and test the connection. Your key is saved only on this PC. A key included in a Relay release works without this local setup.").color(c_muted())).wrap(true));
        ui.add(egui::Label::new(RichText::new("Searches and previews connect directly to GIPHY. GIF links in chat use Relay's normal message encryption.").color(c_muted())).wrap(true));
    }

    pub(super) fn online_gif_picker(&mut self, ui: &mut egui::Ui) -> Option<String> {
        self.online_gifs.ensure(&self.prefs.giphy_api_key, ui.ctx());
        let browser = &mut self.online_gifs;
        let mut chosen = None;
        ui.add_space(8.0);
        if browser.key.is_empty() {
            ui.label("Online GIF search hasn't been set up for this app yet.");
            ui.label(RichText::new("You can still send GIFs from your computer.").color(c_muted()));
            if ui.button("GIF setup").clicked() {
                self.gif_picker = false;
                self.settings_open = true;
                self.settings_tab = SettingsTab::Advanced;
            }
            return None;
        }
        let previous_favorites = browser.favorites;
        ui.horizontal(|ui| {
            ui.selectable_value(&mut browser.favorites, false, "Explore");
            ui.selectable_value(&mut browser.favorites, true, "Favorites");
        });
        if previous_favorites != browser.favorites {
            browser.offset = 0;
            browser.invalidate();
        }
        if !browser.favorites
            && ui
                .add(
                    egui::TextEdit::singleline(&mut browser.query)
                        .char_limit(50)
                        .hint_text("Search GIPHY")
                        .desired_width(f32::INFINITY),
                )
                .changed()
        {
            browser.offset = 0;
            browser.invalidate();
        }
        if !browser.initialized
            && browser.changed_at.is_none()
            && !browser.busy
            && browser.error.is_none()
        {
            browser.changed_at = Some(Instant::now());
        }
        if let Some(changed) = browser.changed_at {
            if changed.elapsed() >= Duration::from_millis(350)
                && browser.loader.as_ref().is_some_and(|loader| {
                    loader.request(Request::Browse {
                        generation: browser.generation,
                        query: browser.query.clone(),
                        offset: browser.offset,
                        favorites: browser
                            .favorites
                            .then(|| self.prefs.giphy_favorites.clone()),
                    })
                })
            {
                browser.changed_at = None;
                browser.busy = true;
            }
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }
        if browser.busy {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Finding GIFs…");
            });
        }
        if let Some(error) = &browser.error {
            ui.label(RichText::new(error).color(c_bad()));
            if ui.button("Try again").clicked() {
                browser.invalidate();
            }
        } else if !browser.busy && browser.changed_at.is_none() && browser.gifs.is_empty() {
            ui.label(if browser.favorites {
                "Favorite a GIF to keep it here."
            } else {
                "No GIFs found. Try another search."
            });
        }
        let mut preferences_changed = false;
        giphy_attribution(ui);
        egui::ScrollArea::vertical()
            .id_source("online-gif-results")
            .max_height((ui.available_height() - font_size(36.0)).max(1.0))
            .show(ui, |ui| {
                let columns = ((ui.available_width() + 8.0) / 164.0).floor().max(1.0) as usize;
                egui::Grid::new("online-gif-grid")
                    .num_columns(columns)
                    .spacing(Vec2::splat(8.0))
                    .show(ui, |ui| {
                        for (index, gif) in browser.gifs.iter().enumerate() {
                            egui::Frame::none()
                                .fill(c_card())
                                .rounding(8.0)
                                .inner_margin(Margin::same(6.0))
                                .show(ui, |ui| {
                                    ui.vertical(|ui| {
                                        ui.set_width(144.0);
                                        let (rect, response) = ui.allocate_exact_size(
                                            Vec2::new(144.0, 110.0),
                                            Sense::click(),
                                        );
                                        let visible = ui.is_rect_visible(rect);
                                        let hovered = response.hovered() && visible;
                                        let animate = hovered
                                            && !self.prefs.reduced_motion
                                            && self.prefs.appearance.gif_autoplay
                                            && ui.ctx().input(|input| {
                                                input.focused
                                                    && !input.viewport().minimized.unwrap_or(false)
                                            });
                                        let cached = browser
                                            .hover
                                            .as_ref()
                                            .is_some_and(|(id, _)| id == &gif.id);
                                        if animate
                                            && !cached
                                            && browser.hover_pending.is_empty()
                                            && !browser.hover_errors.contains(&gif.id)
                                            && browser.loader.as_ref().is_some_and(|loader| {
                                                loader.request(Request::Hover {
                                                    generation: browser.generation,
                                                    id: gif.id.clone(),
                                                    url: gif.preview.clone(),
                                                })
                                            })
                                        {
                                            browser.hover_pending.insert(gif.id.clone());
                                        }
                                        let image = if animate && cached {
                                            browser.hover.as_mut().map(|(_, image)| {
                                                (
                                                    image.texture(
                                                        ui.ctx(),
                                                        hovered,
                                                        self.prefs.reduced_motion,
                                                    ),
                                                    image.dimensions(),
                                                )
                                            })
                                        } else {
                                            browser
                                                .previews
                                                .get(&gif.id)
                                                .map(|texture| (texture.id(), texture.size_vec2()))
                                        };
                                        ui.painter().rect_filled(rect, 6.0, c_panel());
                                        if let Some((texture, dimensions)) = image {
                                            let scale = (rect.width() / dimensions.x)
                                                .min(rect.height() / dimensions.y);
                                            ui.painter().image(
                                                texture,
                                                Rect::from_center_size(
                                                    rect.center(),
                                                    dimensions * scale,
                                                ),
                                                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                                                Color32::WHITE,
                                            );
                                        } else {
                                            if visible
                                                && !browser.preview_pending.contains(&gif.id)
                                                && !browser.preview_errors.contains(&gif.id)
                                                && browser.loader.as_ref().is_some_and(|loader| {
                                                    loader.request(Request::Preview {
                                                        generation: browser.generation,
                                                        id: gif.id.clone(),
                                                        url: gif.still.clone(),
                                                    })
                                                })
                                            {
                                                browser.preview_pending.insert(gif.id.clone());
                                            }
                                            ui.painter().text(
                                                rect.center(),
                                                Align2::CENTER_CENTER,
                                                "GIF",
                                                FontId::proportional(font_size(16.0)),
                                                c_muted(),
                                            );
                                        }
                                        if hovered || response.has_focus() {
                                            ui.painter().rect_stroke(
                                                rect,
                                                6.0,
                                                Stroke::new(2.0_f32, c_accent()),
                                            );
                                        }
                                        response.widget_info(|| {
                                            egui::WidgetInfo::labeled(
                                                egui::WidgetType::Button,
                                                format!("Send GIF: {}", gif.title),
                                            )
                                        });
                                        if response.on_hover_text(&gif.title).clicked() {
                                            chosen = Some(gif.link());
                                        }
                                        ui.horizontal(|ui| {
                                            if ui.small_button("Send").clicked() {
                                                chosen = Some(gif.link());
                                            }
                                            let favorite =
                                                self.prefs.giphy_favorites.contains(&gif.id);
                                            if ui
                                                .small_button(if favorite {
                                                    "Unfavorite"
                                                } else {
                                                    "Favorite"
                                                })
                                                .clicked()
                                            {
                                                if favorite {
                                                    self.prefs
                                                        .giphy_favorites
                                                        .retain(|id| id != &gif.id);
                                                } else if self.prefs.giphy_favorites.len() < 50 {
                                                    self.prefs.giphy_favorites.push(gif.id.clone());
                                                }
                                                preferences_changed = true;
                                            }
                                        });
                                    });
                                });
                            if (index + 1) % columns == 0 {
                                ui.end_row();
                            }
                        }
                    });
            });
        ui.horizontal(|ui| {
            if ui
                .add_enabled(browser.offset > 0 && !browser.busy, secondary("Previous"))
                .clicked()
            {
                browser.offset = browser.offset.saturating_sub(20);
                browser.invalidate();
            }
            if ui
                .add_enabled(
                    browser.more
                        && !browser.busy
                        && browser.offset < if browser.query.is_empty() { 480 } else { 4980 },
                    secondary("Next"),
                )
                .clicked()
            {
                browser.offset += 20;
                browser.invalidate();
            }
        });
        if preferences_changed {
            self.defer_preferences();
        }
        chosen
    }

    pub(super) fn send_online_gif(&mut self, link: String) -> bool {
        let Some(target) = &self.current else {
            return false;
        };
        if self.pending_sends.contains(target)
            || self.cooldowns.remaining(target, Instant::now()).is_some()
        {
            self.error = Some("Wait for your current message before sending this GIF.".into());
            return false;
        }
        let saved = std::mem::replace(&mut self.draft, link);
        let attachments = std::mem::take(&mut self.attachments);
        self.submit_composer();
        self.draft = saved;
        self.attachments = attachments;
        self.defer_preferences();
        self.current
            .as_ref()
            .is_some_and(|target| self.pending_sends.contains(target))
    }

    pub(super) fn online_gif_message(&mut self, ui: &mut egui::Ui, content: &str) -> bool {
        self.online_gifs.draw_message(ui, content, &self.prefs)
    }

    pub(super) fn online_gif_preview_dialog(&mut self, ctx: &egui::Context) {
        let Some((id, link)) = self.online_gifs.message_preview.clone() else {
            return;
        };
        let Some(image) = self.online_gifs.messages.get_mut(&id) else {
            self.online_gifs.message_preview = None;
            return;
        };
        let mut open = true;
        let window = desktop_ui::dialog(ctx, "GIF preview")
            .open(&mut open)
            .collapsible(false)
            .resizable(false);
        // The shared header (title, Close, Escape) stays above the scrolling body.
        if host_setup_ui::show_with_header(ctx, window, "GIF preview", |ui| {
            let dimensions = image.dimensions();
            let scale = ((ctx.screen_rect().width() - 120.0).min(800.0) / dimensions.x)
                .min((ctx.screen_rect().height() - 160.0) / dimensions.y)
                .min(2.0);
            let texture = image.texture_with_autoplay(
                ctx,
                true,
                self.prefs.reduced_motion,
                self.prefs.appearance.gif_autoplay,
            );
            ui.add(
                egui::Image::new((texture, dimensions * scale))
                    .rounding(8.0)
                    .sense(Sense::click()),
            )
            .context_menu(|ui| gif_link_menu(ui, &link));
            giphy_attribution(ui);
        }) {
            open = false;
        }
        if !open {
            self.online_gifs.close_message_preview();
        }
    }
}

impl GifBrowser {
    pub(super) fn draw_blocked_message(
        &mut self,
        ui: &mut egui::Ui,
        message_id: &str,
        content: &str,
        prefs: &Preferences,
    ) {
        egui::CollapsingHeader::new("Blocked message")
            .id_source(("blocked-message", message_id))
            .show(ui, |ui| {
                if !self.draw_message(ui, content, prefs) {
                    ui.label(content);
                }
            });
    }

    pub(super) fn draw_message(
        &mut self,
        ui: &mut egui::Ui,
        content: &str,
        prefs: &Preferences,
    ) -> bool {
        let Some(id) = online_gifs::message_id(content) else {
            return false;
        };
        if !prefs.online_gif_previews || ui.ctx().input(|i| i.viewport().minimized.unwrap_or(false))
        {
            ui.hyperlink_to("Open GIF", content.trim())
                .context_menu(|ui| gif_link_menu(ui, content.trim()));
            return true;
        }
        self.configure(&prefs.giphy_api_key);
        if self.key.is_empty() {
            ui.hyperlink_to("Open GIF", content.trim())
                .context_menu(|ui| gif_link_menu(ui, content.trim()));
            return true;
        }
        if let Some(image) = self.messages.get_mut(&id) {
            let dimensions = image.dimensions();
            let scale = (ui.available_width().min(320.0) / dimensions.x)
                .min(240.0 / dimensions.y)
                .min(1.5);
            let (rect, response) = ui.allocate_exact_size(dimensions * scale, Sense::click());
            let texture = image.texture_with_autoplay(
                ui.ctx(),
                ui.is_rect_visible(rect),
                prefs.reduced_motion,
                prefs.appearance.gif_autoplay,
            );
            ui.painter().image(
                texture,
                rect,
                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                Color32::WHITE,
            );
            gif_preview_actions(&response, &id, content.trim(), &mut self.message_preview);
            giphy_attribution(ui);
        } else if let Some(error) = self.message_errors.get(&id) {
            ui.add(egui::Label::new(RichText::new(error).color(c_muted())).sense(Sense::click()))
                .context_menu(|ui| gif_link_menu(ui, content.trim()));
            if ui.small_button("Retry GIF").clicked() {
                self.message_errors.remove(&id);
            }
        } else {
            if !self.message_pending.contains(&id) {
                self.ensure(&prefs.giphy_api_key, ui.ctx());
                if self
                    .loader
                    .as_ref()
                    .is_some_and(|loader| loader.request(Request::Message { id: id.clone() }))
                {
                    self.message_pending.insert(id);
                }
            }
            ui.spinner()
                .interact(Sense::click())
                .context_menu(|ui| gif_link_menu(ui, content.trim()));
        }
        true
    }
}
