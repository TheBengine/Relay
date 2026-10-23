//! Opt-in native render evidence for an explicit QA profile. No input injection.
use eframe::egui;
use image::ImageEncoder;
use std::{
    fs::{self, OpenOptions},
    io::Read,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const LIMIT: usize = 64;

pub fn path_from_args(args: &[String]) -> Result<Option<PathBuf>, String> {
    if !args.iter().any(|arg| arg == "--native-render-probe") {
        return Ok(None);
    }
    if args.iter().any(|arg| arg == "--capture") {
        return Err("Native render probe and capture-only mode are mutually exclusive".into());
    }
    let profile = args
        .iter()
        .position(|arg| arg == "--profile-dir")
        .and_then(|index| args.get(index + 1))
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or("Native render probe requires an explicit absolute --profile-dir")?;
    reject_redirects(&profile)?;
    Ok(Some(profile.join("native-render-probe")))
}

fn reject_redirects(path: &Path) -> Result<(), String> {
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) => {
                #[cfg(windows)]
                let redirect = {
                    use std::os::windows::fs::MetadataExt;
                    metadata.file_attributes() & 0x400 != 0
                };
                #[cfg(not(windows))]
                let redirect = metadata.file_type().is_symlink();
                if redirect {
                    return Err("Native render probe refuses redirected paths".into());
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(())
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 48
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

pub struct Probe {
    directory: PathBuf,
    pending: Option<(String, Instant)>,
    issued: usize,
    disabled: bool,
}

impl Probe {
    pub fn open(directory: PathBuf) -> Result<Self, String> {
        reject_redirects(&directory)?;
        fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
        Ok(Self {
            directory,
            pending: None,
            issued: 0,
            disabled: false,
        })
    }

    fn take_request(&mut self) -> Result<Option<String>, String> {
        if self.disabled || self.issued >= LIMIT || self.pending.is_some() {
            return Ok(None);
        }
        let path = self.directory.join("request");
        let metadata = match fs::symlink_metadata(&path) {
            Ok(value) => value,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.to_string()),
        };
        reject_redirects(&path)?;
        if !metadata.is_file() || metadata.len() > 64 {
            return Err("Invalid render request file".into());
        }
        let mut value = String::new();
        fs::File::open(&path)
            .map_err(|error| error.to_string())?
            .take(65)
            .read_to_string(&mut value)
            .map_err(|error| error.to_string())?;
        let id = value.trim();
        if value.len() > 64 || !valid_id(id) {
            return Err("Invalid render request identifier".into());
        }
        let output = self.directory.join(format!("{id}.png"));
        if fs::symlink_metadata(output).is_ok() {
            return Err("Render evidence must not be overwritten".into());
        }
        fs::remove_file(path).map_err(|error| error.to_string())?;
        self.issued += 1;
        Ok(Some(id.to_owned()))
    }

    pub fn update(&mut self, ctx: &egui::Context) {
        if self.disabled || self.issued >= LIMIT && self.pending.is_none() {
            return;
        }
        // A late screenshot after a timeout must never be assigned to a new request.
        if self
            .pending
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() > Duration::from_secs(3))
        {
            self.disabled = true;
            self.pending = None;
            eprintln!("Native render probe timed out; disabled for this process");
            return;
        }
        match self.take_request() {
            Ok(Some(id)) => {
                self.pending = Some((id, Instant::now()));
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot);
            }
            Ok(None) => {}
            Err(error) => {
                self.disabled = true;
                eprintln!("Native render probe disabled: {error}");
            }
        }
        if !self.disabled {
            ctx.request_repaint_after(Duration::from_millis(200));
        }
    }

    pub fn complete(&mut self, image: &egui::ColorImage) {
        let Some((id, _)) = self.pending.take() else {
            return;
        };
        let result = (|| -> Result<(), String> {
            let [width, height] = image.size;
            if width == 0
                || height == 0
                || width > 4096
                || height > 4096
                || image.pixels.len() != width * height
            {
                return Err("Invalid native render dimensions".into());
            }
            reject_redirects(&self.directory)?;
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(self.directory.join(format!("{id}.png")))
                .map_err(|error| error.to_string())?;
            let bytes: Vec<_> = image
                .pixels
                .iter()
                .flat_map(|pixel| pixel.to_array())
                .collect();
            image::codecs::png::PngEncoder::new(file)
                .write_image(&bytes, width as u32, height as u32, image::ColorType::Rgba8)
                .map_err(|error| error.to_string())
        })();
        if let Err(error) = result {
            self.disabled = true;
            eprintln!("Native render probe disabled: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Temporary(PathBuf);
    impl Temporary {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("relay-render-probe-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Temporary {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn normal_launch_never_enables_probe_and_explicit_profile_is_required() {
        assert!(path_from_args(&[]).unwrap().is_none());
        assert!(path_from_args(&["--native-render-probe".into()]).is_err());
        assert!(path_from_args(&[
            "--native-render-probe".into(),
            "--profile-dir".into(),
            "relative".into()
        ])
        .is_err());
        let temp = Temporary::new();
        let mut args = vec![
            "--native-render-probe".into(),
            "--profile-dir".into(),
            temp.0.to_string_lossy().into_owned(),
        ];
        assert_eq!(
            path_from_args(&args).unwrap(),
            Some(temp.0.join("native-render-probe"))
        );
        args.push("--capture".into());
        assert!(path_from_args(&args).is_err());
    }
    #[test]
    fn traversal_and_oversized_requests_cannot_select_an_output() {
        let temp = Temporary::new();
        let mut probe = Probe::open(temp.0.clone()).unwrap();
        for value in [
            "../outside",
            "C:\\outside",
            "test/name",
            "",
            &"a".repeat(65),
        ] {
            fs::write(temp.0.join("request"), value).unwrap();
            assert!(probe.take_request().is_err());
            assert_eq!(probe.issued, 0);
            assert!(temp.0.join("request").exists());
        }
    }
    #[test]
    fn completed_evidence_cannot_be_overwritten_or_reissued() {
        let temp = Temporary::new();
        let mut probe = Probe::open(temp.0.clone()).unwrap();
        fs::write(temp.0.join("request"), "one").unwrap();
        let id = probe.take_request().unwrap().unwrap();
        probe.pending = Some((id, Instant::now()));
        probe.complete(&egui::ColorImage::new([2, 2], egui::Color32::RED));
        let bytes = fs::read(temp.0.join("one.png")).unwrap();
        fs::write(temp.0.join("request"), "one").unwrap();
        assert!(probe.take_request().is_err());
        assert_eq!(fs::read(temp.0.join("one.png")).unwrap(), bytes);
        probe.pending = Some(("one".into(), Instant::now()));
        probe.complete(&egui::ColorImage::new([2, 2], egui::Color32::BLUE));
        assert!(probe.disabled);
        assert_eq!(fs::read(temp.0.join("one.png")).unwrap(), bytes);
    }
    #[test]
    fn pending_or_unsolicited_frames_cannot_consume_the_next_request() {
        let temp = Temporary::new();
        let mut probe = Probe::open(temp.0.clone()).unwrap();
        fs::write(temp.0.join("request"), "two").unwrap();
        probe.complete(&egui::ColorImage::new([2, 2], egui::Color32::RED));
        assert!(!temp.0.join("two.png").exists());
        probe.pending = Some(("one".into(), Instant::now()));
        assert!(probe.take_request().unwrap().is_none());
        assert!(temp.0.join("request").exists());
        probe.complete(&egui::ColorImage::new([2, 2], egui::Color32::RED));
        assert_eq!(probe.take_request().unwrap().as_deref(), Some("two"));
    }
    #[test]
    fn capture_limit_and_timeout_disable_further_work() {
        let temp = Temporary::new();
        let mut probe = Probe::open(temp.0.clone()).unwrap();
        fs::write(temp.0.join("request"), "two").unwrap();
        probe.issued = LIMIT;
        assert!(probe.take_request().unwrap().is_none());
        probe.issued = 1;
        probe.pending = Some(("one".into(), Instant::now() - Duration::from_secs(4)));
        probe.update(&egui::Context::default());
        assert!(probe.disabled);
        assert!(probe.pending.is_none());
        probe.complete(&egui::ColorImage::new([2, 2], egui::Color32::RED));
        assert!(!temp.0.join("one.png").exists());
        assert!(probe.take_request().unwrap().is_none());
    }
}
