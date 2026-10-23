type LoadedImage = Result<(Vec<u8>, image::RgbaImage), String>;
struct ImagePicker {
    scope: String,
    kind: String,
    name: String,
    loaded: Receiver<LoadedImage>,
    bytes: Vec<u8>,
    image: Option<image::RgbaImage>,
    texture: Option<egui::TextureHandle>,
    zoom: f32,
    x: f32,
    y: f32,
}
/// The Crop Image window: same id and width as before, with the shared dialog
/// header instead of a title bar.
fn crop_window<'a>(ctx: &egui::Context, open: &'a mut bool) -> egui::Window<'a> {
    desktop_ui::dialog(ctx, "Crop Image")
        .id(egui::Id::new("crop-image"))
        .open(open)
        .default_width(480.0)
        .collapsible(false)
}

/// The crop preview and controls. Returns true when Save (or Apply to Profile
/// Draft) was pressed; nothing else changes outside `picker`.
fn crop_controls(ui: &mut egui::Ui, picker: &mut ImagePicker, pending: bool) -> bool {
    let mut save = false;
    ui.add_enabled_ui(!pending, |ui| {
        if let (Some(texture), Some(image)) = (&picker.texture, &picker.image) {
            let aspect = if picker.kind.ends_with("banner") {
                2.5
            } else {
                1.0
            };
            let crop = crop_rectangle(
                image.width(),
                image.height(),
                aspect,
                picker.zoom,
                picker.x,
                picker.y,
            );
            let width = ui.available_width().min(460.0);
            ui.add(
                egui::Image::new((texture.id(), Vec2::new(width, width / aspect)))
                    .uv(Rect::from_min_size(
                        Pos2::new(crop.x, crop.y),
                        Vec2::new(crop.width, crop.height),
                    ))
                    .rounding(if picker.kind == "avatar" || picker.kind == "server_icon" {
                        width * 0.5
                    } else {
                        6.0
                    }),
            );
            ui.add(egui::Slider::new(&mut picker.zoom, 1.0..=8.0).text("Zoom"));
            ui.add(egui::Slider::new(&mut picker.x, 0.0..=1.0).text("Horizontal Position"));
            ui.add(egui::Slider::new(&mut picker.y, 0.0..=1.0).text("Vertical Position"));
            ui.horizontal(|ui| {
                if ui.button("Reset Crop").clicked() {
                    picker.zoom = 1.0;
                    picker.x = 0.5;
                    picker.y = 0.5;
                }
                save = ui
                    .add(primary(if picker.kind == "avatar" {
                        "Apply to Profile Draft"
                    } else {
                        "Save Image"
                    }))
                    .clicked();
            });
        } else {
            ui.spinner();
            ui.label("Loading image…");
        }
    });
    if pending {
        ui.spinner();
        ui.label("Saving image…");
    }
    save
}
fn crop_rectangle(width: u32, height: u32, aspect: f32, zoom: f32, x: f32, y: f32) -> ImageCrop {
    let scale = zoom.clamp(1.0, 8.0);
    let (w, h) = if width as f32 / height as f32 > aspect {
        (height as f32 * aspect / width as f32 / scale, 1.0 / scale)
    } else {
        (1.0 / scale, width as f32 / aspect / height as f32 / scale)
    };
    ImageCrop {
        x: x.clamp(0.0, 1.0) * (1.0 - w),
        y: y.clamp(0.0, 1.0) * (1.0 - h),
        width: w,
        height: h,
    }
}
fn read_crop_image(path: &std::path::Path) -> LoadedImage {
    use std::io::Read;
    let mut bytes = vec![];
    std::fs::File::open(path)
        .map_err(|_| "Cannot open image")?
        .take(2 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "Cannot read image")?;
    if bytes.is_empty() || bytes.len() > 2 * 1024 * 1024 {
        return Err("Choose an image up to 2 MiB.".into());
    }
    let format = image::guess_format(&bytes).map_err(|_| "Choose PNG, JPEG or WebP")?;
    if !matches!(
        format,
        image::ImageFormat::Png | image::ImageFormat::Jpeg | image::ImageFormat::WebP
    ) {
        return Err("Choose a static PNG, JPEG or WebP image.".into());
    }
    let mut reader = image::io::Reader::with_format(std::io::Cursor::new(&bytes), format);
    let mut limits = image::io::Limits::default();
    limits.max_image_width = Some(2048);
    limits.max_image_height = Some(2048);
    limits.max_alloc = Some(32 * 1024 * 1024);
    reader.limits(limits);
    let image = reader
        .decode()
        .map_err(|_| "The image is damaged or exceeds 2048 × 2048 pixels.")?
        .to_rgba8();
    Ok((bytes, image))
}
impl RelayApp {
    pub(super) fn community_assets_pending(&self) -> bool {
        !self.admin_ui.asset_pending.is_empty()
    }

