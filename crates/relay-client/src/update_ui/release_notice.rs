//! Public release discovery only. A GitHub notice never authorizes downloading
//! or executing an installer through the verified updater.
use chrono::{NaiveDate, NaiveDateTime};
use serde::Deserialize;
use std::{io::Read, sync::mpsc, time::Duration};

const API: &str = "https://api.github.com/repos/TheBengine/Relay/releases?per_page=10";
const REPOSITORY: &str = "https://github.com/TheBengine/Relay";
const MAX_RESPONSE: u64 = 512 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Notice {
    pub tag: String,
    pub url: String,
    pub version: String,
    pub sequence: String,
    pub prerelease: bool,
}

pub(super) enum Poll {
    Running,
    Finished(Result<Option<Notice>, String>),
}

pub(super) struct Worker {
    result: mpsc::Receiver<Result<Option<Notice>, String>>,
}

impl Worker {
    pub fn start(
        installed_version: String,
        installed_sequence: String,
        include_previews: bool,
        wake: impl FnOnce() + Send + 'static,
    ) -> Result<Self, String> {
        // An unknown local build must not be compared using publication dates
        // or the package version alone. Validate before creating any worker.
        let installed = installed(&installed_version, &installed_sequence)?;
        let (sender, result) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("relay-release-notice".into())
            .spawn(move || {
                let result = fetch().and_then(|bytes| {
                    select_release(&bytes, installed, &installed_sequence, include_previews)
                });
                // No thread join or polling timer on the UI. Dropping the
                // receiver abandons the bounded request and suppresses wakeup.
                if sender.send(result).is_ok() {
                    wake();
                }
            })
            .map_err(|_| "Couldn't start the release check.".to_owned())?;
        Ok(Self { result })
    }

    pub fn poll(&mut self) -> Poll {
        match self.result.try_recv() {
            Ok(result) => Poll::Finished(result),
            Err(mpsc::TryRecvError::Empty) => Poll::Running,
            Err(mpsc::TryRecvError::Disconnected) => {
                Poll::Finished(Err("The release check stopped unexpectedly.".into()))
            }
        }
    }
}

fn fetch() -> Result<Vec<u8>, String> {
    let client = reqwest::blocking::Client::builder()
        .https_only(true)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        // Total deadline also bounds a response whose body stops arriving.
        .timeout(Duration::from_secs(15))
        .user_agent("Relay-release-notice")
        .build()
        .map_err(|_| "Couldn't prepare the release check.".to_owned())?;
    let response = client
        .get(API)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .map_err(|_| "Couldn't reach Relay's releases. Try again later.".to_owned())?;
    if response.status() != reqwest::StatusCode::OK {
        return Err("Relay's releases are unavailable. Try again later.".into());
    }
    if response
        .content_length()
        .is_some_and(|size| size > MAX_RESPONSE)
    {
        return Err("The release response is too large.".into());
    }
    let mut bytes = Vec::new();
    response
        .take(MAX_RESPONSE + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "Couldn't read Relay's releases. Try again later.".to_owned())?;
    if bytes.len() as u64 > MAX_RESPONSE {
        return Err("The release response is too large.".into());
    }
    Ok(bytes)
}

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    body: Option<String>,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    state: String,
    size: u64,
    browser_download_url: String,
}

type Version = [u32; 3];

fn number(value: &str) -> Option<u32> {
    if value.is_empty()
        || !value.bytes().all(|byte| byte.is_ascii_digit())
        || (value.len() > 1 && value.starts_with('0'))
    {
        return None;
    }
    value.parse().ok()
}

fn version(value: &str) -> Option<Version> {
    let mut parts = value.split('.');
    let result = [
        number(parts.next()?)?,
        number(parts.next()?)?,
        number(parts.next()?)?,
    ];
    parts.next().is_none().then_some(result)
}

fn valid_sequence(value: &str) -> bool {
    value.len() == 14
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && NaiveDateTime::parse_from_str(value, "%Y%m%d%H%M%S").is_ok()
}

fn installed(version_text: &str, sequence: &str) -> Result<Version, String> {
    version(version_text)
        .filter(|_| valid_sequence(sequence))
        .ok_or_else(|| "This build has no comparable release identity.".into())
}

fn tag_version(tag: &str, prerelease: bool) -> Option<Version> {
    if tag.len() > 80 {
        return None;
    }
    let tag = tag.strip_prefix('v')?;
    if prerelease {
        let (base, preview) = tag.split_once("-preview.")?;
        let (date, revision) = preview.split_once('.')?;
        if date.len() != 8
            || !date.bytes().all(|byte| byte.is_ascii_digit())
            || NaiveDate::parse_from_str(date, "%Y%m%d").is_err()
            || number(revision)? == 0
        {
            return None;
        }
        version(base)
    } else {
        version(tag)
    }
}

fn build_sequence(body: &str) -> Option<&str> {
    if body.len() > 32 * 1024 {
        return None;
    }
    let mut markers = body
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("Build:"));
    let identity = markers.next()?.strip_prefix("Build: ")?;
    if markers.next().is_some() {
        return None;
    }
    let (sequence, digest) = identity.split_once('-')?;
    (valid_sequence(sequence)
        && digest.len() == 12
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
    .then_some(sequence)
}

