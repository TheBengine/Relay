use super::*;

fn worker_exit_probe(browser: &mut GifBrowser) -> std::sync::mpsc::Receiver<()> {
    std::mem::replace(
        &mut browser.loader.as_mut().unwrap().worker_exits,
        std::sync::mpsc::channel().1,
    )
}

fn workers_stopped(exits: std::sync::mpsc::Receiver<()>) {
    for _ in 0..2 {
        exits.recv_timeout(Duration::from_secs(3)).unwrap();
    }
    assert!(matches!(
        exits.recv_timeout(Duration::from_secs(3)),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
    ));
}

#[test]
fn closed_picker_workers_exit_and_reopened_picker_gets_a_result() {
    let ctx = egui::Context::default();
    let mut browser = GifBrowser::default();
    browser.ensure("fixture-key-not-sent", &ctx);
    let exits = worker_exit_probe(&mut browser);
    browser.close_picker();
    assert!(browser.loader.is_none());
    workers_stopped(exits);

    browser.ensure("fixture-key-not-sent", &ctx);
    // Empty favorites uses the real worker without an external HTTP request.
    assert!(browser.loader.as_ref().unwrap().request(Request::Browse {
        generation: browser.generation,
        query: String::new(),
        offset: 0,
        favorites: Some(vec![]),
    }));
    let deadline = Instant::now() + Duration::from_secs(3);
    while !browser.initialized && Instant::now() < deadline {
        browser.poll(&ctx);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(browser.initialized && browser.error.is_none() && browser.gifs.is_empty());
    let exits = worker_exit_probe(&mut browser);
    browser.close_picker();
    workers_stopped(exits);
}

#[test]
fn closing_picker_preserves_pending_message_then_releases_workers() {
    let ctx = egui::Context::default();
    let mut browser = GifBrowser::default();
    browser.ensure("fixture-key-not-sent", &ctx);
    let exits = worker_exit_probe(&mut browser);
    let (results, rx) = std::sync::mpsc::channel();
    browser.loader.as_mut().unwrap().rx = rx;
    browser.message_pending.insert("pending-message".into());
    browser.close_picker();
    assert!(browser.loader.is_some());
    assert!(matches!(
        exits.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    ));
    results
        .send(ResultItem::Message(
            "pending-message".into(),
            Ok(crate::chat_media::DecodedMedia {
                source_size: [2, 2],
                frames: vec![crate::chat_media::DecodedFrame {
                    image: std::sync::Arc::new(egui::ColorImage::new([2, 2], Color32::WHITE)),
                    delay: Duration::from_millis(100),
                }],
                decoded_bytes: 16,
            }),
        ))
        .unwrap();
    browser.poll(&ctx);
    assert!(browser.message_pending.is_empty());
    assert!(browser.messages.contains_key("pending-message"));
    browser.release_idle_loader();
    assert!(browser.loader.is_none());
    workers_stopped(exits);

    let output = ctx.run(egui::RawInput::default(), |ctx| {
        browser.configure("fixture-key-not-sent");
        let image = browser.messages.get_mut("pending-message").unwrap();
        assert_eq!(image.dimensions(), Vec2::new(2.0, 2.0));
        let _ = image.texture_with_autoplay(ctx, true, true, false);
    });
    assert!(!output.textures_delta.set.is_empty());
    assert!(browser.loader.is_none());
    assert_eq!(browser.cached_message_bytes(), 16);
}

#[test]
fn conversation_cache_clear_does_not_keep_old_message_workers_alive() {
    let ctx = egui::Context::default();
    let mut browser = GifBrowser::default();
    browser.ensure("fixture-key-not-sent", &ctx);
    let exits = worker_exit_probe(&mut browser);
    browser.message_pending.insert("old-conversation".into());
    browser.clear_message_cache();
    browser.release_idle_loader();
    assert!(browser.loader.is_none() && browser.message_pending.is_empty());
    workers_stopped(exits);
}

#[test]
fn provider_key_change_clears_pending_state_and_stops_old_workers() {
    let ctx = egui::Context::default();
    let mut browser = GifBrowser::default();
    browser.ensure("fixture-key-not-sent", &ctx);
    let exits = worker_exit_probe(&mut browser);
    browser.message_pending.insert("old-key".into());
    browser.message_preview = Some(("old-key".into(), LINK.into()));
    browser.configure("replacement-fixture-key-not-sent");
    assert!(browser.loader.is_none() && browser.message_pending.is_empty());
    assert!(!browser.message_preview_open());
    workers_stopped(exits);
}

const LINK: &str = "https://giphy.com/gifs/fixture55";

fn pointer(pos: Pos2, button: egui::PointerButton) -> Vec<egui::Event> {
    vec![
        egui::Event::PointerMoved(pos),
        egui::Event::PointerButton {
            pos,
            button,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        },
        egui::Event::PointerButton {
            pos,
            button,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        },
    ]
}

fn click(
    ctx: &egui::Context,
    preview: &mut Option<(String, String)>,
    pos: Pos2,
    button: egui::PointerButton,
    loaded: bool,
) -> (egui::FullOutput, Rect) {
    frame(ctx, preview, vec![egui::Event::PointerMoved(pos)], loaded);
    let mut events = pointer(pos, button);
    let release = events.pop().unwrap();
    frame(ctx, preview, events, loaded);
    frame(ctx, preview, vec![release], loaded)
}

fn frame(
    ctx: &egui::Context,
    preview: &mut Option<(String, String)>,
    events: Vec<egui::Event>,
    loaded: bool,
) -> (egui::FullOutput, Rect) {
    let mut rect = Rect::NOTHING;
    let output = ctx.run(
        egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(640.0, 480.0))),
            focused: true,
            events,
            ..Default::default()
        },
        |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let message = egui::Frame::none().show(ui, |ui| {
                    ui.label("Author · Today");
                    let response = if loaded {
                        ui.allocate_exact_size(Vec2::new(240.0, 160.0), Sense::click())
                            .1
                    } else {
                        ui.hyperlink_to("Open GIF", LINK)
                    };
                    rect = response.rect;
                    if loaded {
                        gif_preview_actions(&response, "fixture55", LINK, preview);
                    } else {
                        response.context_menu(|ui| gif_link_menu(ui, LINK));
                    }
                });
                // Match the actual enclosing message's independent context menu.
                message_context_menu(ui, &message.response, &[rect], |ui| {
                    let _ = ui.button("Reply to message");
                });
            });
        },
    );
    (output, rect)
}

