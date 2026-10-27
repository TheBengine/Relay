//! Local file review. No request is made until the user explicitly uploads.
use super::*;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc,
};

enum Status {
    Reviewing,
    Ready,
    Uploading(String),
}
struct Reviewed {
    review: crate::network::UploadReview,
    preview: Option<Arc<egui::ColorImage>>,
    note: Option<String>,
}
struct Entry {
    id: String,
    generation: u64,
    origin: CommandOrigin,
    target: Target,
    path: PathBuf,
    status: Status,
    reviewed: Option<Reviewed>,
    texture: Option<egui::TextureHandle>,
    error: Option<String>,
    cancel: Arc<AtomicBool>,
    last_operation: Option<String>,
}
struct ReviewJob {
    id: String,
    generation: u64,
    path: PathBuf,
    cancel: Arc<AtomicBool>,
}
struct ReviewResult {
    id: String,
    generation: u64,
    result: Result<Reviewed, String>,
}
#[derive(Default)]
pub(super) struct State {
    entries: Vec<Entry>,
    jobs: Option<mpsc::SyncSender<ReviewJob>>,
    results: Option<mpsc::Receiver<ReviewResult>>,
    discarded: std::collections::HashSet<String>,
}
impl Drop for State {
    fn drop(&mut self) {
        for entry in &self.entries {
            entry.cancel.store(true, Ordering::Release);
        }
    }
}

fn review_file(path: &std::path::Path, cancel: &AtomicBool) -> Result<Reviewed, String> {
    let mut file = std::fs::File::open(path).map_err(|_| "The selected file cannot be read.")?;
    let before = file
        .metadata()
        .map_err(|_| "The selected file cannot be read.")?;
    if !before.is_file()
        || before.len() == 0
        || before.len() > relay_protocol::MAX_SUPPORTED_UPLOAD_BYTES as u64
    {
        return Err("Choose a nonempty file no larger than 25 MiB.".into());
    }
    let mut digest = Sha256::new();
    let mut encoded =
        (before.len() <= crate::chat_media::MAX_COMPRESSED_BYTES as u64).then(Vec::new);
    let mut block = [0u8; 64 * 1024];
    let mut read = 0u64;
    loop {
        if cancel.load(Ordering::Acquire) {
            return Err("File review cancelled.".into());
        }
        let count = file
            .read(&mut block)
            .map_err(|_| "The selected file could not be reviewed.")?;
        if count == 0 {
            break;
        }
        read += count as u64;
        if read > before.len() {
            return Err("The file changed while it was being reviewed. Choose it again.".into());
        }
        digest.update(&block[..count]);
        if let Some(encoded) = &mut encoded {
            encoded.extend_from_slice(&block[..count]);
        }
    }
    let after = file
        .metadata()
        .map_err(|_| "The selected file cannot be read.")?;
    if read != before.len()
        || after.len() != before.len()
        || after.modified().ok() != before.modified().ok()
    {
        return Err("The file changed while it was being reviewed. Choose it again.".into());
    }
    let (preview, note) = match encoded {
        Some(bytes) => match crate::chat_media::staging_preview(&bytes) {
            Ok(decoded) => (
                decoded.frames.first().map(|frame| frame.image.clone()),
                None,
            ),
            Err(note) => (None, Some(note)),
        },
        None => (
            None,
            Some(
                "Preview unavailable above 10 MiB; the original file can still be uploaded.".into(),
            ),
        ),
    };
    Ok(Reviewed {
        review: crate::network::UploadReview {
            bytes: read,
            sha256: digest.finalize().into(),
        },
        preview,
        note,
    })
}

