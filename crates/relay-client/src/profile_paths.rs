//! Explicit isolated profiles are resolved before any UI or worker starts.
use std::path::PathBuf;

pub(crate) fn requested_root(args: &[String]) -> Result<Option<PathBuf>, String> {
    let mut found = None;
    for (index, argument) in args.iter().enumerate().skip(1) {
        if argument.starts_with("--profile-dir=") {
            return Err("Use --profile-dir followed by an absolute directory.".into());
        }
        if argument != "--profile-dir" {
            continue;
        }
        if found.is_some() {
            return Err("Only one --profile-dir may be supplied.".into());
        }
        let root = args
            .get(index + 1)
            .filter(|value| !value.is_empty() && !value.starts_with("--"))
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .ok_or("--profile-dir requires an absolute directory.")?;
        found = Some(root);
    }
    Ok(found)
}

pub(crate) fn configure(args: &[String]) -> Result<(), String> {
    quit_on_close(args)?;
    let Some(root) = requested_root(args)? else {
        return Ok(());
    };
    std::fs::create_dir_all(&root).map_err(|e| format!("Create profile directory: {e}"))?;
    let root = root
        .canonicalize()
        .map_err(|e| format!("Resolve profile directory: {e}"))?;
    for name in ["config", "encryption", "server-data"] {
        std::fs::create_dir_all(root.join(name))
            .map_err(|e| format!("Create profile storage: {e}"))?;
    }
    // Called once in main before eframe, devices or network workers exist.
    std::env::set_var("RELAY_CONFIG_DIR", root.join("config"));
    std::env::set_var("RELAY_CRYPTO_DIR", root.join("encryption"));
    std::env::set_var("RELAY_SERVER_DATA_DIR", root.join("server-data"));
    std::env::set_var("RELAY_PROFILE_DIR", root);
    Ok(())
}

/// An explicit isolated run can finish through its window's close control.
/// This is runtime-only; the user's saved close-to-tray preference is untouched.
pub(crate) fn quit_on_close(args: &[String]) -> Result<bool, String> {
    let count = args.iter().filter(|arg| *arg == "--quit-on-close").count();
    if count > 1 || args.iter().any(|arg| arg.starts_with("--quit-on-close=")) {
        return Err("Use --quit-on-close once, without a value.".into());
    }
    if count == 1 && requested_root(args)?.is_none() {
        return Err("--quit-on-close requires an explicit --profile-dir.".into());
    }
    Ok(count == 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn args(values: &[&str]) -> Vec<String> {
        std::iter::once("relay")
            .chain(values.iter().copied())
            .map(String::from)
            .collect()
    }
    #[test]
    fn malformed_profile_cannot_fall_back_to_personal_storage() {
        for values in [
            vec!["--profile-dir"],
            vec!["--profile-dir", ""],
            vec!["--profile-dir", "--settings", "voice"],
            vec!["--profile-dir", "relative-profile"],
            vec!["--profile-dir="],
        ] {
            assert!(requested_root(&args(&values)).is_err());
        }
        let root = std::env::temp_dir().join("relay-profile-parser");
        let path = root.to_str().unwrap();
        assert!(requested_root(&args(&["--profile-dir", path, "--profile-dir", path])).is_err());
        assert_eq!(
            requested_root(&args(&["--settings", "voice"])).unwrap(),
            None
        );
        assert_eq!(
            requested_root(&args(&["--profile-dir", path, "--settings", "voice"])).unwrap(),
            Some(root)
        );
    }

    #[test]
    fn quit_on_close_is_explicit_and_never_changes_a_normal_launch() {
        assert!(!quit_on_close(&args(&[])).unwrap());
        let root = std::env::temp_dir().join("relay-close-parser");
        let path = root.to_str().unwrap();
        assert!(!quit_on_close(&args(&["--profile-dir", path])).unwrap());
        assert!(quit_on_close(&args(&["--profile-dir", path, "--quit-on-close"])).unwrap());
        for values in [
            vec!["--quit-on-close"],
            vec!["--quit-on-close=true"],
            vec!["--profile-dir", path, "--quit-on-close", "--quit-on-close"],
        ] {
            assert!(quit_on_close(&args(&values)).is_err());
        }
    }
}