fn text_rect(shape: &egui::Shape, label: &str) -> Option<Rect> {
    match shape {
        egui::Shape::Text(text) if text.galley.job.text == label => {
            Some(Rect::from_min_size(text.pos, text.galley.size()))
        }
        egui::Shape::Vec(shapes) => shapes.iter().find_map(|shape| text_rect(shape, label)),
        _ => None,
    }
}

fn button(output: &egui::FullOutput, label: &str) -> Option<Pos2> {
    output
        .shapes
        .iter()
        .find_map(|shape| text_rect(&shape.shape, label).map(|rect| rect.center()))
}

fn menu(
    ctx: &egui::Context,
    preview: &mut Option<(String, String)>,
    loaded: bool,
) -> egui::FullOutput {
    let (_, rect) = frame(ctx, preview, vec![], loaded);
    click(
        ctx,
        preview,
        rect.center(),
        egui::PointerButton::Secondary,
        loaded,
    );
    frame(ctx, preview, vec![], loaded).0
}

#[test]
fn left_click_opens_preview_without_browser_or_clipboard_action() {
    let ctx = egui::Context::default();
    let mut preview = None;
    let (_, rect) = frame(&ctx, &mut preview, vec![], true);
    let (output, _) = click(
        &ctx,
        &mut preview,
        rect.center(),
        egui::PointerButton::Primary,
        true,
    );
    assert_eq!(preview, Some(("fixture55".into(), LINK.into())));
    assert!(output.platform_output.copied_text.is_empty());
    assert!(output.platform_output.open_url.is_none());
}