impl State {
    pub(super) fn clear(&mut self) {
        for entry in self.entries.drain(..) {
            entry.cancel.store(true, Ordering::Release);
            if let Status::Uploading(id) = entry.status {
                self.discarded.insert(id);
            }
        }
    }
    pub(super) fn owns_operation(&self, id: &str) -> bool {
        self.entries
            .iter()
            .any(|e| e.last_operation.as_deref() == Some(id))
    }
    fn ensure_worker(&mut self, ctx: &egui::Context) -> Result<(), String> {
        if self.jobs.is_some() {
            return Ok(());
        }
        let (jobs, receive) = mpsc::sync_channel::<ReviewJob>(8);
        let (complete, results) = mpsc::sync_channel(8);
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("relay-file-review".into())
            .spawn(move || {
                while let Ok(job) = receive.recv() {
                    if job.cancel.load(Ordering::Acquire) {
                        continue;
                    }
                    let result = review_file(&job.path, &job.cancel);
                    if job.cancel.load(Ordering::Acquire) {
                        continue;
                    }
                    if complete
                        .send(ReviewResult {
                            id: job.id,
                            generation: job.generation,
                            result,
                        })
                        .is_err()
                    {
                        return;
                    }
                    ctx.request_repaint();
                }
            })
            .map_err(|_| "File previews could not start.".to_string())?;
        self.jobs = Some(jobs);
        self.results = Some(results);
        Ok(())
    }
    fn stage(
        &mut self,
        origin: CommandOrigin,
        target: Target,
        path: PathBuf,
        ctx: &egui::Context,
    ) -> Result<(), String> {
        if self.entries.len() >= 16 || self.count(&origin, &target) >= 8 {
            return Err("Upload or remove selected files before adding more.".into());
        }
        self.ensure_worker(ctx)?;
        let id = uuid::Uuid::new_v4().to_string();
        let cancel = Arc::new(AtomicBool::new(false));
        self.jobs
            .as_ref()
            .unwrap()
            .try_send(ReviewJob {
                id: id.clone(),
                generation: 0,
                path: path.clone(),
                cancel: cancel.clone(),
            })
            .map_err(|_| "File review is busy. Wait briefly and try again.".to_string())?;
        self.entries.push(Entry {
            id,
            generation: 0,
            origin,
            target,
            path,
            status: Status::Reviewing,
            reviewed: None,
            texture: None,
            error: None,
            cancel,
            last_operation: None,
        });
        Ok(())
    }
    fn count(&self, origin: &CommandOrigin, target: &Target) -> usize {
        self.entries
            .iter()
            .filter(|e| &e.origin == origin && &e.target == target)
            .count()
    }
    fn poll(&mut self, ctx: &egui::Context, pending_limit: u64) {
        let Some(results) = &self.results else {
            return;
        };
        while let Ok(result) = results.try_recv() {
            let used: u64 = self
                .entries
                .iter()
                .filter_map(|e| e.reviewed.as_ref())
                .map(|r| r.review.bytes)
                .sum();
            let Some(entry) = self
                .entries
                .iter_mut()
                .find(|e| e.id == result.id && e.generation == result.generation)
            else {
                continue;
            };
            entry.status = Status::Ready;
            match result.result {
                Ok(reviewed) if used.saturating_add(reviewed.review.bytes) <= pending_limit.min(relay_protocol::MAX_PENDING_UPLOAD_BYTES) => {
                    if let Some(image) = &reviewed.preview {
                        entry.texture = Some(ctx.load_texture(format!("selected-file-{}", entry.id), image.clone(), egui::TextureOptions::LINEAR));
                    }
                    entry.reviewed = Some(reviewed); entry.error = None;
                }
                Ok(_) => entry.error = Some("Selected files exceed the pending upload allowance. Remove a file, then review this one again.".into()),
                Err(error) => entry.error = Some(error),
            }
        }
    }
    pub(super) fn uploaded(
        &mut self,
        operation: &str,
        active: Option<&CommandOrigin>,
        target: &Target,
    ) -> bool {
        if self.discarded.remove(operation) {
            return false;
        }
        let Some(index) = self
            .entries
            .iter()
            .position(|e| matches!(&e.status, Status::Uploading(id) if id == operation))
        else {
            return true;
        };
        if Some(&self.entries[index].origin) != active || &self.entries[index].target != target {
            self.entries[index].status = Status::Ready;
            self.entries[index].error =
                Some("The conversation changed. Return here to upload the file again.".into());
            return false;
        }
        self.entries.remove(index);
        true
    }
    pub(super) fn failed(&mut self, operation: &str, error: &str) {
        self.discarded.remove(operation);
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|e| matches!(&e.status, Status::Uploading(id) if id == operation))
        {
            entry.status = Status::Ready;
            entry.error = Some(error.into());
        }
    }
}

