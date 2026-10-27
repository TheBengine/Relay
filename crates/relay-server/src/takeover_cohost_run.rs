//! The co-host's takeover work as hidden companion modes of `relay-server`,
//! like `--replica-pull`: the app starts the installed companion, writes one JSON
//! object to its standard input and closes it, and reads one JSON line back.
//! The app keeps the route to the host alive and passes the resolved address;
//! the companion never discovers addresses itself. The session token and feed
//! key travel only on that pipe and in the community's private folder.
//!
//! * `--takeover-copy DIR`: enroll, consent, fetch the copy (resuming), install
//!   it and report it verified. One run moves as far as it can.
//! * `--takeover-follow DIR`: while protection is on, follow live changes until
//!   the app stops it.
use crate::takeover_cohost::CohostKey;
use crate::takeover_setup::Manifest;
use relay_protocol::{handover::RoutePin, takeover::AuthoritySet};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};

const INPUT_LIMIT: u64 = 16 * 1024;
const PART_LIMIT: usize = 1024 * 1024;
const UNREACHABLE: &str = "The host could not be reached; the copy continues next time";

/// What the app writes to the companion's standard input.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CopyInput {
    /// The resolved address of the host (the app keeps the route alive).
    pub url: String,
    pub token: String,
    pub server_id: String,
    pub user_id: String,
    pub pin: RoutePin,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FollowInput {
    pub url: String,
}

/// Where one run ended; the companion prints it as one JSON line.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", content = "reason", rename_all = "snake_case")]
pub enum Outcome {
    WaitingForSecondCoHost,
    UpToDate,
    /// Protection is on and this copy follows live changes.
    Following,
    RoleEnded,
    Stopped(String),
}

impl Outcome {
    pub fn text(&self) -> String {
        match self {
            Outcome::WaitingForSecondCoHost => "Joined. Waiting for a second approved co-host before the host signs the takeover agreement.".into(),
            Outcome::UpToDate => "Data copy up to date: a verified full copy is kept on this computer. Ready to take over stays off until live replication exists.".into(),
            Outcome::Following => "Following live changes: every change on the host is confirmed by this computer or the other co-host before it is accepted.".into(),
            Outcome::RoleEnded => "Your co-host role ended, so the copy on this computer was removed.".into(),
            Outcome::Stopped(why) => format!("Stopped: {why}"),
        }
    }
}

pub fn feed_key_path(dir: &Path) -> PathBuf {
    dir.join("feed.key")
}

/// True when this computer follows the host live (protection is on).
pub fn following(dir: &Path) -> bool {
    feed_key_path(dir).is_file()
}

fn read_input<T: for<'de> Deserialize<'de>>() -> Result<T, String> {
    let mut raw = Vec::new();
    std::io::stdin()
        .take(INPUT_LIMIT + 1)
        .read_to_end(&mut raw)
        .map_err(|_| "No input".to_string())?;
    if raw.len() as u64 > INPUT_LIMIT {
        return Err("Input too large".into());
    }
    serde_json::from_slice(&raw).map_err(|_| "Invalid input".into())
}

/// `--takeover-copy DIR`: reads its input, runs once, prints the outcome.
pub fn copy_stdin(dir: &Path) -> Outcome {
    let input: CopyInput = match read_input() {
        Ok(i) => i,
        Err(e) => return Outcome::Stopped(e),
    };
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(r) => r,
        Err(_) => return Outcome::Stopped(UNREACHABLE.into()),
    };
    runtime.block_on(copy(input, dir))
}

/// `--takeover-follow DIR`: reads its input and follows until killed.
pub fn follow_stdin(dir: &Path) -> Result<(), String> {
    let input: FollowInput = read_input()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    runtime.block_on(follow(input.url, dir.to_owned(), None));
    Ok(())
}

struct Http {
    client: reqwest::Client,
    url: String,
    token: String,
}

impl Http {
    async fn json(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value, Outcome> {
        let mut request = self
            .client
            .request(method, format!("{}{path}", self.url))
            .bearer_auth(&self.token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request
            .send()
            .await
            .map_err(|_| Outcome::Stopped(UNREACHABLE.into()))?;
        let status = response.status();
        let value: Value = response.json().await.unwrap_or(Value::Null);
        if status == reqwest::StatusCode::FORBIDDEN {
            return Err(Outcome::RoleEnded);
        }
        if !status.is_success() {
            return Err(Outcome::Stopped(
                value["error"].as_str().unwrap_or("The host refused the request").to_owned(),
            ));
        }
        Ok(value)
    }
    async fn part(&self, snapshot: &str, index: usize) -> Result<Vec<u8>, String> {
        let response = self
            .client
            .get(format!("{}/api/hosting/takeover/snapshot/{snapshot}/parts/{index}", self.url))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|_| UNREACHABLE.to_string())?;
        if !response.status().is_success() {
            return Err("The host stopped sending the copy".into());
        }
        let bytes = response.bytes().await.map_err(|_| UNREACHABLE.to_string())?;
        if bytes.len() > PART_LIMIT {
            return Err("A part of the copy is too large".into());
        }
        Ok(bytes.to_vec())
    }
}

fn client() -> Option<reqwest::Client> {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(60))
        .build()
        .ok()
}

/// One complete attempt: enroll, consent, copy, install, report.
pub async fn copy(input: CopyInput, dir: &Path) -> Outcome {
    let Some(client) = client() else {
        return Outcome::Stopped(UNREACHABLE.into());
    };
    let key = match CohostKey::open(dir) {
        Ok(k) => k,
        Err(e) => return Outcome::Stopped(e.to_string()),
    };
    let http = Http {
        client,
        url: input.url.trim_end_matches('/').to_owned(),
        token: input.token.clone(),
    };
    let outcome = match steps(&http, &key, &input).await {
        Ok(o) | Err(o) => o,
    };
    if outcome == Outcome::RoleEnded {
        key.forget_copy();
        let _ = std::fs::remove_file(feed_key_path(dir));
    }
    outcome
}