#[test]
fn gif_menu_wins_over_enclosing_message_and_copy_only_copies_the_link() {
    let ctx = egui::Context::default();
    let mut preview = None;
    let output = menu(&ctx, &mut preview, true);
    assert!(button(&output, "View GIF").is_some());
    assert!(button(&output, "Open original").is_some());
    assert!(button(&output, "Reply to message").is_none());
    assert!(preview.is_none());
    let pos = button(&output, "Copy GIF link").expect("GIF menu was not drawn");
    let (output, _) = click(&ctx, &mut preview, pos, egui::PointerButton::Primary, true);
    assert_eq!(output.platform_output.copied_text, LINK);
    assert!(output.platform_output.open_url.is_none());
    assert!(preview.is_none());
    let output = frame(&ctx, &mut preview, vec![], true).0;
    assert!(
        button(&output, "Copy GIF link").is_none(),
        "menu must close after Copy"
    );
}

#[test]
fn open_original_emits_only_the_public_gif_url_without_opening_preview() {
    let ctx = egui::Context::default();
    let mut preview = None;
    let output = menu(&ctx, &mut preview, true);
    let pos = button(&output, "Open original").unwrap();
    let (output, _) = click(&ctx, &mut preview, pos, egui::PointerButton::Primary, true);
    let open = output.platform_output.open_url.unwrap();
    assert_eq!(open.url, LINK);
    assert!(open.new_tab);
    assert!(output.platform_output.copied_text.is_empty());
    assert!(preview.is_none());
}

#[test]
fn view_menu_opens_preview_and_unloaded_gif_still_has_link_actions() {
    let ctx = egui::Context::default();
    let mut preview = None;
    let output = menu(&ctx, &mut preview, true);
    let pos = button(&output, "View GIF").unwrap();
    let (output, _) = click(&ctx, &mut preview, pos, egui::PointerButton::Primary, true);
    assert_eq!(preview, Some(("fixture55".into(), LINK.into())));
    assert!(output.platform_output.copied_text.is_empty());
    assert!(output.platform_output.open_url.is_none());

    let ctx = egui::Context::default();
    let mut preview = None;
    let output = menu(&ctx, &mut preview, false);
    assert!(button(&output, "View GIF").is_none());
    let pos = button(&output, "Copy GIF link").unwrap();
    let (output, _) = click(&ctx, &mut preview, pos, egui::PointerButton::Primary, false);
    assert_eq!(output.platform_output.copied_text, LINK);
    assert!(output.platform_output.open_url.is_none());
}

#[test]
fn clearing_message_scope_discards_preview_pending_media_and_cached_frame() {
    let mut browser = GifBrowser {
        message_preview: Some(("fixture55".into(), LINK.into())),
        ..Default::default()
    };
    let image = std::sync::Arc::new(egui::ColorImage::new([2, 2], Color32::LIGHT_BLUE));
    browser.messages.insert(
        "fixture55".into(),
        AnimatedImage::new(crate::chat_media::DecodedMedia {
            source_size: [2, 2],
            decoded_bytes: 16,
            frames: vec![crate::chat_media::DecodedFrame {
                image,
                delay: Duration::from_millis(100),
            }],
        }),
    );
    browser.message_pending.insert("pending".into());
    browser
        .message_errors
        .insert("old-error".into(), "Unavailable".into());
    assert!(browser.message_preview_open());
    browser.clear_message_cache();
    assert!(!browser.message_preview_open());
    assert!(browser.messages.is_empty());
    assert!(browser.message_pending.is_empty());
    assert!(browser.message_errors.is_empty());
}

#[test]
fn message_menu_still_opens_outside_the_image() {
    let ctx = egui::Context::default();
    let mut preview = None;
    let (_, rect) = frame(&ctx, &mut preview, vec![], true);
    click(
        &ctx,
        &mut preview,
        Pos2::new(rect.center().x, rect.top() - 10.0),
        egui::PointerButton::Secondary,
        true,
    );
    let output = frame(&ctx, &mut preview, vec![], true).0;
    assert!(button(&output, "Reply to message").is_some());
    assert!(button(&output, "View GIF").is_none());
    assert!(preview.is_none());
    assert!(output.platform_output.copied_text.is_empty());
    assert!(output.platform_output.open_url.is_none());
}

struct ReactionClickFixture {
    clicks: usize,
    pill: Rect,
    author: Rect,
}

impl Default for ReactionClickFixture {
    fn default() -> Self {
        Self {
            clicks: 0,
            pill: Rect::NOTHING,
            author: Rect::NOTHING,
        }
    }
}

