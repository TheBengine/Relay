//! Read-only Git discovery. Unsupported layouts yield unavailable provenance.
use super::{invalid, redirected, reject_redirects};
use std::{
    fs, io,
    path::{Component, Path, PathBuf},
};

#[cfg(any(windows, test))]
fn direct_volume_target(target: &str) -> bool {
    target
        .strip_prefix("\\Device\\HarddiskVolume")
        .is_some_and(|number| {
            !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
        })
}

fn local(path: &Path) -> io::Result<PathBuf> {
    if !path.is_absolute() {
        return Err(invalid("Git metadata path must be absolute"));
    }
    let mut clean = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir if clean.parent().is_some() => {
                clean.pop();
            }
            Component::ParentDir => return Err(invalid("Git path escaped its volume")),
            Component::CurDir => {}
            component => clean.push(component.as_os_str()),
        }
    }
    #[cfg(windows)]
    {
        use std::path::Prefix;
        let Some(Component::Prefix(prefix)) = clean.components().next() else {
            return Err(invalid("Git path is not a permitted local drive"));
        };
        let drive = match prefix.kind() {
            Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => drive.to_ascii_uppercase(),
            _ => return Err(invalid("Git network/device path refused")),
        };
        if !matches!(drive, b'C' | b'D') {
            return Err(invalid("Git metadata is outside local C/D"));
        }
        #[link(name = "kernel32")]
        extern "system" {
            fn GetDriveTypeW(root: *const u16) -> u32;
            fn QueryDosDeviceW(name: *const u16, target: *mut u16, length: u32) -> u32;
        }
        let volume = format!("{}:\\", char::from(drive))
            .encode_utf16()
            .chain(Some(0))
            .collect::<Vec<_>>();
        if unsafe { GetDriveTypeW(volume.as_ptr()) } != 3 {
            return Err(invalid("Git metadata requires a fixed local drive"));
        }
        let name = format!("{}:", char::from(drive))
            .encode_utf16()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let mut target = [0u16; 1024];
        if unsafe { QueryDosDeviceW(name.as_ptr(), target.as_mut_ptr(), target.len() as u32) } == 0
        {
            return Err(invalid("Cannot validate direct local Git volume mapping"));
        }
        let end = target
            .iter()
            .position(|value| *value == 0)
            .ok_or_else(|| invalid("Unterminated Git volume mapping"))?;
        if !direct_volume_target(&String::from_utf16_lossy(&target[..end])) {
            return Err(invalid(
                "Mapped, substituted and nonlocal Git drives are refused",
            ));
        }
    }
    // Inspect raw prefixes before lexical '..' normalization can hide a link.
    reject_redirects(path)?;
    reject_redirects(&clean)?;
    Ok(clean)
}

fn read(path: &Path, limit: u64) -> io::Result<String> {
    let path = local(path)?;
    let metadata = fs::symlink_metadata(&path)?;
    if redirected(&metadata) || !metadata.is_file() || metadata.len() > limit {
        return Err(invalid("Git metadata is not a bounded direct file"));
    }
    let content = fs::read_to_string(path)?;
    if content.contains('\0') {
        return Err(invalid("NUL in Git metadata"));
    }
    Ok(content)
}

fn optional_config(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(local(path)?) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
        Ok(_) => {}
    }
    let config = read(path, 1024 * 1024)?;
    // Do not let Git follow conditional/global/local config includes into a
    // location the build did not inspect. No silent claim of clean on refusal.
    for line in config.lines() {
        let line = line.trim_start().to_ascii_lowercase();
        if line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let compact = line
            .chars()
            .filter(|character| !character.is_ascii_whitespace())
            .collect::<String>();
        if compact.starts_with("[include") || compact.starts_with("[filter") || line.ends_with('\\')
        {
            return Err(invalid(
                "Git config includes, filters and continuations are unsupported for provenance",
            ));
        }
    }
    Ok(())
}

