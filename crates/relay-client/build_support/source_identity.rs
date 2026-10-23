//! Bounded, deterministic provenance for local workspace sources, including untracked inputs.
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    ffi::OsString,
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    process::Command,
};

pub const SOURCE_SCOPE: &str = "relay-client-inputs-v2";
#[path = "git_layout.rs"]
mod git_layout;
#[path = "source_scope.rs"]
mod source_scope;
const DIRECTORIES: &[&str] = &[".cargo", "crates", "scripts", "vendor"];

pub struct SourceIdentity {
    pub sha256: String,
    pub watched: Vec<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitIdentity {
    pub revision: String,
    pub dirty: Option<bool>,
}

impl GitIdentity {
    pub fn unavailable() -> Self {
        Self {
            revision: "nogit".into(),
            dirty: None,
        }
    }

    pub fn from_output(revision: Option<&str>, dirty: Option<bool>) -> Self {
        let Some(revision) = revision.map(str::trim).filter(|value| {
            matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
        }) else {
            return Self::unavailable();
        };
        Self {
            revision: revision[..12].to_ascii_lowercase(),
            dirty,
        }
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn redirected(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return true;
        }
    }
    false
}

fn reject_redirects_with(
    path: &Path,
    mut metadata: impl FnMut(&Path) -> io::Result<fs::Metadata>,
) -> io::Result<()> {
    // Check the volume/root before any descendant: even lstat of a descendant
    // can traverse a redirected ancestor.
    for ancestor in path.ancestors().collect::<Vec<_>>().into_iter().rev() {
        match metadata(ancestor) {
            Ok(metadata) if redirected(&metadata) => {
                return Err(invalid("Redirected source path refused"))
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

pub fn reject_redirects(path: &Path) -> io::Result<()> {
    reject_redirects_with(path, |ancestor| fs::symlink_metadata(ancestor))
}

pub fn validate_build_output(root: &Path, output: &Path) -> io::Result<()> {
    reject_redirects(root)?;
    reject_redirects(output)?;
    let root = root.canonicalize()?;
    let output = output.canonicalize()?;
    if output.starts_with(root.join(".git"))
        || DIRECTORIES
            .iter()
            .any(|name| output.starts_with(root.join(name)))
    {
        return Err(invalid("Cargo build output must be outside .git/.cargo/crates/scripts/vendor; choose an external CARGO_TARGET_DIR"));
    }
    Ok(())
}

fn excluded(relative: &Path, directory: bool) -> bool {
    let parts = relative
        .iter()
        .map(|part| part.to_string_lossy().to_ascii_lowercase())
        .collect::<Vec<_>>();
    let Some(name) = parts.last() else {
        return false;
    };
    let fixture = parts
        .iter()
        .any(|part| matches!(part.as_str(), "fixtures" | "testdata"));
    let package_root = parts.len() == 3 && matches!(parts[0].as_str(), "crates" | "vendor");
    let runtime_root_file =
        package_root || (parts.len() == 2 && matches!(parts[0].as_str(), "scripts" | ".cargo"));
    if directory {
        return matches!(name.as_str(), ".git" | "__pycache__")
            || (package_root
                && matches!(
                    name.as_str(),
                    "target"
                        | "dist"
                        | "work"
                        | "profiles"
                        | "server-data"
                        | "encryption"
                        | "private-network"
                        | "tailscale-state"
                ));
    }
    // Examples and checked-in fixture data are source inputs. In particular,
    // src/profiles and fixtures/*.db must not be mistaken for personal storage.
    let environment_example = matches!(
        name.as_str(),
        ".env.example" | ".env.sample" | ".env.template"
    );
    name.ends_with(".local")
        || name.ends_with(".pfx")
        || name.ends_with(".p12")
        || (name.starts_with(".env") && !environment_example && !fixture)
        || [".pyc", ".pyo"]
            .iter()
            .any(|extension| name.ends_with(extension))
        || (runtime_root_file
            && !fixture
            && [".pem", ".key", ".log", ".db", ".sqlite", ".sqlite3"]
                .iter()
                .any(|extension| name.ends_with(extension)))
        || (runtime_root_file
            && matches!(
                name.as_str(),
                "preferences.json"
                    | "credentials.json"
                    | "credentials.toml"
                    | "session.json"
                    | "managed-instance.json"
                    | "stack-instance.json"
                    | "server-runtime.json"
                    | "id_rsa"
                    | "id_ed25519"
            ))
}

fn collect(
    root: &Path,
    directory: &Path,
    files: &mut BTreeSet<PathBuf>,
    watched: &mut BTreeSet<PathBuf>,
) -> io::Result<()> {
    let metadata = match fs::symlink_metadata(directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(invalid(
                "Declared source root disappeared while fingerprinting",
            ))
        }
        Err(error) => return Err(error),
    };
    if redirected(&metadata) || !metadata.is_dir() {
        return Err(invalid("Source directory is redirected or not a directory"));
    }
    watched.insert(directory.into());
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        let name = entry.file_name();
        let name = name
            .to_str()
            .ok_or_else(|| invalid("Source filename is not UTF-8"))?;
        if name.chars().any(char::is_control) {
            return Err(invalid("Control character in source filename"));
        }
        // Never follow a redirect, even when its target would be excluded.
        if redirected(&metadata) {
            return Err(invalid("Redirected source entry refused"));
        }
        if excluded(
            path.strip_prefix(root)
                .map_err(|_| invalid("Source escaped workspace"))?,
            metadata.is_dir(),
        ) {
            continue;
        }
        if metadata.is_dir() {
            collect(root, &path, files, watched)?;
        } else if metadata.is_file() {
            files.insert(path);
        } else {
            return Err(invalid("Nonregular source entry refused"));
        }
        if files.len() > 200_000 {
            return Err(invalid("Source input count exceeds its limit"));
        }
    }
    Ok(())
}

pub fn source_identity(root: &Path) -> io::Result<SourceIdentity> {
    if !root.is_absolute() {
        return Err(invalid("Source root must be absolute"));
    }
    reject_redirects(root)?;
    let mut files = BTreeSet::new();
    let mut watched = BTreeSet::new();
    let scope = source_scope::inputs(root)?;
    for directory in scope.directories {
        collect(root, &directory, &mut files, &mut watched)?;
    }
    for path in scope.files {
        reject_redirects(&path)?;
        match fs::symlink_metadata(&path) {
            Ok(metadata) if !redirected(&metadata) && metadata.is_file() => {
                watched.insert(path.clone());
                files.insert(path);
            }
            Ok(_) => return Err(invalid("Build input is redirected or not a file")),
            Err(error) => return Err(error),
        }
    }
    if !files.contains(&root.join("Cargo.toml")) || !files.contains(&root.join("Cargo.lock")) {
        return Err(invalid("Workspace Cargo.toml and Cargo.lock are required"));
    }
    let mut tree = Sha256::new();
    tree.update(SOURCE_SCOPE.as_bytes());
    tree.update([0]);
    let mut inputs = files
        .into_iter()
        .map(|path| {
            let relative = path
                .strip_prefix(root)
                .map_err(|_| invalid("Source escaped workspace"))?
                .to_str()
                .ok_or_else(|| invalid("Source path is not UTF-8"))?
                .replace('\\', "/");
            Ok((relative, path))
        })
        .collect::<io::Result<Vec<_>>>()?;
    inputs.sort_by(|a, b| a.0.cmp(&b.0));
    for (relative, path) in inputs {
        let mut file = fs::File::open(&path)?;
        let before = file.metadata()?;
        if !before.is_file() || before.len() > 512 * 1024 * 1024 {
            return Err(invalid(
                "Source input exceeds its size limit or is not regular",
            ));
        }
        let mut content = Sha256::new();
        let mut bytes = 0u64;
        let mut buffer = [0; 64 * 1024];
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            bytes += count as u64;
            if bytes > before.len() {
                return Err(invalid("Source changed while computing build identity"));
            }
            content.update(&buffer[..count]);
        }
        let after = file.metadata()?;
        let path_after = fs::symlink_metadata(&path)?;
        if redirected(&path_after)
            || bytes != before.len()
            || before.len() != after.len()
            || before.modified()? != after.modified()?
            || before.modified()? != path_after.modified()?
        {
            return Err(invalid("Source changed while computing build identity"));
        }
        tree.update((relative.len() as u64).to_le_bytes());
        tree.update(relative.as_bytes());
        tree.update(bytes.to_le_bytes());
        let digest = content.finalize();
        if scope
            .parsed
            .get(&path)
            .is_some_and(|parsed| parsed[..] != digest[..])
        {
            return Err(invalid(
                "Cargo metadata changed after resolving client inputs",
            ));
        }
        tree.update(digest);
        watched.insert(path);
    }
    Ok(SourceIdentity {
        sha256: format!("{:x}", tree.finalize()),
        watched: watched.into_iter().collect(),
    })
}

fn git_command(
    root: &Path,
    git_directory: &Path,
    environment: &[(OsString, OsString)],
    empty_config: &Path,
) -> Command {
    let mut command = Command::new("git");
    command.current_dir(root);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Read-only build metadata and its GUI-subsystem test harness need no
        // console. Avoid allocating a console for every Git child on Windows.
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    command.env_clear();
    for (key, value) in environment {
        if !key
            .to_string_lossy()
            .to_ascii_uppercase()
            .starts_with("GIT_")
        {
            command.env(key, value);
        }
    }
    command
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", empty_config)
        .env("GIT_CONFIG_SYSTEM", empty_config)
        .arg("--git-dir")
        .arg(git_directory)
        .arg("--work-tree")
        .arg(root)
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "submodule.recurse=false",
            "-c",
            "status.submoduleSummary=false",
        ])
        .arg("-c")
        .arg(format!("core.excludesFile={}", empty_config.display()))
        .arg("-c")
        .arg(format!("core.attributesFile={}", empty_config.display()));
    command
}