impl ReactionClickFixture {
    fn frame(&mut self, ctx: &egui::Context, events: Vec<egui::Event>) -> egui::FullOutput {
        ctx.run(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(640.0, 480.0))),
                focused: true,
                events,
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let row = egui::Frame::none().show(ui, |ui| {
                        self.author = ui.label("Author · Today").rect;
                        ui.label("A message with an existing reaction");
                        let response = crate::emoji::reaction_button(
                            ui,
                            "<3",
                            2,
                            Color32::DARK_GRAY,
                            Stroke::new(1.0_f32, Color32::GRAY),
                        );
                        self.pill = response.rect;
                        self.clicks += usize::from(response.clicked());
                    });
                    // The production row registers its context menu after its
                    // controls; their rectangles must remain independent targets.
                    message_context_menu(ui, &row.response, &[self.pill], |ui| {
                        let _ = ui.button("Reply to message");
                    });
                });
            },
        )
    }

    fn click(
        &mut self,
        ctx: &egui::Context,
        pos: Pos2,
        button: egui::PointerButton,
    ) -> egui::FullOutput {
        self.frame(ctx, vec![egui::Event::PointerMoved(pos)]);
        let mut events = pointer(pos, button);
        let release = events.pop().unwrap();
        self.frame(ctx, events);
        self.frame(ctx, vec![release])
    }
}

#[test]
fn reaction_pill_receives_both_primary_clicks_with_enclosing_message_context_menu() {
    let ctx = egui::Context::default();
    configure(&ctx);
    let mut fixture = ReactionClickFixture::default();
    fixture.frame(&ctx, vec![]);
    for expected in [1, 2] {
        let pos = fixture.pill.center();
        let output = fixture.click(&ctx, pos, egui::PointerButton::Primary);
        assert_eq!(
            fixture.clicks, expected,
            "The row must not swallow the reaction click"
        );
        assert!(button(&output, "Reply to message").is_none());
    }
}

#[test]
fn enclosing_message_menu_remains_accessible_around_the_reaction_pill() {
    let ctx = egui::Context::default();
    configure(&ctx);
    let mut fixture = ReactionClickFixture::default();
    fixture.frame(&ctx, vec![]);
    let pos = fixture.author.center();
    fixture.click(&ctx, pos, egui::PointerButton::Secondary);
    let output = fixture.frame(&ctx, vec![]);
    assert!(button(&output, "Reply to message").is_some());
    assert_eq!(fixture.clicks, 0);
}

struct DeliveryFixture {
    browser: GifBrowser,
    prefs: Preferences,
    texture: egui::TextureId,
}

impl DeliveryFixture {
    fn new(ctx: &egui::Context) -> Self {
        configure(ctx);
        let prefs = Preferences {
            giphy_api_key: "outbox-fixture-not-sent".into(),
            reduced_motion: true,
            ..Default::default()
        };
        let mut browser = GifBrowser::default();
        browser.configure(&prefs.giphy_api_key);
        let mut bytes = Vec::new();
        {
            let mut encoder = image::codecs::gif::GifEncoder::new(&mut bytes);
            for color in [[10, 30, 60, 255], [80, 20, 40, 255]] {
                encoder
                    .encode_frame(image::Frame::from_parts(
                        image::RgbaImage::from_pixel(64, 32, image::Rgba(color)),
                        0,
                        0,
                        image::Delay::from_numer_denom_ms(100, 1),
                    ))
                    .unwrap();
            }
        }
        let media = crate::chat_media::decode(&bytes).unwrap();
        assert_eq!(media.frames.len(), 2);
        let mut image = AnimatedImage::new(media);
        let texture = image.texture_with_autoplay(ctx, true, true, false);
        browser.messages.insert("fixture55".into(), image);
        Self {
            browser,
            prefs,
            texture,
        }
    }