fn metadata_tree(
    directory: &Path,
    count: &mut usize,
    depth: usize,
    worktree: bool,
) -> io::Result<()> {
    if depth > 64 {
        return Err(invalid("Git preflight depth exceeded"));
    }
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        *count += 1;
        if *count > 100_000 {
            return Err(invalid("Git preflight entry budget exceeded"));
        }
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if redirected(&metadata) {
            return Err(invalid("Redirected Git/worktree entry refused"));
        }
        if worktree && entry.file_name() == ".git" {
            if depth == 0 {
                continue;
            }
            return Err(invalid(
                "Nested Git layout is unsupported for all-tree status",
            ));
        }
        if metadata.is_dir() {
            metadata_tree(&path, count, depth + 1, worktree)?;
        } else if !metadata.is_file() {
            return Err(invalid("Nonregular Git/worktree entry refused"));
        }
    }
    Ok(())
}

pub struct Layout {
    pub directory: PathBuf,
}

pub fn empty_config(path: &Path) -> io::Result<()> {
    let path = local(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.len() != 0 {
        return Err(invalid(
            "Git override config must be an owned empty direct file",
        ));
    }
    Ok(())
}

pub fn inspect(root: &Path) -> io::Result<Layout> {
    let root = local(root)?;
    let pointer = root.join(".git");
    let metadata = fs::symlink_metadata(local(&pointer)?)?;
    let directory = if metadata.is_dir() {
        pointer
    } else {
        let pointer_text = read(&pointer, 4096)?;
        let value = pointer_text
            .trim_end_matches(['\r', '\n'])
            .strip_prefix("gitdir: ")
            .filter(|value| !value.is_empty() && !value.chars().any(char::is_control))
            .ok_or_else(|| invalid("Invalid bounded worktree gitdir pointer"))?;
        local(&root.join(value))?
    };
    if !fs::symlink_metadata(&directory)?.is_dir() {
        return Err(invalid("Git directory is not a direct directory"));
    }
    let common_pointer = directory.join("commondir");
    let common = match fs::symlink_metadata(local(&common_pointer)?) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => directory.clone(),
        Err(error) => return Err(error),
        Ok(_) => {
            let text = read(&common_pointer, 4096)?;
            let value = text.trim_end_matches(['\r', '\n']);
            if value.is_empty() || value.chars().any(char::is_control) {
                return Err(invalid("Invalid bounded commondir pointer"));
            }
            local(&directory.join(value))?
        }
    };
    if !fs::symlink_metadata(&common)?.is_dir() {
        return Err(invalid("Git common directory is not direct"));
    }
    let head = read(&directory.join("HEAD"), 4096)?;
    if let Some(reference) = head.trim_end().strip_prefix("ref: ") {
        if !reference.starts_with("refs/")
            || reference.contains("..")
            || reference.chars().any(char::is_control)
            || reference.contains('\\')
        {
            return Err(invalid("Unsafe Git HEAD reference"));
        }
        let _ = local(&common.join(reference))?;
    }
    optional_config(&common.join("config"))?;
    optional_config(&directory.join("config.worktree"))?;
    // Index/ref files are direct metadata, not Cargo watch roots. Packed object
    // stores/refs are metadata-preflighted without reading their content here.
    for path in [
        directory.join("index"),
        common.join("packed-refs"),
        common.join("shallow"),
    ] {
        let _ = local(&path)?;
    }
    let alternates = common.join("objects/info/alternates");
    match fs::symlink_metadata(local(&alternates)?) {
        Ok(_) if !read(&alternates, 64 * 1024)?.trim().is_empty() => {
            return Err(invalid("Alternate Git object stores are unsupported"))
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut count = 0;
    metadata_tree(&root, &mut count, 0, true)?;
    for path in [&directory, &common]
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>()
    {
        metadata_tree(path, &mut count, 0, false)?;
    }
    Ok(Layout { directory })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preflight_budget_failure_is_explicit_and_bounded() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("owned-untracked.txt"), "synthetic fixture").unwrap();
        let mut count = 100_000;
        assert!(metadata_tree(root.path(), &mut count, 0, true).is_err());
        assert_eq!(count, 100_001);
    }

    #[test]
    fn volume_mapping_rejects_subst_network_and_ambiguous_devices() {
        assert!(direct_volume_target(r"\Device\HarddiskVolume3"));
        for target in [
            r"\??\C:\redirect",
            r"\Device\Mup\server\share",
            r"\Device\HarddiskVolumeShadowCopy1",
            r"\Device\HarddiskVolume3\redirect",
            "",
        ] {
            assert!(!direct_volume_target(target), "accepted {target:?}");
        }
    }
}
