//! QA builds only (`qa-automation`): owned GUI checks run in background windows where egui menu
//! buttons and native file dialogs cannot be opened through accessibility. Two files in the
//! isolated profile folder stand in for those clicks; each is read once and deleted:
//! `qa-stage.txt` stages the named file in the open conversation exactly as the "+" menu does,
//! and `qa-pick-file.txt` is what the next presentation-image file dialog "chooses".
use super::*;

impl RelayApp {
    pub(super) fn qa_commands(&mut self, ctx: &egui::Context) {
        // Owned GUI checks drop command files into the profile; poll them twice a second.
        ctx.request_repaint_after(std::time::Duration::from_millis(500));
        static FIRST: std::sync::Once = std::sync::Once::new();
        FIRST.call_once(|| {
            eprintln!("QA commands active; profile {:?}", std::env::var_os("RELAY_PROFILE_DIR"));
        });
        let Some(dir) = std::env::var_os("RELAY_PROFILE_DIR") else {
            return;
        };
        // `qa-shot.txt` holds a file name: the next frame is saved as shots/NAME.png in the profile.
        let shot = std::path::Path::new(&dir).join("qa-shot.txt");
        if let Ok(name) = std::fs::read_to_string(&shot) {
            let _ = std::fs::remove_file(&shot);
            let name: String = name.trim().chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
            if !name.is_empty() {
                let folder = std::path::Path::new(&dir).join("shots");
                let _ = std::fs::create_dir_all(&folder);
                self.qa_input.1 = Some(folder.join(format!("{name}.png")));
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot);
            }
        }
        let file = std::path::Path::new(&dir).join("qa-stage.txt");
        let text = match std::fs::read_to_string(&file) {
            Ok(text) => text,
            Err(error) => {
                if error.kind() != std::io::ErrorKind::NotFound {
                    eprintln!("QA command file unreadable: {error}");
                }
                return;
            }
        };
        if let Err(error) = std::fs::remove_file(&file) {
            eprintln!("QA command file not removed: {error}");
        }
        eprintln!("QA command: stage {:?} in {:?}", text.trim(), self.current);
        if let Some(target) = self.current.clone() {
            self.stage_file(target, PathBuf::from(text.trim()));
        }
    }
}

pub(crate) fn picked_file() -> Option<PathBuf> {
    let dir = std::env::var_os("RELAY_PROFILE_DIR")?;
    let file = std::path::Path::new(&dir).join("qa-pick-file.txt");
    let text = std::fs::read_to_string(&file).ok()?;
    let _ = std::fs::remove_file(&file);
    Some(PathBuf::from(text.trim()))
}