    fn frame(
        &mut self,
        ctx: &egui::Context,
        content: &str,
        confirmed: bool,
        minimized: bool,
        events: Vec<egui::Event>,
    ) -> egui::FullOutput {
        assert!(
            self.browser.messages.contains_key("fixture55")
                || self.browser.message_pending.contains("fixture55")
                || self.browser.message_errors.contains_key("fixture55")
        );
        assert_eq!(
            self.browser.key,
            online_gifs::configured_key(&self.prefs.giphy_api_key),
            "Changing a key would clear the test cache and start a real loader"
        );
        let mut input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(640.0, 480.0))),
            focused: true,
            events,
            ..Default::default()
        };
        input
            .viewports
            .get_mut(&egui::ViewportId::ROOT)
            .unwrap()
            .minimized = Some(minimized);
        let output = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                crate::app::message_preview_ui::render_delivery_content(
                    ui,
                    content,
                    &[],
                    (Color32::RED, confirmed),
                    &mut self.browser,
                    &self.prefs,
                );
            });
        });
        assert!(
            self.browser.loader.is_none(),
            "No external HTTP worker in UI test"
        );
        output
    }

    fn click(
        &mut self,
        ctx: &egui::Context,
        pos: Pos2,
        button: egui::PointerButton,
    ) -> egui::FullOutput {
        self.frame(
            ctx,
            LINK,
            false,
            false,
            vec![egui::Event::PointerMoved(pos)],
        );
        let mut events = pointer(pos, button);
        let release = events.pop().unwrap();
        self.frame(ctx, LINK, false, false, events);
        self.frame(ctx, LINK, false, false, vec![release])
    }
}

fn painted_text(shape: &egui::Shape, text: &mut Vec<String>) {
    match shape {
        egui::Shape::Text(shape) => text.push(shape.galley.job.text.clone()),
        egui::Shape::Vec(shapes) => shapes.iter().for_each(|shape| painted_text(shape, text)),
        _ => {}
    }
}

fn delivery_text(output: &egui::FullOutput) -> Vec<String> {
    let mut text = Vec::new();
    for shape in &output.shapes {
        painted_text(&shape.shape, &mut text);
    }
    text
}

fn texture_rect(shape: &egui::Shape, texture: egui::TextureId) -> Option<Rect> {
    match shape {
        egui::Shape::Mesh(mesh) if mesh.texture_id == texture => {
            let mut rect = Rect::NOTHING;
            for vertex in &mesh.vertices {
                rect.extend_with(vertex.pos);
            }
            Some(rect)
        }
        egui::Shape::Vec(shapes) => shapes.iter().find_map(|shape| texture_rect(shape, texture)),
        _ => None,
    }
}

#[test]
fn outbox_gif_paints_image_without_raw_url_and_keeps_click_and_copy_menu() {
    for confirmed in [false, true] {
        let ctx = egui::Context::default();
        let mut f = DeliveryFixture::new(&ctx);
        let output = f.frame(&ctx, LINK, confirmed, false, vec![]);
        assert!(delivery_text(&output)
            .iter()
            .all(|text| !text.contains("giphy.com")));
        let rect = output
            .shapes
            .iter()
            .find_map(|shape| texture_rect(&shape.shape, f.texture))
            .expect("The actual outbox body must paint the decoded GIF");
        f.click(&ctx, rect.center(), egui::PointerButton::Primary);
        assert_eq!(
            f.browser.message_preview,
            Some(("fixture55".into(), LINK.into()))
        );
        f.browser.close_message_preview();
        f.click(&ctx, rect.center(), egui::PointerButton::Secondary);
        let output = f.frame(&ctx, LINK, false, false, vec![]);
        let pos = button(&output, "Copy GIF link").expect("Outbox GIF context menu");
        assert!(button(&output, "View GIF").is_some());
        assert!(button(&output, "Open original").is_some());
        let output = f.click(&ctx, pos, egui::PointerButton::Primary);
        assert_eq!(output.platform_output.copied_text, LINK);
        assert!(output.platform_output.open_url.is_none());
    }
}

