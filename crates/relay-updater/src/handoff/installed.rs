//! Read-only identification of the existing production installation. A portable
//! package has no installer record and is deliberately not an automatic target.
use crate::{storage, Release, Result};
use relay_protocol::installation::{inspect_directory, InstallationRecord, InstallationState};
use std::path::{Path, PathBuf};

const MANUAL: &str = "This copy of Relay cannot be updated automatically. Use the downloaded installer to update or repair Relay; your accounts and server data are kept.";

pub(super) struct Installed {
    pub directory: PathBuf,
    pub record: InstallationRecord,
}

pub(super) fn current(release: &Release) -> Result<Installed> {
    let executable = std::env::current_exe().map_err(|_| MANUAL)?;
    inspect(
        &executable,
        env!("CARGO_PKG_VERSION"),
        option_env!("RELAY_BUILD_SEQUENCE").unwrap_or(""),
        release,
    )
}

fn inspect(
    executable: &Path,
    version: &str,
    sequence: &str,
    release: &Release,
) -> Result<Installed> {
    storage::guard(executable).map_err(|_| MANUAL)?;
    let dist = executable.parent().ok_or(MANUAL)?;
    let named = |path: &Path, name: &str| {
        path.file_name()
            .and_then(|v| v.to_str())
            .is_some_and(|value| value.eq_ignore_ascii_case(name))
    };
    if !executable.is_file() || !named(executable, "relay.exe") || !named(dist, "dist") {
        return Err(MANUAL.into());
    }
    let directory = dist.parent().ok_or(MANUAL)?;
    let launcher = directory.join("relay.exe");
    storage::guard(&launcher).map_err(|_| MANUAL)?;
    if !launcher.is_file()
        || !matches!(
            inspect_directory(dist),
            InstallationState::Ready { record: None }
        )
    {
        return Err(MANUAL.into());
    }
    let InstallationState::Ready {
        record: Some(record),
    } = inspect_directory(directory)
    else {
        return Err(MANUAL.into());
    };
    // Engine only produces production releases with crate::APP_ID. Use that
    // already-approved release identity here so owned test installers can use a
    // separate product ID without weakening any production publisher decision.
    if record.schema != Some(1)
        || record.app_id != release.app_id
        || record.version != version
        || record.sequence != sequence
        || record.sequence >= release.sequence
    {
        return Err(MANUAL.into());
    }
    Ok(Installed {
        directory: directory.to_owned(),
        record,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::APP_ID;
    use std::fs;

    #[test]
    fn automatic_destination_requires_this_complete_installed_build() {
        let (_download, ready) = crate::tests::handoff_fixture();
        let mut release = ready.release;
        release.sequence = "20261026000000".into();
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("Relay");
        fs::create_dir_all(directory.join("dist")).unwrap();
        let executable = directory.join("dist/relay.exe");
        fs::write(&executable, b"owned inert client fixture").unwrap();
        fs::write(directory.join("relay.exe"), b"owned inert launcher fixture").unwrap();
        let inspect = || super::inspect(&executable, "0.10.0", "20261025000000", &release);
        assert!(
            inspect().is_err(),
            "portable copies must not become installer targets"
        );
        let marker = directory.join("relay-installation.ini");
        let record = format!("[Relay]\nSchema=1\nAppId={APP_ID}\nVersion=0.10.0\nSequence=20261025000000\nPayloadSHA256={}\n", "a".repeat(64));
        fs::write(&marker, &record).unwrap();
        assert_eq!(inspect().unwrap().directory, directory);
        assert!(super::inspect(&executable, "0.10.0", "20261024000000", &release).is_err());
        assert!(super::inspect(
            &directory.join("relay.exe"),
            "0.10.0",
            "20261025000000",
            &release
        )
        .is_err());
        fs::write(directory.join("relay-install-pending.ini"), &record).unwrap();
        assert!(
            inspect().is_err(),
            "partial installations require manual repair"
        );
        fs::remove_file(directory.join("relay-install-pending.ini")).unwrap();
        fs::write(
            &marker,
            record.replace(APP_ID, "68e09dab-cada-4de3-b9d3-e2a9bdb8036c"),
        )
        .unwrap();
        assert!(
            inspect().is_err(),
            "another product's marker is not a Relay installation"
        );
        assert_eq!(
            fs::read(&executable).unwrap(),
            b"owned inert client fixture"
        );
    }
}