    pub(super) fn expressions_open(&self) -> bool {
        self.admin_ui.expression_picker || self.gif_picker
    }

    pub(super) fn open_expression_picker(&mut self) {
        self.gif_picker = false;
        self.admin_ui.expression_picker = true;
        self.admin_ui.expression_stickers = false;
        self.admin_ui.expression_focus = true;
        self.admin_ui.expression_reaction = None;
    }
    pub(super) fn open_sticker_picker(&mut self) {
        self.open_expression_picker();
        self.admin_ui.expression_stickers = true;
    }

    pub(super) fn open_gif_picker(&mut self) {
        self.admin_ui.expression_picker = false;
        self.admin_ui.expression_reaction = None;
        self.gif_picker = true;
    }

    pub(super) fn choose_crop_image(&mut self, kind: &str, name: &str) {
        let Some(scope) = self.conversation_scope() else {
            return;
        };
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("Static image", &["png", "jpg", "jpeg", "webp"])
            .pick_file()
        {
            let (tx, loaded) = mpsc::sync_channel(1);
            let ctx = self.capture_ctx.clone();
            if let Err(error) = std::thread::Builder::new()
                .name("relay-crop-image".into())
                .spawn(move || {
                    let _ = tx.send(read_crop_image(&path));
                    ctx.request_repaint();
                })
            {
                self.error = Some(error.to_string());
                return;
            }
            self.admin_ui.image_picker = Some(ImagePicker {
                scope,
                kind: kind.into(),
                name: name.into(),
                loaded,
                bytes: vec![],
                image: None,
                texture: None,
                zoom: 1.0,
                x: 0.5,
                y: 0.5,
            });
        }
    }
    /// True while `crop_image_dialog` draws the dialog (its header then owns
    /// Escape): a picker exists for the current conversation scope.
    pub(super) fn crop_dialog_shown(&self) -> bool {
        self.admin_ui.image_picker.as_ref().is_some_and(|picker| {
            self.conversation_scope().as_deref() == Some(picker.scope.as_str())
        })
    }
    pub(super) fn crop_image_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut picker) = self.admin_ui.image_picker.take() else {
            return;
        };
        if self.conversation_scope().as_deref() != Some(&picker.scope) {
            return;
        }
        match picker.loaded.try_recv() {
            Ok(Ok((bytes, image))) => {
                let color = egui::ColorImage::from_rgba_unmultiplied(
                    [image.width() as usize, image.height() as usize],
                    image.as_raw(),
                );
                picker.texture =
                    Some(ctx.load_texture("crop-original", color, egui::TextureOptions::LINEAR));
                picker.bytes = bytes;
                picker.image = Some(image);
            }
            Ok(Err(error)) => {
                self.error = Some(error);
                return;
            }
            _ => {}
        }
        let mut open = true;
        let mut save = false;
        let pending = self.admin_requests.values().any(|k| k == "action:assets");
        let window = crop_window(ctx, &mut open);
        // The shared header (title, Close, Escape) stays above the scrolling
        // crop controls. Closing cancels: the draft is dropped and never saved.
        if host_setup_ui::show_with_header(ctx, window, "Crop Image", |ui| {
            save = crop_controls(ui, &mut picker, pending);
        }) {
            open = false;
            save = false;
        }
        if save {
            let image = picker.image.as_ref().unwrap();
            let aspect = if picker.kind.ends_with("banner") {
                2.5
            } else {
                1.0
            };
            let crop = crop_rectangle(
                image.width(),
                image.height(),
                aspect,
                picker.zoom,
                picker.x,
                picker.y,
            );
            if picker.kind == "avatar" {
                let x = (crop.x * image.width() as f32).floor() as u32;
                let y = (crop.y * image.height() as f32).floor() as u32;
                let w = ((crop.width * image.width() as f32).round() as u32)
                    .max(1)
                    .min(image.width() - x);
                let h = ((crop.height * image.height() as f32).round() as u32)
                    .max(1)
                    .min(image.height() - y);
                let cropped = image::imageops::crop_imm(image, x, y, w, h).to_image();
                let normalized = image::DynamicImage::ImageRgba8(cropped)
                    .thumbnail(256, 256)
                    .to_rgba8();
                let color = egui::ColorImage::from_rgba_unmultiplied(
                    [normalized.width() as usize, normalized.height() as usize],
                    normalized.as_raw(),
                );
                let mut bytes = std::io::Cursor::new(vec![]);
                if let Err(error) = image::DynamicImage::ImageRgba8(normalized)
                    .write_to(&mut bytes, image::ImageOutputFormat::Png)
                {
                    self.error = Some(error.to_string());
                } else {
                    self.apply_cropped_avatar(bytes.into_inner(), color);
                    open = false;
                }
            } else {
                use base64::Engine;
                self.admin_request(AdminMethod::Put,format!("/api/assets/{}/{}",picker.kind,picker.name),json!({"image_base64":base64::engine::general_purpose::STANDARD.encode(&picker.bytes),"crop":crop}),"action:assets".into());
            }
        }
        if open {
            self.admin_ui.image_picker = Some(picker);
        }
    }
    pub(super) fn asset_texture(&mut self, kind: &str, name: &str) -> Option<egui::TextureHandle> {
        let asset = self
            .bootstrap
            .as_ref()?
            .assets
            .iter()
            .find(|a| a.kind == kind && a.name == name)?
            .clone();
        let key = crate::network::asset_attachment_id(&asset);
        if let Some((texture, at)) = self.admin_ui.asset_cache.get_mut(&key) {
            *at = Instant::now();
            return Some(texture.clone());
        }
        if self.admin_ui.asset_pending.len() < 4
            && !self.admin_ui.asset_pending.contains(&key)
            && !self.admin_ui.asset_failed.contains(&key)
            && self.network.send(Command::FetchAsset { asset }).is_ok()
        {
            self.admin_ui.asset_pending.insert(key);
        }
        None
    }
    pub(super) fn inline_community_emoji(
        &mut self,
        content: &str,
        revealed: bool,
    ) -> Vec<crate::emoji::CustomEmoji> {
        if !content.contains("<emoji:") {
            return Vec::new();
        }
        let visible = visible_expression_text(content, revealed);
        let assets = self
            .bootstrap
            .as_ref()
            .map(|b| b.assets.clone())
            .unwrap_or_default();
        assets
            .into_iter()
            .filter(|asset| asset.kind == "emoji")
            .filter_map(|asset| {
                let token = format!("<emoji:{}:{}>", asset.name, asset.id);
                visible.iter().any(|text| text.contains(&token)).then(|| {
                    crate::emoji::CustomEmoji {
                        token,
                        name: asset.name.clone(),
                        texture: self.asset_texture(&asset.kind, &asset.name),
                    }
                })
            })
            .collect()
    }
    pub(super) fn fetched_asset(
        &mut self,
        ctx: &egui::Context,
        key: String,
        media: crate::chat_media::DecodedMedia,
    ) {
        if !self.admin_ui.asset_pending.remove(&key) {
            return;
        }
        if media.frames.len() != 1 {
            self.admin_ui.asset_failed.insert(key);
            return;
        }
        let frame = &media.frames[0];
        if frame.image.size[0] > 1200 || frame.image.size[1] > 480 {
            self.admin_ui.asset_failed.insert(key);
            return;
        }
        while self.admin_ui.asset_cache.len() >= 32 {
            let old = self
                .admin_ui
                .asset_cache
                .iter()
                .min_by_key(|(_, (_, at))| at)
                .map(|(key, _)| key.clone())
                .unwrap();
            self.admin_ui.asset_cache.remove(&old);
        }
        self.admin_ui.asset_cache.insert(
            key.clone(),
            (
                ctx.load_texture(key, frame.image.clone(), egui::TextureOptions::LINEAR),
                Instant::now(),
            ),
        );
    }
    pub(super) fn failed_asset(&mut self, key: String) {
        self.admin_ui.asset_pending.remove(&key);
        if self.admin_ui.asset_failed.len() >= 128 {
            self.admin_ui.asset_failed.clear();
        }
        self.admin_ui.asset_failed.insert(key);
    }
    pub(super) fn presentation_images_ui(&mut self, ui: &mut egui::Ui, profile: bool) {
        let Some(b) = self.bootstrap.clone() else {
            return;
        };
        if !b.capabilities.iter().any(|c| c == "assets-v1") {
            return;
        }
        let choices = if profile {
            vec![("profile_banner", b.user.id.clone(), "Profile Banner")]
        } else {
            vec![
                ("server_icon", "community".into(), "Server Icon"),
                ("server_banner", "community".into(), "Server Banner"),
            ]
        };
        for (kind, name, label) in choices {
            ui.push_id(kind, |ui| {
                ui.label(RichText::new(label).strong());
                if let Some(texture) = self.asset_texture(kind, &name) {
                    let size = if kind.ends_with("banner") {
                        Vec2::new(
                            ui.available_width().min(480.0),
                            ui.available_width().min(480.0) / 2.5,
                        )
                    } else {
                        Vec2::splat(72.0)
                    };
                    ui.add(egui::Image::new((texture.id(), size)).rounding(6.0));
                }
                ui.add_enabled_ui(profile || b.has_permission(PERMISSION_ADMIN), |ui| {
                    ui.horizontal(|ui| {
                        if ui.button("Upload / Crop").clicked() {
                            self.choose_crop_image(kind, &name);
                        }
                        if ui.button("Remove Image").clicked() {
                            self.admin_request(
                                AdminMethod::Delete,
                                format!("/api/assets/{kind}/{name}"),
                                Value::Null,
                                "action:assets".into(),
                            );
                        }
                    });
                });
                ui.add_space(10.0);
            });
        }
    }
    pub(super) fn expressions_settings(&mut self, ui: &mut egui::Ui) {
        crate::settings_ui::page_heading(
            ui,
            "Emoji & Stickers",
            "Static community images, available to your members.",
        );
        let Some(b) = self.bootstrap.clone() else {
            return;
        };
        ui.add(
            egui::TextEdit::singleline(&mut self.admin_ui.expression_name)
                .char_limit(32)
                .hint_text("Image name: letters, numbers and underscore"),
        );
        let valid = (2..=32).contains(&self.admin_ui.expression_name.len())
            && self
                .admin_ui
                .expression_name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_');
        ui.add_enabled_ui(valid, |ui| {
            ui.horizontal(|ui| {
                for (kind, label) in [("emoji", "Upload Emoji"), ("sticker", "Upload Sticker")] {
                    if ui.button(label).clicked() {
                        self.choose_crop_image(kind, &self.admin_ui.expression_name.clone());
                    }
                }
            });
        });
        for asset in b
            .assets
            .iter()
            .filter(|a| matches!(a.kind.as_str(), "emoji" | "sticker"))
        {
            ui.push_id(&asset.id, |ui| {
                ui.horizontal(|ui| {
                    if let Some(texture) = self.asset_texture(&asset.kind, &asset.name) {
                        ui.add(egui::Image::new((texture.id(), Vec2::splat(48.0))));
                    }
                    ui.label(format!("{} · {}", asset.name, asset.kind));
                    if ui.button("Replace").clicked() {
                        self.choose_crop_image(&asset.kind, &asset.name);
                    }
                    if ui.button("Remove").clicked() {
                        self.admin_request(
                            AdminMethod::Delete,
                            format!("/api/assets/{}/{}", asset.kind, asset.name),
                            Value::Null,
                            "action:assets".into(),
                        );
                    }
                });
            });
        }
    }
    pub(super) fn expressions_dialog(&mut self, ctx: &egui::Context) {
        if !self.admin_ui.expression_picker && !self.gif_picker {
            return;
        }
        let Some(b) = self.bootstrap.clone() else {
            return;
        };
        if self.modal_open() {
            self.admin_ui.expression_picker = false;
            self.admin_ui.expression_reaction = None;
            self.gif_picker = false;
            self.finish_gif_picker(ctx);
            return;
        }
        let was_gif = self.gif_picker;
        let mut open = true;
        let mut chosen = None;
        let mut unicode = None;
        let mut online_chosen = None;
        let bounds = emoji_ui::expression_panel_rect(ctx.screen_rect(), self.composer_anchor);
        let inner_size = (bounds.size() - Vec2::splat(21.0)).max(Vec2::splat(1.0));
        let panel = egui::Window::new("Expressions")
            .id(egui::Id::new("expression-picker"))
            .title_bar(false)
            .movable(false)
            .resizable(false)
            .fixed_pos(bounds.min)
            .fixed_size(inner_size)
            .constrain_to(ctx.screen_rect().shrink(8.0))
            .vscroll(false)
            .frame(
                egui::Frame::window(&ctx.style())
                    .fill(c_card())
                    .inner_margin(Margin::same(10.0)),
            )
            .show(ctx, |ui| {
                // Keep the panel anchored when a search returns only a few items.
                ui.set_min_size(inner_size);
                ui.horizontal(|ui| {
                    if self.admin_ui.expression_reaction.is_none() {
                        if ui.selectable_label(self.gif_picker, "GIFs").clicked() {
                            self.open_gif_picker();
                        }
                        if ui
                            .selectable_label(
                                !self.gif_picker && self.admin_ui.expression_stickers,
                                "Stickers",
                            )
                            .clicked()
                        {
                            self.open_sticker_picker();
                        }
                    }
                    if ui
                        .selectable_label(
                            !self.gif_picker && !self.admin_ui.expression_stickers,
                            "Emoji",
                        )
                        .clicked()
                    {
                        let reaction = self.admin_ui.expression_reaction.take();
                        self.open_expression_picker();
                        self.admin_ui.expression_reaction = reaction;
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if icon_button(ui, Icon::Close, false, "Close expressions · Escape")
                            .clicked()
                        {
                            open = false;
                        }
                    });
                });
                ui.add_space(6.0);
                ui.separator();
                ui.add_space(6.0);
                let body_height =
                    (inner_size.y - (ui.cursor().top() - ui.min_rect().top())).max(1.0);
                egui::ScrollArea::vertical()
                    .id_source("expression-panel-body")
                    .max_height(body_height)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.set_max_height(body_height);
                        if self.gif_picker {
                            online_chosen = self.gif_picker_content(ui);
                            return;
                        }
                        let kind = if self.admin_ui.expression_stickers {
                            "sticker"
                        } else {
                            "emoji"
                        };
                        if !self.admin_ui.expression_stickers {
                            let query = self
                                .admin_ui
                                .expression_query
                                .trim()
                                .trim_matches(':')
                                .to_lowercase();
                            unicode = self.standard_emoji_picker(
                                ui,
                                if self.admin_ui.expression_reaction.is_none()
                                    && b.assets.iter().any(|asset| {
                                        asset.kind == "emoji"
                                            && asset.name.to_lowercase().contains(&query)
                                    })
                                {
                                    85.0
                                } else {
                                    0.0
                                },
                            );
                        } else {
                            let search = ui.add(
                                egui::TextEdit::singleline(&mut self.admin_ui.expression_query)
                                    .hint_text("Search stickers")
                                    .desired_width(f32::INFINITY),
                            );
                            if std::mem::take(&mut self.admin_ui.expression_focus) {
                                search.request_focus();
                            }
                        }
                        if self.admin_ui.expression_reaction.is_none() {
                            let query = self
                                .admin_ui
                                .expression_query
                                .trim()
                                .trim_matches(':')
                                .to_lowercase();
                            let assets: Vec<_> = b
                                .assets
                                .iter()
                                .filter(|asset| {
                                    asset.kind == kind && asset.name.to_lowercase().contains(&query)
                                })
                                .collect();
                            if !assets.is_empty() {
                                ui.add_space(8.0);
                                ui.label(
                                    RichText::new(if kind == "emoji" {
                                        "Community emoji"
                                    } else {
                                        &b.server_name
                                    })
                                    .color(c_muted()),
                                );
                                egui::ScrollArea::vertical()
                                    .id_source("community-expressions")
                                    .max_height(if kind == "emoji" {
                                        88.0
                                    } else {
                                        ui.available_height().max(80.0)
                                    })
                                    .show(ui, |ui| {
                                        ui.horizontal_wrapped(|ui| {
                                            for asset in assets {
                                                ui.push_id(&asset.id, |ui| {
                                                    if let Some(texture) =
                                                        self.asset_texture(&asset.kind, &asset.name)
                                                    {
                                                        let response = ui
                                                            .add(
                                                                egui::ImageButton::new((
                                                                    texture.id(),
                                                                    Vec2::splat(
                                                                        if kind == "emoji" {
                                                                            30.0
                                                                        } else {
                                                                            88.0
                                                                        },
                                                                    ),
                                                                ))
                                                                .frame(false),
                                                            )
                                                            .on_hover_text(&asset.name);
                                                        response.widget_info(|| {
                                                            egui::WidgetInfo::labeled(
                                                                egui::WidgetType::Button,
                                                                &asset.name,
                                                            )
                                                        });
                                                        if response.clicked() {
                                                            chosen = Some(asset.clone());
                                                        }
                                                    } else {
                                                        ui.add(egui::Button::new(&asset.name))
                                                            .on_hover_text("Loading image…");
                                                    }
                                                });
                                            }
                                        });
                                    });
                            } else if kind == "sticker" {
                                ui.add_space(20.0);
                                ui.label(if query.is_empty() {
                                    "This community has no stickers yet."
                                } else {
                                    "No stickers found. Try another name."
                                });
                            }
                        }
                    });
            });
        if let Some(panel) = panel {
            if ctx.input(|input| {
                input.pointer.any_pressed()
                    && input.pointer.interact_pos().is_some_and(|position| {
                        !panel.response.rect.contains(position)
                            && !self
                                .composer_anchor
                                .is_some_and(|anchor| anchor.contains(position))
                    })
            }) {
                open = false;
            }
        }
        if let Some(asset) = chosen {
            self.insert_composer_text(
                ctx,
                &format!(" <{}:{}:{}> ", asset.kind, asset.name, asset.id),
            );
            open = false;
        }
        if let Some(index) = unicode {
            self.remember_emoji(index);
            if let Some((origin, target, message)) = self.admin_ui.expression_reaction.take() {
                if self.command_origin().as_ref() == Some(&origin)
                    && self.current.as_ref() == Some(&target)
                {
                    self.react_to_message(
                        &message,
                        &crate::emoji::catalog().entries[index].unicode,
                    );
                }
            } else {
                self.insert_composer_text(ctx, &crate::emoji::catalog().entries[index].unicode);
            }
            self.defer_preferences();
            ctx.memory_mut(|memory| {
                memory.request_focus(egui::Id::new(("composer-editor", self.current.clone())))
            });
            open = false;
        }
        if let Some(link) = online_chosen {
            if self.send_online_gif(link) {
                open = false;
            }
        }
        self.gif_picker &= open;
        self.admin_ui.expression_picker &= open;
        if !self.gif_picker && (was_gif || self.gif_previews.is_some()) {
            self.finish_gif_picker(ctx);
        }
        if !open {
            self.admin_ui.expression_reaction = None;
            ctx.memory_mut(|memory| {
                memory.request_focus(egui::Id::new(("composer-editor", self.current.clone())))
            });
        }
    }
    pub(super) fn expression_message_view(
        &mut self,
        ui: &mut egui::Ui,
        content: &str,
        revealed: bool,
    ) {
        let Some(b) = self.bootstrap.clone() else {
            return;
        };
        let visible = visible_expression_text(content, revealed);
        for asset in b.assets.iter().filter(|a| a.kind == "sticker") {
            let token = format!("<{}:{}:{}>", asset.kind, asset.name, asset.id);
            if visible.iter().any(|text| text.contains(&token)) {
                if let Some(texture) = self.asset_texture(&asset.kind, &asset.name) {
                    ui.add(egui::Image::new((
                        texture.id(),
                        Vec2::splat(if asset.kind == "sticker" { 160.0 } else { 32.0 }),
                    )))
                    .on_hover_text(&asset.name);
                }
            }
        }
    }
}
fn visible_expression_text(content: &str, revealed: bool) -> Vec<String> {
    crate::message_format::parse(content)
        .into_iter()
        .filter(|span| !span.style.code && (revealed || !span.style.spoiler))
        .map(|span| span.text)
        .collect()
}
#[cfg(test)]
mod crop_tests {
    use super::*;
    #[test]
    fn community_images_obey_spoilers_and_literal_code() {
        let token = "<emoji:squad:owned>";
        for content in [
            format!("||{token}||"),
            format!("`{token}`"),
            format!("```\n{token}\n```"),
        ] {
            assert!(!visible_expression_text(&content, false)
                .iter()
                .any(|text| text.contains(token)));
        }
        assert!(visible_expression_text(&format!("||{token}||"), true)
            .iter()
            .any(|text| text.contains(token)));
        assert!(!visible_expression_text(&format!("`{token}`"), true)
            .iter()
            .any(|text| text.contains(token)));
    }
    #[test]
    fn rectangles_remain_inside_original_for_all_aspects_positions_and_zooms() {
        for (w, h) in [(1, 2048), (2048, 1), (100, 100), (1600, 900)] {
            for aspect in [1.0, 2.5] {
                for zoom in [1.0, 2.0, 8.0] {
                    for x in [0.0, 0.5, 1.0] {
                        for y in [0.0, 0.5, 1.0] {
                            let crop = crop_rectangle(w, h, aspect, zoom, x, y);
                            assert!(crop.valid());
                            assert!(
                                (crop.width * w as f32 / (crop.height * h as f32) - aspect).abs()
                                    < 0.01
                            );
                        }
                    }
                }
            }
        }
    }
}
#[cfg(test)]
mod welcome_crop_header_tests {
    use super::*;