#[test]
fn outbox_gif_fallbacks_hide_address_and_keep_public_link_actions() {
    for mode in ["disabled", "minimized", "pending", "error"] {
        let ctx = egui::Context::default();
        let mut f = DeliveryFixture::new(&ctx);
        match mode {
            "disabled" => f.prefs.online_gif_previews = false,
            "pending" => {
                f.browser.messages.clear();
                f.browser.message_pending.insert("fixture55".into());
            }
            "error" => {
                f.browser.messages.clear();
                f.browser
                    .message_errors
                    .insert("fixture55".into(), "GIF unavailable".into());
            }
            _ => {}
        }
        let output = f.frame(&ctx, LINK, false, mode == "minimized", vec![]);
        let text = delivery_text(&output);
        assert!(
            text.iter().all(|text| !text.contains("giphy.com")),
            "{mode}"
        );
        if ["disabled", "minimized"].contains(&mode) {
            assert!(button(&output, "Open GIF").is_some(), "{mode}");
        }
        if mode == "error" {
            assert!(button(&output, "GIF unavailable").is_some());
            assert!(button(&output, "Retry GIF").is_some());
        }
        assert!(!f.browser.message_preview_open());
        assert!(output.platform_output.copied_text.is_empty());
        assert!(output.platform_output.open_url.is_none());
    }
}

#[test]
fn outbox_normal_text_and_spoilers_keep_formatting_without_requesting_gif() {
    let ctx = egui::Context::default();
    let mut f = DeliveryFixture::new(&ctx);
    for content in [
        "**Ready** ||private surprise|| __underlined__",
        "||https://giphy.com/gifs/fixture55||",
        "See https://giphy.com/gifs/fixture55 later",
    ] {
        let output = f.frame(&ctx, content, false, false, vec![]);
        let text = delivery_text(&output).join(" ");
        assert!(!text.contains("private surprise"));
        assert!(!text.contains("**") && !text.contains("__"));
        if content.starts_with("||") {
            assert!(text.contains("[spoiler]") && !text.contains("giphy.com"));
        }
        if content.starts_with("See ") {
            assert!(text.contains("See https://giphy.com/gifs/fixture55 later"));
        }
        assert!(!f.browser.message_preview_open());
        assert!(f.browser.message_pending.is_empty());
    }
}

struct BlockedFixture {
    media: DeliveryFixture,
    time: f64,
}

impl BlockedFixture {
    fn new(ctx: &egui::Context) -> Self {
        Self {
            media: DeliveryFixture::new(ctx),
            time: 0.0,
        }
    }

    fn frame(
        &mut self,
        ctx: &egui::Context,
        content: &str,
        events: Vec<egui::Event>,
    ) -> egui::FullOutput {
        assert!(self.media.browser.loader.is_none());
        assert_eq!(
            self.media.browser.key,
            online_gifs::configured_key(&self.media.prefs.giphy_api_key),
            "Keep the local fixture state instead of creating an HTTP loader"
        );
        if let Some(id) = online_gifs::message_id(content) {
            assert!(
                self.media.browser.messages.contains_key(&id)
                    || self.media.browser.message_pending.contains(&id)
                    || self.media.browser.message_errors.contains_key(&id),
                "Even a failed collapsed-state regression must not fetch a GIF"
            );
        }
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(640.0, 480.0))),
            focused: true,
            time: Some(self.time),
            events,
            ..Default::default()
        };
        // Let collapsing-header animation settle between real pointer events.
        self.time += 0.25;
        let output = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                self.media.browser.draw_blocked_message(
                    ui,
                    "blocked-fixture55",
                    content,
                    &self.media.prefs,
                );
            });
        });
        assert!(
            self.media.browser.loader.is_none(),
            "Blocked-message UI tests must not create an HTTP worker"
        );
        output
    }

    fn click(
        &mut self,
        ctx: &egui::Context,
        content: &str,
        pos: Pos2,
        button: egui::PointerButton,
    ) -> egui::FullOutput {
        self.frame(ctx, content, vec![egui::Event::PointerMoved(pos)]);
        let mut events = pointer(pos, button);
        let release = events.pop().unwrap();
        self.frame(ctx, content, events);
        self.frame(ctx, content, vec![release])
    }

    fn expand(&mut self, ctx: &egui::Context, content: &str) -> egui::FullOutput {
        let output = self.frame(ctx, content, vec![]);
        let pos = button(&output, "Blocked message").expect("Collapsed header must be drawn");
        self.click(ctx, content, pos, egui::PointerButton::Primary);
        self.frame(ctx, content, vec![])
    }
}