fn git_identity_with_runner(
    root: &Path,
    empty_config: &Path,
    mut run: impl FnMut(&Path, &[&str]) -> Option<std::process::Output>,
) -> GitIdentity {
    if git_layout::empty_config(empty_config).is_err() {
        return GitIdentity::unavailable();
    }
    let Ok(layout) = git_layout::inspect(root) else {
        return GitIdentity::unavailable();
    };
    let Some(revision) = run(&layout.directory, &["rev-parse", "--verify", "HEAD"]) else {
        return GitIdentity::unavailable();
    };
    let dirty = run(
        &layout.directory,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignore-submodules=all",
        ],
    )
    .map(|output| !output.stdout.is_empty());
    GitIdentity::from_output(std::str::from_utf8(&revision.stdout).ok(), dirty)
}

fn git_identity_with_environment(
    root: &Path,
    environment: &[(OsString, OsString)],
    empty_config: &Path,
) -> GitIdentity {
    git_identity_with_runner(root, empty_config, |git_directory, arguments| {
        git_command(root, git_directory, environment, empty_config)
            .args(arguments)
            .output()
            .ok()
            .filter(|output| output.status.success())
    })
}

pub fn git_identity(root: &Path, empty_config: &Path) -> GitIdentity {
    git_identity_with_environment(root, &std::env::vars_os().collect::<Vec<_>>(), empty_config)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn workspace() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/relay-client\",\"crates/relay-protocol\"]\n",
        )
        .unwrap();
        fs::write(directory.path().join("Cargo.lock"), "version = 4\n[[package]]\nname = \"relay-client\"\nversion = \"0.1.0\"\ndependencies = [\"relay-protocol\"]\n[[package]]\nname = \"relay-protocol\"\nversion = \"0.1.0\"\n").unwrap();
        fs::create_dir_all(directory.path().join("crates/relay-protocol/src")).unwrap();
        fs::write(
            directory.path().join("crates/relay-protocol/Cargo.toml"),
            "[package]\nname=\"relay-protocol\"\nversion=\"0.1.0\"\n",
        )
        .unwrap();
        fs::write(
            directory.path().join("crates/relay-protocol/src/lib.rs"),
            "// protocol source\n",
        )
        .unwrap();
        fs::create_dir_all(directory.path().join("crates/relay-client/src")).unwrap();
        for name in [
            "crates/relay-client/assets",
            "crates/relay-client/build_support",
            "crates/relay-protocol/fixtures",
            "scripts",
        ] {
            fs::create_dir_all(directory.path().join(name)).unwrap();
        }
        fs::write(
            directory.path().join("scripts/prepare-webrtc.ps1"),
            "# synthetic recipe\n",
        )
        .unwrap();
        fs::write(directory.path().join("crates/relay-client/build_support/client-inputs.toml"), "schema=1\nworkspace_files=[\"scripts/prepare-webrtc.ps1\"]\n[packages.\"crates/relay-client\"]\ndirectories=[\"src\",\"assets\",\"build_support\"]\n[packages.\"crates/relay-protocol\"]\ndirectories=[\"src\",\"fixtures\"]\n").unwrap();
        fs::write(directory.path().join("crates/relay-client/Cargo.toml"), "[package]\nname=\"relay-client\"\nversion=\"0.1.0\"\n[dependencies]\nrelay-protocol={path=\"../relay-protocol\"}\n").unwrap();
        fs::write(
            directory.path().join("crates/relay-client/src/main.rs"),
            "fn main() {}\n",
        )
        .unwrap();
        directory
    }

    #[test]
    fn changed_and_new_untracked_source_inputs_change_the_exact_digest() {
        let workspace = workspace();
        let first = source_identity(workspace.path()).unwrap();
        assert_eq!(
            first.sha256,
            source_identity(workspace.path()).unwrap().sha256
        );
        fs::write(
            workspace.path().join("crates/relay-client/src/main.rs"),
            "fn main() { println!(\"changed\"); }\n",
        )
        .unwrap();
        let changed = source_identity(workspace.path()).unwrap();
        assert_ne!(first.sha256, changed.sha256);
        let asset_dir = workspace.path().join("crates/relay-client/assets");
        fs::create_dir_all(&asset_dir).unwrap();
        fs::write(asset_dir.join("new-untracked.png"), [1, 2, 3, 4]).unwrap();
        let new_asset = source_identity(workspace.path()).unwrap();
        assert_ne!(changed.sha256, new_asset.sha256);
        assert!(new_asset.watched.contains(&asset_dir));
        assert!(new_asset
            .watched
            .contains(&asset_dir.join("new-untracked.png")));
        fs::remove_file(asset_dir.join("new-untracked.png")).unwrap();
        assert_eq!(
            changed.sha256,
            source_identity(workspace.path()).unwrap().sha256
        );
    }

    #[test]
    fn legitimate_profiles_and_other_source_names_are_not_runtime_storage() {
        let workspace = workspace();
        let mut previous = source_identity(workspace.path()).unwrap().sha256;
        for name in [
            "profiles",
            "encryption",
            "target",
            "dist",
            "work",
            "server-data",
            "private-network",
            "tailscale-state",
        ] {
            let directory = workspace
                .path()
                .join("crates/relay-protocol/src")
                .join(name);
            fs::create_dir_all(&directory).unwrap();
            let file = directory.join("tests.rs");
            fs::write(&file, format!("// legitimate {name} source\n")).unwrap();
            let current = source_identity(workspace.path()).unwrap();
            assert_ne!(previous, current.sha256, "omitted src/{name}/tests.rs");
            assert!(current.watched.contains(&file));
            previous = current.sha256;
        }
        let profile_test = workspace
            .path()
            .join("crates/relay-protocol/src/profiles/tests.rs");
        fs::write(profile_test, "// changed actual profile regression\n").unwrap();
        assert_ne!(previous, source_identity(workspace.path()).unwrap().sha256);
        let fixtures = workspace.path().join("crates/relay-protocol/fixtures");
        fs::create_dir_all(&fixtures).unwrap();
        for name in [
            "state.db",
            "certificate.pem",
            "public.key",
            "credentials.json",
            ".env.example",
        ] {
            let file = fixtures.join(name);
            fs::write(&file, "explicit test fixture; no real secret\n").unwrap();
            assert!(
                source_identity(workspace.path())
                    .unwrap()
                    .watched
                    .contains(&file),
                "omitted fixture {name}"
            );
        }
    }

    #[test]
    fn build_output_inside_any_watched_source_root_is_rejected() {
        let workspace = workspace();
        for root in DIRECTORIES {
            let output = workspace
                .path()
                .join(root)
                .join("client/target/release/build/out");
            fs::create_dir_all(&output).unwrap();
            assert!(validate_build_output(workspace.path(), &output).is_err());
        }
        let git_output = workspace.path().join(".git/target/release/build/out");
        fs::create_dir_all(&git_output).unwrap();
        assert!(validate_build_output(workspace.path(), &git_output).is_err());
        let ordinary_output = workspace.path().join("target/release/build/out");
        fs::create_dir_all(&ordinary_output).unwrap();
        assert!(validate_build_output(workspace.path(), &ordinary_output).is_ok());
        let external = tempfile::tempdir().unwrap();
        assert!(validate_build_output(workspace.path(), external.path()).is_ok());
    }

    #[test]
    fn inherited_git_repository_and_index_overrides_cannot_change_provenance() {
        let first = workspace();
        let other = workspace();
        let clean_environment = std::env::vars_os().collect::<Vec<_>>();
        let empty_config = tempfile::NamedTempFile::new().unwrap();
        assert!(
            git_command(
                first.path(),
                &first.path().join(".git"),
                &clean_environment,
                empty_config.path()
            )
            .get_envs()
            .any(|(key, value)| key == "GIT_NO_LAZY_FETCH"
                && value == Some(std::ffi::OsStr::new("1")))
        );
        assert_eq!(
            git_command(
                first.path(),
                &first.path().join(".git"),
                &clean_environment,
                empty_config.path()
            )
            .get_current_dir(),
            Some(first.path())
        );
        let initialize = |workspace: &Path, message: &str| {
            let run = |args: &[&str]| {
                let result = git_command(
                    workspace,
                    &workspace.join(".git"),
                    &clean_environment,
                    empty_config.path(),
                )
                .arg("-c")
                .arg(format!(
                    "core.hooksPath={}",
                    workspace.join("owned-no-hooks").display()
                ))
                .args(args)
                .output()
                .unwrap();
                assert!(
                    result.status.success(),
                    "Owned Git fixture command failed; workspace={workspace:?}; args={args:?}; cwd={:?}; status={}; stdout={}; stderr={}",
                    std::env::current_dir(),
                    result.status,
                    String::from_utf8_lossy(&result.stdout),
                    String::from_utf8_lossy(&result.stderr),
                );
            };
            run(&["init", "--quiet"]);
            run(&["add", "--all"]);
            run(&[
                "-c",
                "user.name=Relay Build Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "-m",
                message,
            ]);
        };
        initialize(first.path(), "first owned fixture");
        fs::write(
            other.path().join("crates/relay-client/src/main.rs"),
            "fn main() { println!(\"other source\"); }\n",
        )
        .unwrap();
        initialize(other.path(), "other owned fixture");
        let expected =
            git_identity_with_environment(first.path(), &clean_environment, empty_config.path());
        let foreign =
            git_identity_with_environment(other.path(), &clean_environment, empty_config.path());
        assert_ne!(expected.revision, "nogit");
        assert_ne!(expected.revision, foreign.revision);
        assert_eq!(expected.dirty, Some(false));
        let mut polluted = clean_environment;
        for (key, value) in [
            ("GIT_DIR", other.path().join(".git").into_os_string()),
            ("GIT_WORK_TREE", other.path().as_os_str().to_owned()),
            (
                "GIT_INDEX_FILE",
                other.path().join(".git/index").into_os_string(),
            ),
            ("GIT_COMMON_DIR", other.path().join(".git").into_os_string()),
            ("GIT_CONFIG_COUNT", OsString::from("1")),
            ("GIT_CONFIG_KEY_0", OsString::from("core.worktree")),
            ("GIT_CONFIG_VALUE_0", other.path().as_os_str().to_owned()),
        ] {
            polluted.push((key.into(), value));
        }
        assert_eq!(
            git_identity_with_environment(first.path(), &polluted, empty_config.path()),
            expected
        );
        fs::write(
            first.path().join("crates/relay-client/src/main.rs"),
            "fn main() { println!(\"first changed\"); }\n",
        )
        .unwrap();
        let changed = git_identity_with_environment(first.path(), &polluted, empty_config.path());
        assert_eq!(changed.revision, expected.revision);
        assert_eq!(changed.dirty, Some(true));
        assert_ne!(changed.revision, foreign.revision);
    }

    #[test]
    fn runtime_secrets_git_and_distribution_do_not_enter_source_identity() {
        let workspace = workspace();
        let first = source_identity(workspace.path()).unwrap().sha256;
        for directory in ["dist", ".git", "crates/relay-client/server-data"] {
            fs::create_dir_all(workspace.path().join(directory)).unwrap();
            fs::write(
                workspace.path().join(directory).join("private.txt"),
                "not a source input",
            )
            .unwrap();
        }
        fs::write(
            workspace.path().join("crates/relay-client/giphy-key.local"),
            "not a real key",
        )
        .unwrap();
        fs::write(
            workspace.path().join("crates/relay-client/.env"),
            "not a real secret",
        )
        .unwrap();
        assert_eq!(first, source_identity(workspace.path()).unwrap().sha256);
    }

    #[test]
    fn missing_git_is_unknown_instead_of_a_claim_of_clean_source() {
        let workspace = workspace();
        let empty_config = tempfile::NamedTempFile::new().unwrap();
        assert_eq!(
            git_identity(workspace.path(), empty_config.path()),
            GitIdentity::unavailable()
        );
        assert_eq!(
            GitIdentity::from_output(Some("not-a-revision"), Some(false)).dirty,
            None
        );
        let revision = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(
            GitIdentity::from_output(Some(revision), Some(true)),
            GitIdentity {
                revision: "0123456789ab".into(),
                dirty: Some(true)
            }
        );
        assert_eq!(
            GitIdentity::from_output(Some(revision), Some(false)).dirty,
            Some(false)
        );
        assert_eq!(GitIdentity::from_output(Some(revision), None).dirty, None);
    }

    #[test]
    fn only_client_dependency_envelope_and_exact_native_recipe_are_watched() {
        let workspace = workspace();
        for name in ["docs", "scripts", "crates/relay-server/src", ".git"] {
            fs::create_dir_all(workspace.path().join(name)).unwrap();
        }
        fs::write(
            workspace.path().join("scripts/prepare-webrtc.ps1"),
            "# native input\n",
        )
        .unwrap();
        let first = source_identity(workspace.path()).unwrap();
        for name in [
            "docs/readme.md",
            "scripts/deploy.ps1",
            "crates/relay-server/src/server.rs",
            ".git/foreign-session",
        ] {
            fs::write(workspace.path().join(name), "unrelated edit\n").unwrap();
            let next = source_identity(workspace.path()).unwrap();
            assert_eq!(
                first.sha256, next.sha256,
                "unrelated {name} entered the client digest"
            );
            assert_eq!(first.watched, next.watched);
        }
        assert!(!first.watched.contains(&workspace.path().join("scripts")));
        assert!(!first.watched.contains(&workspace.path().join(".git")));
        assert!(!first.watched.contains(&workspace.path().join("crates")));
        fs::write(
            workspace.path().join("crates/relay-protocol/src/lib.rs"),
            "// changed dependency\n",
        )
        .unwrap();
        assert_ne!(
            first.sha256,
            source_identity(workspace.path()).unwrap().sha256
        );
        fs::write(
            workspace.path().join("scripts/prepare-webrtc.ps1"),
            "# changed native recipe\n",
        )
        .unwrap();
        let recipe = source_identity(workspace.path()).unwrap();
        assert!(recipe
            .watched
            .contains(&workspace.path().join("scripts/prepare-webrtc.ps1")));
    }

    #[test]
    fn first_top_level_source_root_requires_an_explicit_inventory_edit() {
        let workspace = workspace();
        let initial = source_identity(workspace.path()).unwrap();
        let added = workspace.path().join("crates/relay-client/extra-source");
        fs::create_dir(&added).unwrap();
        fs::write(added.join("new.txt"), "synthetic future input\n").unwrap();
        assert_eq!(
            initial.sha256,
            source_identity(workspace.path()).unwrap().sha256
        );
        let inventory = workspace
            .path()
            .join("crates/relay-client/build_support/client-inputs.toml");
        let declared = fs::read_to_string(&inventory)
            .unwrap()
            .replace("\"build_support\"]", "\"build_support\",\"extra-source\"]");
        fs::write(&inventory, &declared).unwrap();
        let registered = source_identity(workspace.path()).unwrap();
        assert_ne!(initial.sha256, registered.sha256);
        assert!(registered.watched.contains(&added.join("new.txt")));
        fs::write(
            &inventory,
            declared.replace("\"extra-source\"", "\"missing-source\""),
        )
        .unwrap();
        assert!(
            source_identity(workspace.path()).is_err(),
            "Declared missing roots must fail, never cache silently"
        );
    }

    #[test]
    fn inherited_and_renamed_path_dependencies_are_parsed_not_scanned() {
        let workspace = workspace();
        let first = source_identity(workspace.path()).unwrap().sha256;
        let root_manifest = workspace.path().join("Cargo.toml");
        fs::write(&root_manifest, "[workspace]\nmembers=[\"crates/relay-client\",\"crates/relay-protocol\"]\n[workspace.dependencies]\nrenamed={package=\"relay-protocol\",path=\"crates/relay-protocol\"}\n").unwrap();
        fs::write(workspace.path().join("crates/relay-client/Cargo.toml"), "[package]\nname=\"relay-client\"\nversion=\"0.1.0\"\n[target.'cfg(windows)'.dependencies]\nrenamed = { workspace = true }\n").unwrap();
        let inherited = source_identity(workspace.path()).unwrap();
        assert_ne!(first, inherited.sha256);
        assert!(inherited
            .watched
            .contains(&workspace.path().join("crates/relay-protocol/src/lib.rs")));
        fs::write(workspace.path().join("crates/relay-protocol/Cargo.toml"), "[package]\nname=\"relay-protocol\"\nversion=\"0.1.0\"\n[dependencies]\nunsafe_path={path=\"../../../../outside\"}\n").unwrap();
        assert!(source_identity(workspace.path()).is_err());
    }

    #[test]
    fn git_worktree_and_guarded_common_directory_report_actual_identity() {
        let repository = workspace();
        let checkout = tempfile::tempdir().unwrap();
        let empty_config = tempfile::NamedTempFile::new().unwrap();
        let environment = std::env::vars_os().collect::<Vec<_>>();
        let run = |arguments: &[&std::ffi::OsStr]| {
            let output = git_command(
                repository.path(),
                &repository.path().join(".git"),
                &environment,
                empty_config.path(),
            )
            .args(arguments)
            .output()
            .unwrap();
            assert!(output.status.success(), "Owned Git worktree fixture failed: args={arguments:?}; status={}; stdout={}; stderr={}", output.status, String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        };
        let strings = |values: &[&str]| {
            values
                .iter()
                .map(std::ffi::OsStr::new)
                .map(std::ffi::OsStr::to_os_string)
                .collect::<Vec<_>>()
        };
        for arguments in [
            strings(&["init", "--quiet"]),
            strings(&["add", "--all"]),
            strings(&[
                "-c",
                "user.name=Relay Build Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "-m",
                "owned source",
            ]),
        ] {
            run(&arguments
                .iter()
                .map(OsString::as_os_str)
                .collect::<Vec<_>>());
        }
        run(&[
            "worktree".as_ref(),
            "add".as_ref(),
            "--quiet".as_ref(),
            "--detach".as_ref(),
            checkout.path().as_os_str(),
        ]);
        let original = git_identity(repository.path(), empty_config.path());
        let linked = git_identity(checkout.path(), empty_config.path());
        assert_ne!(linked.revision, "nogit");
        assert_eq!(linked, original);
        assert_eq!(linked.dirty, Some(false));
        fs::write(
            checkout.path().join("crates/relay-client/src/main.rs"),
            "// owned worktree edit\n",
        )
        .unwrap();
        assert_eq!(
            git_identity(checkout.path(), empty_config.path()).dirty,
            Some(true)
        );
        fs::write(
            repository.path().join(".git/config"),
            "[include]\npath = Z:/prohibited/not-read\n",
        )
        .unwrap();
        assert_eq!(
            git_identity(checkout.path(), empty_config.path()),
            GitIdentity::unavailable()
        );
        let mut invoked = false;
        assert_eq!(
            git_identity_with_runner(checkout.path(), empty_config.path(), |_, _| {
                invoked = true;
                None
            }),
            GitIdentity::unavailable()
        );
        assert!(
            !invoked,
            "Git must not start after unsupported metadata preflight"
        );
    }

    #[cfg(windows)]
    #[test]
    fn owned_directory_junction_is_rejected_before_its_target_is_read() {
        let workspace = workspace();
        fs::create_dir_all(workspace.path().join(".git/objects")).unwrap();
        fs::write(workspace.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        assert!(git_layout::inspect(workspace.path()).is_ok());
        let target = workspace.path().join("owned-target");
        fs::create_dir(&target).unwrap();
        fs::write(
            target.join("not-source.txt"),
            "do not fingerprint a redirect target",
        )
        .unwrap();
        // cmd's mklink treats forward slashes as option prefixes, while Rust's
        // filesystem APIs accept them. Give the shell only native components.
        let junction = workspace
            .path()
            .join("crates")
            .join("relay-client")
            .join("src")
            .join("redirect");
        use std::os::windows::process::CommandExt;
        let result = Command::new("cmd")
            .creation_flags(0x0800_0000)
            .args(["/D", "/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&target)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "Cannot create owned test junction; link={junction:?}; target={target:?}; status={}; stdout={}; stderr={}",
            result.status,
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr),
        );
        let descendant = junction.join("not-source.txt");
        let mut probes = Vec::new();
        assert!(reject_redirects_with(&descendant, |probe| {
            assert_ne!(
                probe, descendant,
                "Must never inspect a descendant behind the junction"
            );
            probes.push(probe.to_owned());
            fs::symlink_metadata(probe)
        })
        .is_err());
        assert_eq!(probes.last(), Some(&junction));
        assert!(
            source_scope::inside(
                workspace.path(),
                workspace.path(),
                "crates/relay-client/src/redirect/../../../relay-protocol"
            )
            .is_err(),
            "Raw junction prefix must not be erased by parent components"
        );
        let rejected = source_identity(workspace.path()).is_err();
        let mut visited = 0;
        // The same redirect must stop all-tree status before any Git child is started.
        assert!(git_layout::inspect(workspace.path()).is_err());
        let empty_config = tempfile::NamedTempFile::new().unwrap();
        let provenance = git_identity_with_runner(workspace.path(), empty_config.path(), |_, _| {
            visited += 1;
            None
        });
        assert_eq!(provenance, GitIdentity::unavailable());
        assert_eq!(visited, 0);
        // Remove the junction itself, never recursively delete its target.
        fs::remove_dir(&junction).unwrap();
        assert!(rejected);
        assert!(target.join("not-source.txt").is_file());
        let info = workspace.path().join(".git").join("info");
        let result = Command::new("cmd")
            .creation_flags(0x0800_0000)
            .args(["/D", "/C", "mklink", "/J"])
            .arg(&info)
            .arg(&target)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "Owned Git info junction creation failed: status={}; stderr={}",
            result.status,
            String::from_utf8_lossy(&result.stderr)
        );
        let mut visits = 0;
        let refused = git_identity_with_runner(workspace.path(), empty_config.path(), |_, _| {
            visits += 1;
            None
        });
        fs::remove_dir(&info).unwrap();
        assert_eq!(refused, GitIdentity::unavailable());
        assert_eq!(
            visits, 0,
            "Git must not start behind redirected info/exclude"
        );
    }

    #[cfg(unix)]
    #[test]
    fn redirected_source_is_rejected_without_reading_the_target() {
        let workspace = workspace();
        std::os::unix::fs::symlink(
            "does-not-exist",
            workspace.path().join("crates/relay-client/src/link.rs"),
        )
        .unwrap();
        assert!(source_identity(workspace.path()).is_err());
    }
}