    fn key(key: egui::Key) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: Some(key),
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }
    }

    fn click(at: Pos2) -> Vec<egui::Event> {
        let press = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        vec![egui::Event::PointerMoved(at), press(true), press(false)]
    }

    /// A synthetic banner picker; no file, disk or picker dialog involved.
    fn picker(ctx: &egui::Context) -> ImagePicker {
        let image = image::RgbaImage::from_pixel(1000, 400, image::Rgba([40, 90, 160, 255]));
        let texture = ctx.load_texture(
            "synthetic-crop",
            egui::ColorImage::from_rgba_unmultiplied([1000, 400], image.as_raw()),
            Default::default(),
        );
        ImagePicker {
            scope: "synthetic-scope".into(),
            kind: "server_banner".into(),
            name: "community".into(),
            loaded: std::sync::mpsc::channel().1,
            bytes: vec![],
            image: Some(image),
            texture: Some(texture),
            zoom: 1.0,
            x: 0.5,
            y: 0.5,
        }
    }

    struct Frame {
        welcome_close: bool,
        welcome_header: Rect,
        welcome_action: Rect,
        welcome_clip: Rect,
        crop_close: bool,
        crop_save: bool,
        crop_body: Rect,
    }

    impl Default for Frame {
        fn default() -> Self {
            Frame {
                welcome_close: false,
                welcome_header: Rect::NOTHING,
                welcome_action: Rect::NOTHING,
                welcome_clip: Rect::NOTHING,
                crop_close: false,
                crop_save: false,
                crop_body: Rect::NOTHING,
            }
        }
    }

    /// One frame of the real welcome window (tall banner, long description)
    /// and, optionally, the real Crop Image window opened on top of it.
    fn frame(
        ctx: &egui::Context,
        size: Vec2,
        crop: Option<&mut ImagePicker>,
        events: Vec<egui::Event>,
    ) -> Frame {
        let banner = ctx.load_texture(
            "synthetic-tall-banner",
            egui::ColorImage::new([10, 1000], Color32::BLUE),
            Default::default(),
        );
        let description = "A very long community description. ".repeat(300);
        let mut out = Frame::default();
        let _ = ctx.run(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size)),
                focused: true,
                events,
                ..Default::default()
            },
            |ctx| {
                let mut open = true;
                let title = "Welcome to Synthetic community";
                welcome_window(ctx, title, &mut open).show(ctx, |ui| {
                    let top = ui.cursor().top();
                    out.welcome_close = profile_ui::dialog_header_ui::dialog_header(ui, title);
                    out.welcome_header =
                        Rect::from_x_y_ranges(ui.max_rect().x_range(), top..=ui.cursor().top());
                    welcome_content(ui, "Synthetic community", &description, Some(banner));
                    ui.horizontal_wrapped(|ui| {
                        ui.add(primary("Continue"));
                        out.welcome_action = ui.add(secondary("Open #general")).rect;
                        out.welcome_clip = ui.clip_rect();
                    });
                });
                if let Some(picker) = crop {
                    let mut open = true;
                    let window = crop_window(ctx, &mut open);
                    out.crop_close =
                        host_setup_ui::show_with_header(ctx, window, "Crop Image", |ui| {
                            // While the body fits, egui clips it to the whole window; its
                            // own maximum rectangle starts below the header.
                            out.crop_body = ui.max_rect();
                            out.crop_save = crop_controls(ui, picker, false);
                        });
                }
            },
        );
        out
    }

    fn settle(ctx: &egui::Context, size: Vec2, mut crop: Option<&mut ImagePicker>) -> Frame {
        let mut last = Frame::default();
        for _ in 0..4 {
            last = frame(ctx, size, crop.as_deref_mut(), vec![]);
        }
        last
    }

    fn context(font_scale: f32) -> egui::Context {
        let ctx = egui::Context::default();
        // Relay's fonts (the dialog header uses its semibold face).
        configure(&ctx);
        crate::appearance::AppearanceSettings {
            font_scale,
            ..Default::default()
        }
        .apply(&ctx, None);
        ctx
    }

    const CASES: [(Vec2, f32); 4] = [
        (Vec2::new(940.0, 650.0), 1.0),
        (Vec2::new(940.0, 650.0), 1.4),
        (Vec2::new(360.0, 420.0), 1.0),
        (Vec2::new(360.0, 420.0), 1.4),
    ];

    #[test]
    fn welcome_header_closes_with_close_or_escape_and_keeps_its_actions_visible() {
        for (size, scale) in CASES {
            let case = format!("{size:?} x{scale}");
            let ctx = context(scale);
            let settled = settle(&ctx, size, None);
            let window = ctx
                .memory(|m| m.area_rect(egui::Id::new("community-welcome")))
                .unwrap();
            let screen = Rect::from_min_size(Pos2::ZERO, size);
            assert!(screen.contains_rect(window), "{case}: {window:?}");
            assert!(
                settled.welcome_clip.contains_rect(settled.welcome_action),
                "{case}: actions stay visible"
            );
            // Scrolling the welcome text leaves the header where it is.
            let over_body = egui::Event::PointerMoved(Pos2::new(
                window.center().x,
                settled.welcome_header.bottom() + 40.0,
            ));
            for _ in 0..6 {
                frame(
                    &ctx,
                    size,
                    None,
                    vec![
                        over_body.clone(),
                        egui::Event::Scroll(Vec2::new(0.0, -400.0)),
                    ],
                );
            }
            let scrolled = frame(&ctx, size, None, vec![]);
            assert_eq!(scrolled.welcome_header, settled.welcome_header, "{case}");
            let close = Pos2::new(
                settled.welcome_header.right() - 15.0,
                settled.welcome_header.top() + 15.0,
            );
            assert!(window.contains(close), "{case}: Close on the dialog");
            assert!(
                frame(&ctx, size, None, click(close)).welcome_close,
                "{case}: Close"
            );
            let ctx = context(scale);
            settle(&ctx, size, None);
            assert!(!frame(&ctx, size, None, vec![key(egui::Key::Enter)]).welcome_close);
            assert!(
                frame(&ctx, size, None, vec![key(egui::Key::Escape)]).welcome_close,
                "{case}: Escape"
            );
        }
    }

    #[test]
    fn crop_close_and_escape_cancel_without_saving_and_only_the_top_dialog_closes() {
        for (size, scale) in CASES {
            let case = format!("{size:?} x{scale}");
            let ctx = context(scale);
            let mut draft = picker(&ctx);
            let settled = settle(&ctx, size, Some(&mut draft));
            let window = ctx
                .memory(|m| m.area_rect(egui::Id::new("crop-image")))
                .unwrap();
            let screen = Rect::from_min_size(Pos2::ZERO, size);
            assert!(screen.contains_rect(window), "{case}: {window:?}");
            // Close sits at the right end of the fixed header above the
            // scrolling controls. The header row spans the same content width
            // as the body's own rectangle; the window can be wider than that
            // when the body shows a scroll bar (360x420 at 140 %), so the
            // window's edge is not where Close is.
            let close = Pos2::new(
                settled.crop_body.right() - 15.0,
                (window.top() + settled.crop_body.top()) / 2.0,
            );
            let clicked = frame(&ctx, size, Some(&mut draft), click(close));
            assert!(
                clicked.crop_close && !clicked.crop_save,
                "{case}: Close cancels (click {close:?}, window {window:?}, body {:?}, welcome {:?})",
                settled.crop_body,
                ctx.memory(|m| m.area_rect(egui::Id::new("community-welcome")))
            );
            assert!(
                !clicked.welcome_close,
                "{case}: the welcome below stays open"
            );
            // Escape belongs to the Crop dialog on top: it cancels the crop,
            // never saves, and the welcome below keeps its own Escape.
            let ctx = context(scale);
            let mut draft = picker(&ctx);
            settle(&ctx, size, Some(&mut draft));
            let escaped = frame(&ctx, size, Some(&mut draft), vec![key(egui::Key::Escape)]);
            assert!(
                escaped.crop_close && !escaped.crop_save,
                "{case}: Escape cancels"
            );
            assert!(!escaped.welcome_close, "{case}: only the top dialog closes");
            // With Crop gone, the welcome is on top and Escape closes it.
            for _ in 0..3 {
                frame(&ctx, size, None, vec![]);
            }
            assert!(
                frame(&ctx, size, None, vec![key(egui::Key::Escape)]).welcome_close,
                "{case}: then the welcome"
            );
            // The crop draft itself is untouched by closing.
            assert_eq!((draft.zoom, draft.x, draft.y), (1.0, 0.5, 0.5), "{case}");
        }
    }
}