#[test]
fn collapsed_blocked_gif_hides_content_without_starting_a_loader() {
    let ctx = egui::Context::default();
    let mut f = BlockedFixture::new(&ctx);
    f.media.browser.messages.clear();
    // A local error sentinel makes an accidental eager draw observable while
    // preventing that regression from sending an external request.
    const HIDDEN_ERROR: &str = "Hidden GIF error must stay collapsed";
    f.media
        .browser
        .message_errors
        .insert("fixture55".into(), HIDDEN_ERROR.into());
    for _ in 0..3 {
        let output = f.frame(&ctx, LINK, vec![]);
        let text = delivery_text(&output);
        assert!(button(&output, "Blocked message").is_some());
        assert!(text.iter().all(|text| !text.contains("giphy.com")));
        assert!(text.iter().all(|text| !text.contains(HIDDEN_ERROR)));
        assert!(button(&output, "Open GIF").is_none());
        assert!(button(&output, "Retry GIF").is_none());
        assert!(output
            .shapes
            .iter()
            .all(|shape| texture_rect(&shape.shape, f.media.texture).is_none()));
        assert!(f.media.browser.message_pending.is_empty());
        assert!(f.media.browser.messages.is_empty());
        assert!(!f.media.browser.message_preview_open());
        assert!(output.platform_output.copied_text.is_empty());
        assert!(output.platform_output.open_url.is_none());
    }
}

#[test]
fn expanded_blocked_gif_hides_url_and_keeps_preview_and_copy_actions() {
    let ctx = egui::Context::default();
    let mut f = BlockedFixture::new(&ctx);
    let output = f.expand(&ctx, LINK);
    assert!(delivery_text(&output)
        .iter()
        .all(|text| !text.contains("giphy.com")));
    let rect = output
        .shapes
        .iter()
        .find_map(|shape| texture_rect(&shape.shape, f.media.texture))
        .expect("Explicitly expanding the blocked GIF must paint its decoded image");
    let output = f.click(&ctx, LINK, rect.center(), egui::PointerButton::Primary);
    assert_eq!(
        f.media.browser.message_preview,
        Some(("fixture55".into(), LINK.into()))
    );
    assert!(output.platform_output.copied_text.is_empty());
    assert!(output.platform_output.open_url.is_none());
    f.media.browser.close_message_preview();

    f.click(&ctx, LINK, rect.center(), egui::PointerButton::Secondary);
    let output = f.frame(&ctx, LINK, vec![]);
    assert!(button(&output, "View GIF").is_some());
    assert!(button(&output, "Open original").is_some());
    assert!(!f.media.browser.message_preview_open());
    let pos = button(&output, "Copy GIF link").expect("Expanded blocked GIF context menu");
    let output = f.click(&ctx, LINK, pos, egui::PointerButton::Primary);
    assert_eq!(output.platform_output.copied_text, LINK);
    assert!(output.platform_output.open_url.is_none());
    assert!(!f.media.browser.message_preview_open());
    let output = f.frame(&ctx, LINK, vec![]);
    assert!(button(&output, "Copy GIF link").is_none());
    assert!(f.media.browser.message_pending.is_empty());
}

#[test]
fn expanded_blocked_plain_text_is_preserved_without_requesting_a_gif() {
    const CONTENT: &str = "Ordinary blocked text remains available after explicit expansion.";
    let ctx = egui::Context::default();
    let mut f = BlockedFixture::new(&ctx);
    f.media.browser.messages.clear();
    let output = f.frame(&ctx, CONTENT, vec![]);
    assert!(delivery_text(&output).iter().all(|text| text != CONTENT));
    let output = f.expand(&ctx, CONTENT);
    assert!(delivery_text(&output).iter().any(|text| text == CONTENT));
    assert!(output
        .shapes
        .iter()
        .all(|shape| texture_rect(&shape.shape, f.media.texture).is_none()));
    assert!(button(&output, "Open GIF").is_none());
    assert!(button(&output, "Retry GIF").is_none());
    assert!(f.media.browser.messages.is_empty());
    assert!(f.media.browser.message_pending.is_empty());
    assert!(f.media.browser.message_errors.is_empty());
    assert!(!f.media.browser.message_preview_open());
    assert!(output.platform_output.copied_text.is_empty());
    assert!(output.platform_output.open_url.is_none());
}

