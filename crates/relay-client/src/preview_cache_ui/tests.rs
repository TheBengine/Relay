use super::*;

fn png_image() -> AnimatedImage {
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgba8(512, 512)
        .write_to(&mut bytes, image::ImageFormat::Png)
        .unwrap();
    AnimatedImage::new(crate::chat_media::decode(bytes.get_ref()).unwrap())
}
#[derive(Default)]
struct Fixture {
    attachments: HashMap<String, AnimatedImage>,
    errors: HashMap<String, String>,
    online: online_gif_ui::GifBrowser,
}
impl Fixture {
    fn add(&mut self, count: usize) {
        for i in 0..count {
            self.attachments.insert(format!("image-{i}"), png_image());
        }
    }
    fn trim(
        &mut self,
        ctx: &egui::Context,
        activity: Instant,
        now: Instant,
        minimized: bool,
        original: Option<&str>,
    ) -> egui::FullOutput {
        ctx.run(egui::RawInput::default(), |ctx| {
            PreviewCache {
                attachments: &mut self.attachments,
                errors: &mut self.errors,
                online: &mut self.online,
                open_original: original,
            }
            .trim(ctx, activity, now, minimized)
        })
    }
    fn bytes(&self) -> usize {
        self.attachments
            .values()
            .map(AnimatedImage::decoded_bytes)
            .sum::<usize>()
            + self.online.cached_message_bytes()
    }
}
fn context() -> egui::Context {
    let ctx = egui::Context::default();
    for _ in 0..3 {
        let _ = ctx.run(egui::RawInput::default(), |_| {});
    }
    ctx
}
fn delay(out: &egui::FullOutput) -> Duration {
    out.viewport_output[&egui::ViewportId::ROOT].repaint_delay
}

#[test]
fn small_actual_png_cache_uses_no_timer_and_late_results_still_trim() {
    let ctx = context();
    let start = Instant::now();
    let mut f = Fixture::default();
    f.add(2);
    assert_eq!(f.bytes(), 2 * 1024 * 1024);
    for seconds in [0, 30, 60, 120, 300] {
        let output = f.trim(
            &ctx,
            start,
            start + Duration::from_secs(seconds),
            false,
            None,
        );
        assert_eq!(delay(&output), Duration::MAX);
        assert_eq!(f.attachments.len(), 2);
    }
    f.add(9);
    assert_eq!(f.bytes(), 9 * 1024 * 1024);
    let output = f.trim(&ctx, start, start + Duration::from_secs(301), false, None);
    assert_eq!(f.attachments.len(), 8);
    assert_eq!(delay(&output), Duration::MAX);
    assert!(f.errors.is_empty());
}

#[test]
fn active_large_cache_has_one_exact_idle_deadline_then_releases_textures_and_stops() {
    let ctx = context();
    let start = Instant::now();
    let mut f = Fixture::default();
    f.add(9);
    let ids: Vec<_> = f
        .attachments
        .values_mut()
        .map(|im| im.texture(&ctx, false, false))
        .collect();
    let _ = ctx.run(egui::RawInput::default(), |_| {});
    let first = f.trim(&ctx, start, start, false, None);
    assert_eq!(delay(&first), Duration::from_secs(60));
    assert_eq!(f.attachments.len(), 9);
    let midway = f.trim(&ctx, start, start + Duration::from_secs(30), false, None);
    assert_eq!(delay(&midway), Duration::from_secs(30));
    assert_eq!(f.attachments.len(), 9);
    let idle = f.trim(&ctx, start, start + Duration::from_secs(60), false, None);
    assert_eq!(delay(&idle), Duration::MAX);
    assert_eq!(f.attachments.len(), 8);
    assert!(idle.textures_delta.free.iter().any(|id| ids.contains(id)));
    let later = f.trim(&ctx, start, start + Duration::from_secs(600), false, None);
    assert_eq!(delay(&later), Duration::MAX);
    assert_eq!(f.attachments.len(), 8);
}

#[test]
fn visible_static_previews_and_open_original_are_kept_without_repeated_idle_timer() {
    let ctx = context();
    let mut f = Fixture::default();
    f.add(9);
    for im in f.attachments.values_mut() {
        im.texture(&ctx, true, false);
    }
    let _ = ctx.run(egui::RawInput::default(), |_| {});
    // Start the cache clock after decode/texture setup, including under a busy
    // full workspace test run. The later instant must follow every actual draw.
    let now = Instant::now();
    let activity = now - Duration::from_secs(60);
    let out = f.trim(&ctx, activity, now, false, None);
    assert_eq!(f.attachments.len(), 9);
    assert_eq!(delay(&out), Duration::MAX);
    assert!(f.errors.is_empty());
    let out = f.trim(
        &ctx,
        activity,
        now + Duration::from_secs(2),
        false,
        Some("image-8"),
    );
    assert!(f.attachments.contains_key("image-8"));
    assert_eq!(f.bytes(), IDLE_BYTES);
    assert_eq!(delay(&out), Duration::MAX);
}