impl RelayApp {
    pub(super) fn stage_file(&mut self, target: Target, path: PathBuf) {
        let Some(origin) = self.command_origin() else {
            return;
        };
        if self.attachments.len() + self.staged_uploads.count(&origin, &target) >= 8 {
            self.error = Some(
                "A message can include up to eight files. Remove one before adding another.".into(),
            );
            return;
        }
        if let Err(error) = self
            .staged_uploads
            .stage(origin, target, path, &self.capture_ctx)
        {
            self.error = Some(error);
        }
    }
    pub(super) fn has_staged_uploads(&self) -> bool {
        self.command_origin()
            .zip(self.current.as_ref())
            .is_some_and(|(origin, target)| self.staged_uploads.count(&origin, target) > 0)
    }
    pub(super) fn staged_files_ui(&mut self, ui: &mut egui::Ui) {
        let (Some(origin), Some(target), Some(bootstrap)) = (
            self.command_origin(),
            self.current.clone(),
            self.bootstrap.clone(),
        ) else {
            return;
        };
        self.staged_uploads
            .poll(ui.ctx(), bootstrap.upload_limits.pending_bytes);
        if self.staged_uploads.count(&origin, &target) == 0 {
            return;
        }
        let accepted = self
            .prefs
            .communities
            .iter()
            .find(|c| c.url == self.login.url && c.server_id == bootstrap.server_id)
            .and_then(|c| c.open_protection_accepted);
        let plain = crate::protection::plain_allowed(&bootstrap.protection, accepted);
        let limit = crate::network::upload_plain_limit(&bootstrap.upload_limits, plain);
        ui.label(
            RichText::new(format!(
                "Selected files · up to {:.1} MiB each",
                limit as f64 / 1_048_576.0
            ))
            .strong(),
        );
        ui.label(RichText::new("Review these files, upload up to two at a time, then send your message. Your text stays in the composer.").small().color(c_muted()));
        let mut remove = None;
        let mut recheck = None;
        egui::ScrollArea::vertical()
            .id_source("selected-files-review")
            .max_height(220.0)
            .show(ui, |ui| {
                for entry in self
                    .staged_uploads
                    .entries
                    .iter()
                    .filter(|e| e.origin == origin && e.target == target)
                {
                    ui.group(|ui| {
                        ui.horizontal(|ui| {
                            if let Some(texture) = &entry.texture {
                                ui.add(
                                    egui::Image::new((texture.id(), texture.size_vec2()))
                                        .max_size(egui::vec2(96.0, 72.0)),
                                );
                            }
                            ui.vertical(|ui| {
                                ui.label(
                                    entry
                                        .path
                                        .file_name()
                                        .and_then(|s| s.to_str())
                                        .unwrap_or("Selected file"),
                                );
                                if let Some(reviewed) = &entry.reviewed {
                                    ui.label(size_label(reviewed.review.bytes));
                                    if let Some(note) = &reviewed.note {
                                        ui.add(
                                            egui::Label::new(
                                                RichText::new(note).small().color(c_muted()),
                                            )
                                            .wrap(true),
                                        );
                                    }
                                }
                                match &entry.status {
                                    Status::Reviewing => {
                                        ui.label("Reviewing file…");
                                    }
                                    Status::Uploading(_) => {
                                        ui.label("Uploading…");
                                    }
                                    Status::Ready => {}
                                }
                                if let Some(error) = &entry.error {
                                    ui.add(
                                        egui::Label::new(RichText::new(error).color(c_bad()))
                                            .wrap(true),
                                    );
                                }
                                if !matches!(entry.status, Status::Uploading(_))
                                    && entry.error.is_some()
                                    && ui.small_button("Review file again").clicked()
                                {
                                    recheck = Some(entry.id.clone());
                                }
                            });
                            if ui.small_button("Remove").clicked() {
                                remove = Some(entry.id.clone());
                            }
                        });
                    });
                }
            });
        if let Some(id) = remove {
            if let Some(index) = self.staged_uploads.entries.iter().position(|e| e.id == id) {
                let entry = self.staged_uploads.entries.remove(index);
                entry.cancel.store(true, Ordering::Release);
                if let Status::Uploading(operation_id) = entry.status {
                    self.staged_uploads.discarded.insert(operation_id.clone());
                    self.send(Command::CancelUpload { operation_id });
                }
            }
        }
        if let Some(id) = recheck {
            if let Some(entry) = self.staged_uploads.entries.iter_mut().find(|e| e.id == id) {
                entry.generation += 1;
                entry.reviewed = None;
                entry.texture = None;
                entry.error = None;
                entry.status = Status::Reviewing;
                let job = ReviewJob {
                    id: entry.id.clone(),
                    generation: entry.generation,
                    path: entry.path.clone(),
                    cancel: entry.cancel.clone(),
                };
                if self
                    .staged_uploads
                    .jobs
                    .as_ref()
                    .unwrap()
                    .try_send(job)
                    .is_err()
                {
                    entry.status = Status::Ready;
                    entry.error = Some("File review is busy. Try again shortly.".into());
                }
            }
        }
        let slots = 2usize.saturating_sub(
            self.uploads
                .values()
                .filter(|u| u.error.is_none() && !u.cancelled)
                .count(),
        );
        let ready = self.staged_uploads.entries.iter().any(|e| {
            e.origin == origin
                && e.target == target
                && matches!(e.status, Status::Ready)
                && e.reviewed.is_some()
        });
        if ui
            .add_enabled(ready && slots > 0, primary("Upload selected files"))
            .clicked()
        {
            let tasks: Vec<_> = self
                .staged_uploads
                .entries
                .iter()
                .filter(|e| {
                    e.origin == origin
                        && e.target == target
                        && matches!(e.status, Status::Ready)
                        && e.reviewed.is_some()
                })
                .take(slots)
                .map(|e| {
                    (
                        e.id.clone(),
                        e.path.clone(),
                        e.reviewed.as_ref().unwrap().review.clone(),
                    )
                })
                .collect();
            for (id, path, review) in tasks {
                if review.bytes > limit || limit == 0 {
                    if let Some(entry) = self.staged_uploads.entries.iter_mut().find(|e| e.id == id)
                    {
                        entry.error = Some("This file exceeds your current server upload allowance. Your draft and reviewed file are kept.".into());
                    }
                    continue;
                }
                if let Some(operation) = self.queue_reviewed_upload(target.clone(), path, review) {
                    if let Some(entry) = self.staged_uploads.entries.iter_mut().find(|e| e.id == id)
                    {
                        entry.last_operation = Some(operation.clone());
                        entry.status = Status::Uploading(operation);
                        entry.error = None;
                    }
                } else if let Some(entry) =
                    self.staged_uploads.entries.iter_mut().find(|e| e.id == id)
                {
                    entry.error = Some(
                        "The upload could not start. Your file and draft are kept; try again."
                            .into(),
                    );
                }
            }
        }
        ui.add_space(8.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn origin(user: &str) -> CommandOrigin {
        CommandOrigin {
            url: "https://owned.invalid".into(),
            server_id: "owned-server".into(),
            user_id: user.into(),
        }
    }
    fn entry(id: &str, bytes: u64) -> Entry {
        Entry {
            id: id.into(),
            generation: 0,
            origin: origin("owned-user"),
            target: Target::Direct("original-peer".into()),
            path: PathBuf::from("C:/owned/selected"),
            status: Status::Ready,
            reviewed: Some(Reviewed {
                review: crate::network::UploadReview {
                    bytes,
                    sha256: [7; 32],
                },
                preview: None,
                note: None,
            }),
            texture: None,
            error: None,
            cancel: Arc::new(AtomicBool::new(false)),
            last_operation: None,
        }
    }
    #[test]
    fn extensionless_rh_selection_sniffs_original_bytes_and_reviews_exact_hash() {
        let base = std::env::var_os("RELAY_TEST_WORK_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("relay-tests"));
        let folder = base.join(format!("file-review-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&folder).unwrap();
        let path = folder.join("rh");
        let mut encoded = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(8, 6)
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        std::fs::write(&path, encoded.get_ref()).unwrap();
        let reviewed = review_file(&path, &AtomicBool::new(false)).unwrap();
        assert_eq!(reviewed.review.bytes, encoded.get_ref().len() as u64);
        let expected: [u8; 32] = Sha256::digest(encoded.get_ref()).into();
        assert_eq!(reviewed.review.sha256, expected);
        assert_eq!(reviewed.preview.unwrap().size, [8, 6]);
        assert_eq!(std::fs::read(&path).unwrap(), encoded.into_inner());
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(folder).unwrap();
    }
    #[test]
    fn upload_failure_keeps_review_and_success_attaches_only_the_original_scoped_draft() {
        let mut state = State::default();
        let mut selected = entry("selected", 12);
        selected.status = Status::Uploading("operation".into());
        selected.last_operation = Some("operation".into());
        state.entries.push(selected);
        state.failed("operation", "Server reduced the upload allowance");
        assert_eq!(
            state.entries[0].reviewed.as_ref().unwrap().review.sha256,
            [7; 32]
        );
        assert!(matches!(state.entries[0].status, Status::Ready));
        assert!(state.entries[0]
            .error
            .as_ref()
            .unwrap()
            .contains("allowance"));
        state.entries[0].status = Status::Uploading("retry".into());
        // Completion's target remains original-peer even if another composer is visible.
        assert!(state.uploaded(
            "retry",
            Some(&origin("owned-user")),
            &Target::Direct("original-peer".into())
        ));
        assert!(state.entries.is_empty());
    }
    #[test]
    fn removed_or_account_invalidated_review_cannot_attach_a_late_result() {
        let mut state = State::default();
        let mut selected = entry("selected", 12);
        selected.status = Status::Uploading("operation".into());
        state.entries.push(selected);
        assert!(!state.uploaded(
            "operation",
            Some(&origin("replacement-user")),
            &Target::Direct("original-peer".into())
        ));
        assert_eq!(state.entries.len(), 1);
        state.entries[0].status = Status::Uploading("operation".into());
        state.clear();
        assert!(state.entries.is_empty());
        assert!(!state.uploaded(
            "operation",
            Some(&origin("owned-user")),
            &Target::Direct("original-peer".into())
        ));
    }
    #[test]
    fn asynchronous_reviews_respect_pending_allowance_without_retaining_large_file_buffers() {
        let mut state = State::default();
        let (tx, rx) = mpsc::sync_channel(4);
        state.results = Some(rx);
        for id in ["first", "second", "third"] {
            let mut selected = entry(id, 0);
            selected.reviewed = None;
            selected.status = Status::Reviewing;
            state.entries.push(selected);
            assert!(tx
                .send(ReviewResult {
                    id: id.into(),
                    generation: 0,
                    result: Ok(Reviewed {
                        review: crate::network::UploadReview {
                            bytes: 20 * 1024 * 1024,
                            sha256: [0; 32]
                        },
                        preview: None,
                        note: None,
                    })
                })
                .is_ok());
        }
        state.poll(
            &egui::Context::default(),
            relay_protocol::MAX_PENDING_UPLOAD_BYTES,
        );
        assert_eq!(
            state
                .entries
                .iter()
                .filter(|e| e.reviewed.is_some())
                .count(),
            2
        );
        assert!(state.entries[2].reviewed.is_none() && state.entries[2].error.is_some());
    }
    #[test]
    fn old_review_generation_is_ignored_after_file_is_removed_or_reviewed_again() {
        let mut state = State::default();
        let (tx, rx) = mpsc::sync_channel(1);
        state.results = Some(rx);
        let mut selected = entry("selected", 0);
        selected.reviewed = None;
        selected.generation = 2;
        state.entries.push(selected);
        assert!(tx
            .send(ReviewResult {
                id: "selected".into(),
                generation: 1,
                result: Ok(Reviewed {
                    review: crate::network::UploadReview {
                        bytes: 12,
                        sha256: [9; 32]
                    },
                    preview: None,
                    note: None,
                })
            })
            .is_ok());
        state.poll(
            &egui::Context::default(),
            relay_protocol::MAX_PENDING_UPLOAD_BYTES,
        );
        assert!(state.entries[0].reviewed.is_none());
    }
}