// Both headers deliberately render in the same Ui: wrapping each
// message in a separate push_id/scope here would hide the title-ID regression.
#[test]
fn two_blocked_messages_keep_expansion_state_separate_and_close_cleanly() {
    const SECOND_LINK: &str = "https://giphy.com/gifs/fixture66";
    const SECOND_ERROR: &str = "Second blocked GIF must stay collapsed";

    struct PairFixture {
        media: DeliveryFixture,
        time: f64,
    }

    impl PairFixture {
        fn new(ctx: &egui::Context) -> Self {
            let mut media = DeliveryFixture::new(ctx);
            // Protect the second GIF from HTTP even if an ID collision opens
            // both bodies. The visible sentinel makes that regression fail.
            media
                .browser
                .message_errors
                .insert("fixture66".into(), SECOND_ERROR.into());
            Self { media, time: 0.0 }
        }

        fn frame(&mut self, ctx: &egui::Context, events: Vec<egui::Event>) -> egui::FullOutput {
            assert!(self.media.browser.loader.is_none());
            assert_eq!(
                self.media.browser.key,
                online_gifs::configured_key(&self.media.prefs.giphy_api_key),
                "Keep the local cache and error sentinel instead of fetching"
            );
            assert!(self.media.browser.messages.contains_key("fixture55"));
            assert_eq!(
                self.media
                    .browser
                    .message_errors
                    .get("fixture66")
                    .map(String::as_str),
                Some(SECOND_ERROR)
            );
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(640.0, 480.0))),
                focused: true,
                time: Some(self.time),
                events,
                ..Default::default()
            };
            self.time += 0.25;
            let output = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    self.media.browser.draw_blocked_message(
                        ui,
                        "blocked-first-message",
                        LINK,
                        &self.media.prefs,
                    );
                    ui.add_space(8.0);
                    self.media.browser.draw_blocked_message(
                        ui,
                        "blocked-second-message",
                        SECOND_LINK,
                        &self.media.prefs,
                    );
                });
            });
            assert!(self.media.browser.loader.is_none());
            assert!(self.media.browser.message_pending.is_empty());
            assert!(!self.media.browser.message_preview_open());
            assert!(output.platform_output.copied_text.is_empty());
            assert!(output.platform_output.open_url.is_none());
            output
        }

        fn click_first_header(&mut self, ctx: &egui::Context, output: &egui::FullOutput) {
            let pos = button(output, "Blocked message").expect("First blocked header");
            self.frame(ctx, vec![egui::Event::PointerMoved(pos)]);
            let mut events = pointer(pos, egui::PointerButton::Primary);
            let release = events.pop().unwrap();
            self.frame(ctx, events);
            self.frame(ctx, vec![release]);
        }

        fn assert_second_is_hidden(&self, output: &egui::FullOutput) {
            let text = delivery_text(output);
            assert_eq!(
                text.iter()
                    .filter(|text| *text == "Blocked message")
                    .count(),
                2,
                "Both independent blocked headers must be present"
            );
            assert!(text.iter().all(|text| !text.contains("giphy.com")));
            assert!(text.iter().all(|text| !text.contains(SECOND_ERROR)));
            assert!(button(output, "Retry GIF").is_none());
        }
    }

    let ctx = egui::Context::default();
    let mut f = PairFixture::new(&ctx);
    let output = f.frame(&ctx, vec![]);
    f.assert_second_is_hidden(&output);
    assert!(output
        .shapes
        .iter()
        .all(|shape| texture_rect(&shape.shape, f.media.texture).is_none()));

    f.click_first_header(&ctx, &output);
    let output = f.frame(&ctx, vec![]);
    f.assert_second_is_hidden(&output);
    let image = output
        .shapes
        .iter()
        .find_map(|shape| texture_rect(&shape.shape, f.media.texture))
        .expect("Opening the first message must paint its cached decoded GIF");
    assert!(image.is_positive());

    f.click_first_header(&ctx, &output);
    // Wait through the collapsing animation using deterministic frame times.
    for _ in 0..3 {
        let output = f.frame(&ctx, vec![]);
        f.assert_second_is_hidden(&output);
        assert!(output
            .shapes
            .iter()
            .all(|shape| texture_rect(&shape.shape, f.media.texture).is_none()));
    }
}