#[test]
fn shared_online_and_attachment_cache_obeys_combined_budget_and_preserves_both_originals() {
    let ctx = context();
    let now = Instant::now();
    let mut f = Fixture::default();
    f.add(5);
    f.online = online_gif_ui::GifBrowser::cached_fixture(
        (0..5)
            .map(|i| (format!("online-{i}"), png_image()))
            .collect(),
        Some("online-4"),
    );
    assert_eq!(f.bytes(), 10 * 1024 * 1024);
    let out = f.trim(
        &ctx,
        now - Duration::from_secs(60),
        now,
        false,
        Some("image-4"),
    );
    assert_eq!(f.bytes(), IDLE_BYTES);
    assert!(f.attachments.contains_key("image-4"));
    assert!(f.online.message_preview_open());
    assert!(f
        .online
        .oldest_cached_message(false, now)
        .is_none_or(|(id, _)| id != "online-4"));
    assert_eq!(delay(&out), Duration::MAX);
    let bytes = f.online.evict_cached_message("online-4", false);
    assert_eq!(
        bytes,
        1024 * 1024,
        "The open online original remained cached"
    );
}

#[test]
fn minimized_cache_trims_immediately_and_new_activity_gets_a_fresh_deadline() {
    let ctx = context();
    let now = Instant::now();
    let mut f = Fixture::default();
    f.add(9);
    let out = f.trim(&ctx, now, now, true, Some("image-8"));
    assert_eq!(f.bytes(), IDLE_BYTES);
    assert_eq!(delay(&out), Duration::MAX);
    assert!(f.attachments.contains_key("image-8"));
    f.add(10);
    let activity = now + Duration::from_secs(100);
    let out = f.trim(&ctx, activity, activity, false, None);
    assert_eq!(f.bytes(), 10 * 1024 * 1024);
    assert_eq!(delay(&out), Duration::from_secs(60));
    let again = f.trim(
        &ctx,
        activity + Duration::from_secs(20),
        activity + Duration::from_secs(25),
        false,
        None,
    );
    assert_eq!(delay(&again), Duration::from_secs(55));
}

#[test]
fn hard_active_budget_still_releases_hidden_previews_before_idle_and_keeps_original() {
    let ctx = context();
    let now = Instant::now();
    let mut f = Fixture::default();
    f.add(33);
    assert_eq!(f.bytes(), 33 * 1024 * 1024);
    let out = f.trim(&ctx, now, now, false, Some("image-32"));
    assert_eq!(f.bytes(), ACTIVE_BYTES);
    assert!(f.attachments.contains_key("image-32"));
    assert_eq!(delay(&out), Duration::from_secs(60));
    assert!(f.errors.is_empty());
}

#[test]
fn closing_online_original_makes_its_hidden_texture_evictable_on_that_frame() {
    let ctx = context();
    // Decode first so construction time cannot age the first visible texture.
    let mut entries: Vec<_> = (0..9)
        .map(|i| (format!("online-{i}"), png_image()))
        .collect();
    let mut original = None;
    for (i, (_, image)) in entries.iter_mut().enumerate() {
        let texture = image.texture(&ctx, i > 0, false);
        if i == 0 {
            original = Some(texture);
        }
    }
    let _ = ctx.run(egui::RawInput::default(), |_| {});
    let now = Instant::now();
    let mut f = Fixture {
        online: online_gif_ui::GifBrowser::cached_fixture(entries, Some("online-0")),
        ..Default::default()
    };
    let out = f.trim(&ctx, now - Duration::from_secs(60), now, false, None);
    assert_eq!(f.bytes(), 9 * 1024 * 1024);
    assert_eq!(delay(&out), Duration::MAX);
    assert!(f.online.close_message_preview());
    let out = f.trim(&ctx, now - Duration::from_secs(60), now, false, None);
    assert_eq!(f.bytes(), IDLE_BYTES);
    assert!(out.textures_delta.free.contains(&original.unwrap()));
    assert_eq!(delay(&out), Duration::MAX);
}