async fn steps(http: &Http, key: &CohostKey, input: &CopyInput) -> Result<Outcome, Outcome> {
    if following(key.dir()) && key.installed().is_some() {
        // Protection is on: the copy follows live changes and is never replaced.
        return Ok(Outcome::Following);
    }
    let enroll = key
        .enroll(&input.server_id, &input.user_id)
        .map_err(|e| Outcome::Stopped(e.to_string()))?;
    let cohort = http
        .json(
            reqwest::Method::POST,
            "/api/hosting/takeover/enroll",
            Some(serde_json::to_value(enroll).map_err(|e| Outcome::Stopped(e.to_string()))?),
        )
        .await?;
    if cohort["set"].is_null() {
        return Ok(Outcome::WaitingForSecondCoHost);
    }
    let set: AuthoritySet = serde_json::from_value(cohort["set"].clone())
        .map_err(|_| Outcome::Stopped("The takeover agreement could not be read".into()))?;
    let me = key.identity();
    let mine = cohort["members"]
        .as_array()
        .and_then(|m| m.iter().find(|m| m["identity"]["host_id"] == me.host_id.as_str()).cloned())
        .ok_or_else(|| Outcome::Stopped("The host did not list this computer".into()))?;
    let accept = key
        .consent(&set, &input.pin)
        .map_err(|e| Outcome::Stopped(e.to_string()))?;
    if mine["accept"].is_null() {
        http.json(
            reqwest::Method::POST,
            "/api/hosting/takeover/consent",
            Some(json!({ "accept": accept })),
        )
        .await?;
    }
    let prepared: Option<Manifest> = serde_json::from_value(mine["snapshot"].clone()).ok();
    let manifest = match prepared {
        Some(m) if mine["verified_ms"].is_null() => m,
        Some(m)
            if key.installed().as_deref() == Some(m.snapshot_id.as_str())
                && m.tracked
                && cohort["head"].as_i64() == Some(m.head) =>
        {
            return Ok(Outcome::UpToDate);
        }
        _ => {
            let value = http
                .json(reqwest::Method::POST, "/api/hosting/takeover/snapshot", None)
                .await?;
            serde_json::from_value(value)
                .map_err(|_| Outcome::Stopped("The copy description could not be read".into()))?
        }
    };
    // Fetch the parts first (async), then hand them to the checked, resumable pull.
    let mut parts: Vec<Option<Vec<u8>>> = vec![None; manifest.parts.len()];
    let incoming = key.dir().join("incoming").join(&manifest.snapshot_id);
    for (index, expected) in manifest.parts.iter().enumerate() {
        let have = std::fs::read(incoming.join(format!("{index}.part")))
            .ok()
            .is_some_and(|b| {
                use sha2::{Digest, Sha256};
                format!("{:x}", Sha256::digest(&b)) == *expected
            });
        if !have {
            match http.part(&manifest.snapshot_id, index).await {
                Ok(bytes) => parts[index] = Some(bytes),
                Err(_) => break,
            }
        }
    }
    let image = key
        .pull(&manifest, |index| {
            parts[index]
                .take()
                .ok_or_else(|| UNREACHABLE.to_string())
        })
        .map_err(|e| Outcome::Stopped(e.to_string()))?;
    key.install(&set, &accept, &manifest, &image)
        .map_err(|e| Outcome::Stopped(e.to_string()))?;
    let reply = http
        .json(
            reqwest::Method::POST,
            "/api/hosting/takeover/verified",
            Some(json!({"snapshot_id": manifest.snapshot_id, "sha256": manifest.sha256})),
        )
        .await?;
    if let Some(feed_key) = reply["feed_key"].as_str() {
        crate::handover_transfer::publish_new(&feed_key_path(key.dir()), feed_key.as_bytes())
            .map_err(|e| Outcome::Stopped(e.to_string()))?;
        return Ok(Outcome::Following);
    }
    Ok(Outcome::UpToDate)
}

/// Follows live changes. Stops when `stop` says so (tests) or never (companion).
pub async fn follow(url: String, dir: PathBuf, stop: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>) {
    use std::sync::atomic::Ordering;
    let Ok(feed_key) = std::fs::read_to_string(feed_key_path(&dir)) else {
        return;
    };
    let feed_key = feed_key.trim().to_owned();
    let Ok(key) = CohostKey::open(&dir) else { return };
    let Ok(mut follower) = key.follower() else { return };
    let Some(client) = client() else { return };
    let url = url.trim_end_matches('/').to_owned();
    let post = |path: &'static str, body: Vec<u8>| {
        let request = client
            .post(format!("{url}{path}"))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body);
        async move {
            let response = request.send().await.ok()?;
            if !response.status().is_success() {
                return None;
            }
            response.bytes().await.ok().map(|b| b.to_vec())
        }
    };
    while !stop.as_ref().is_some_and(|s| s.load(Ordering::Acquire)) {
        let Some(reply) = post("/api/hosting/takeover/feed", follower.request(&feed_key)).await else {
            tokio::time::sleep(Duration::from_secs(2)).await;
            continue;
        };
        if let Ok(answers) = follower.apply(&reply, &feed_key) {
            let _ = post("/api/hosting/takeover/answers", answers).await;
        }
    }
}

#[cfg(test)]
mod tests;