fn select_release(
    bytes: &[u8],
    installed: Version,
    installed_sequence: &str,
    include_previews: bool,
) -> Result<Option<Notice>, String> {
    if bytes.len() as u64 > MAX_RESPONSE {
        return Err("The release response is too large.".into());
    }
    let releases: Vec<GithubRelease> = serde_json::from_slice(bytes)
        .map_err(|_| "Relay's release response could not be read.".to_owned())?;
    if releases.len() > 10 {
        return Err("Relay's release response exceeds the requested limit.".into());
    }
    let mut newest: Option<Notice> = None;
    for release in releases {
        if release.draft || (release.prerelease && !include_previews) {
            continue;
        }
        let Some(release_version) = tag_version(&release.tag_name, release.prerelease) else {
            continue;
        };
        let Some(sequence) = release.body.as_deref().and_then(build_sequence) else {
            continue;
        };
        if release_version < installed || sequence <= installed_sequence {
            continue;
        }
        let download = format!(
            "{REPOSITORY}/releases/download/{}/Relay-Setup.exe",
            release.tag_name
        );
        if !release.assets.iter().any(|asset| {
            asset.name == "Relay-Setup.exe"
                && asset.state == "uploaded"
                && asset.size > 0
                && asset.browser_download_url == download
        }) {
            continue;
        }
        if newest
            .as_ref()
            .is_some_and(|old| old.sequence.as_str() >= sequence)
        {
            continue;
        }
        newest = Some(Notice {
            url: format!("{REPOSITORY}/releases/tag/{}", release.tag_name),
            tag: release.tag_name,
            version: release_version
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join("."),
            sequence: sequence.into(),
            prerelease: release.prerelease,
        });
    }
    Ok(newest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn release(tag: &str, sequence: &str, prerelease: bool) -> Value {
        json!({
            "tag_name": tag, "draft": false, "prerelease": prerelease,
            "body": format!("Notes\n\nBuild: {sequence}-75addcc63de1  \nSource checkpoint: `abc`"),
            "assets": [{"name": "Relay-Setup.exe", "state": "uploaded", "size": 123,
                "browser_download_url": format!("{REPOSITORY}/releases/download/{tag}/Relay-Setup.exe")}]
        })
    }

    fn select(releases: Vec<Value>, previews: bool) -> Option<Notice> {
        select_release(
            &serde_json::to_vec(&releases).unwrap(),
            [0, 10, 0],
            "20261025160437",
            previews,
        )
        .unwrap()
    }

    #[test]
    fn actual_build_identity_overrides_tag_order_and_publication_time() {
        let current = release("v0.10.0-preview.20261025.6", "20261025160437", true);
        let mut republished = release("v0.10.0-preview.20261026.9", "20261025160437", true);
        republished["published_at"] = json!("2099-01-01T00:00:00Z");
        let newer = release("v0.10.0-preview.20261026.1", "20261026080000", true);
        let result = select(vec![republished, newer, current], true).unwrap();
        assert_eq!(result.tag, "v0.10.0-preview.20261026.1");
        assert_eq!(
            result.url,
            format!("{REPOSITORY}/releases/tag/{}", result.tag)
        );
        assert_eq!(result.sequence, "20261026080000");
    }

    #[test]
    fn previews_drafts_older_builds_and_lower_versions_are_not_stable_updates() {
        let preview = release("v0.10.1-preview.20261026.1", "20261026090000", true);
        assert!(select(vec![preview], false).is_none());
        let stable = release("v0.10.1", "20261026080000", false);
        let mut draft = release("v0.10.2", "20261026100000", false);
        draft["draft"] = json!(true);
        let older_build = release("v0.11.0", "20261025080000", false);
        let lower_version = release("v0.9.0", "20261026120000", false);
        assert_eq!(
            select(vec![draft, older_build, lower_version, stable], false)
                .unwrap()
                .tag,
            "v0.10.1"
        );
    }

    #[test]
    fn incomplete_or_ambiguous_identity_and_unofficial_assets_are_ignored() {
        let good = release("v0.10.1", "20261026080000", false);
        for body in [
            "",
            "Build: 20261026080000",
            "Build: 20261326080000-75addcc63de1",
            "Build: 20261026080000-75addcc63de1\nBuild: 20261027080000-75addcc63de1",
        ] {
            let mut candidate = good.clone();
            candidate["body"] = json!(body);
            assert!(select(vec![candidate], true).is_none());
        }
        for field in ["assets", "url", "state"] {
            let mut candidate = good.clone();
            match field {
                "assets" => candidate["assets"] = json!([]),
                "url" => {
                    candidate["assets"][0]["browser_download_url"] =
                        json!("https://example.org/Relay-Setup.exe")
                }
                _ => candidate["assets"][0]["state"] = json!("new"),
            }
            assert!(select(vec![candidate], true).is_none());
        }
        for tag in [
            "v0.10.1/../../other",
            "v00.10.1",
            "v0.10.1+extra",
            "v0.10.1-preview.20261325.1",
        ] {
            assert!(select(
                vec![release(tag, "20261026080000", tag.contains("preview"))],
                true
            )
            .is_none());
        }
        assert!(installed("0.10.0", "").is_err());
        assert!(installed("0.10.0", "20261326080000").is_err());
        assert!(select_release(b"not json", [0, 10, 0], "20261025160437", true).is_err());
    }
}
