//! The one place presentation-image file dialogs are opened.
use std::path::PathBuf;

pub(crate) fn pick(dialog: rfd::FileDialog) -> Option<PathBuf> {
    #[cfg(feature = "qa-automation")]
    if let Some(path) = crate::app::qa_picked_file() {
        return Some(path);
    }
    dialog.pick_file()
}
