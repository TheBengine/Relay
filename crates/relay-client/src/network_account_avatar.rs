//! Account avatar authority is the configured Friends service, never a community.
//! Copies contain only image bytes; each service receives its own session token.
use super::extended::TransferPermit;
use super::*;
use base64::Engine;
use std::io::Read;

const CAPABILITY: &str = "account-avatar-v1";
const MAX_RESPONSE: u64 = 400 * 1024;

#[derive(Clone)]
pub(super) struct Endpoint {
    origin: CommandOrigin,
    token: zeroize::Zeroizing<String>,
}
impl From<&Connection> for Endpoint {
    fn from(c: &Connection) -> Self {
        Self {
            origin: c.into(),
            token: zeroize::Zeroizing::new(c.session.token.clone()),
        }
    }
}
impl Endpoint {
    fn request(&self, http: &Client, method: reqwest::Method, path: &str) -> RequestBuilder {
        http.request(
            method,
            crate::embedded_network::endpoint(&format!("{}{path}", self.origin.url)),
        )
        .bearer_auth(self.token.as_str())
        .timeout(Duration::from_secs(8))
    }
    fn bootstrap(&self, http: &Client) -> Result<Bootstrap, String> {
        let b: Bootstrap = parse(
            self.request(http, reqwest::Method::GET, "/api/bootstrap")
                .send()
                .map_err(network_error)?,
        )?;
        validate_bootstrap_identity(&self.origin.server_id, &self.origin.user_id, &b)?;
        Ok(b)
    }
}

fn linked<'a>(b: &'a Bootstrap, issuer: &str) -> Option<&'a str> {
    b.linked_identities
        .iter()
        .find(|i| i.user_id == b.user.id && i.issuer == issuer)
        .map(|i| i.handle.as_str())
        .filter(|h| !h.is_empty())
}
fn same_account(source: &Bootstrap, target: &Bootstrap, issuer: &str) -> bool {
    source.capabilities.iter().any(|c| c == "people-v1")
        && linked(source, issuer).is_some()
        && linked(target, issuer).is_some()
}
fn supported(b: &Bootstrap) -> Result<(), String> {
    if b.capabilities.iter().any(|c| c == CAPABILITY) {
        Ok(())
    } else {
        Err("Update Friends and this server to use your account avatar everywhere.".into())
    }
}

#[derive(serde::Deserialize, PartialEq, Eq)]
struct Account {
    issuer: String,
    binding: String,
}
fn account(http: &Client, endpoint: &Endpoint, issuer: &str) -> Result<Account, String> {
    let response = checked(
        endpoint
            .request(http, reqwest::Method::GET, "/api/profile/account")
            .send()
            .map_err(network_error)?,
    )?;
    let mut bytes = Vec::new();
    response
        .take(4097)
        .read_to_end(&mut bytes)
        .map_err(|_| "Cannot confirm account avatar ownership.")?;
    if bytes.len() > 4096 {
        return Err("Invalid account avatar ownership response.".into());
    }
    let identity: Option<Account> =
        serde_json::from_slice(&bytes).map_err(|_| "Invalid account avatar ownership response.")?;
    identity
        .filter(|a| {
            a.issuer == issuer
                && a.binding.len() == 64
                && a.binding.bytes().all(|b| b.is_ascii_hexdigit())
        })
        .ok_or_else(|| "This server is not linked to your Relay account.".into())
}

#[derive(Clone, serde::Deserialize)]
struct Avatar {
    configured: bool,
    version: u64,
    image: Option<String>,
}
impl Avatar {
    fn bytes(&self) -> Result<Option<Vec<u8>>, String> {
        self.image
            .as_ref()
            .map(|value| {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(value)
                    .map_err(|_| "The account avatar could not be read.")?;
                if bytes.len() > 300 * 1024
                    || image::guess_format(&bytes).ok() != Some(image::ImageFormat::Png)
                {
                    return Err("The account avatar is invalid.".into());
                }
                Ok(bytes)
            })
            .transpose()
    }
}
fn read_avatar(
    http: &Client,
    source: &Endpoint,
    cached: Option<&Avatar>,
    binding: &str,
) -> Result<Avatar, String> {
    let mut request = source
        .request(http, reqwest::Method::GET, "/api/profile/avatar")
        .header("X-Relay-Account-Binding", binding);
    if let Some(cached) = cached {
        request = request.header("If-None-Match", format!("\"profile-{}\"", cached.version));
    }
    let response = request.send().map_err(network_error)?;
    if response.status() == reqwest::StatusCode::NOT_MODIFIED {
        return cached
            .cloned()
            .ok_or_else(|| "Missing account avatar cache.".into());
    }
    let mut bytes = Vec::new();
    checked(response)?
        .take(MAX_RESPONSE + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "Cannot read account avatar.")?;
    if bytes.len() as u64 > MAX_RESPONSE {
        return Err("The account avatar is too large.".into());
    }
    let avatar: Avatar =
        serde_json::from_slice(&bytes).map_err(|_| "Invalid account avatar response.")?;
    avatar.bytes()?;
    if !avatar.configured && avatar.image.is_some() {
        return Err("Invalid account avatar state.".into());
    }
    Ok(avatar)
}

fn write_avatar(
    http: &Client,
    target: &Endpoint,
    version: u64,
    bytes: Option<Vec<u8>>,
    binding: &str,
) -> Result<User, String> {
    let request = match bytes {
        Some(bytes) => target
            .request(http, reqwest::Method::PUT, "/api/profile/avatar")
            .header("Content-Type", "application/octet-stream")
            .body(bytes),
        None => target.request(http, reqwest::Method::DELETE, "/api/profile/avatar"),
    };
    let user: User = parse(
        request
            .header("If-Match", format!("\"profile-{version}\""))
            .header("X-Relay-Account-Binding", binding)
            .send()
            .map_err(network_error)?,
    )?;
    if user.id != target.origin.user_id {
        return Err("The avatar response did not match your account.".into());
    }
    Ok(user)
}

/// Called inside the existing bounded profile transfer, before any avatar write.
/// A linked account never falls back to a server-only change when Friends is offline.
pub(super) fn save(
    http: &Client,
    target: &Endpoint,
    source: Option<&Endpoint>,
    issuer: Option<&str>,
    avatar: &ProfileAvatarChange,
    cancelled: impl Fn() -> bool,
) -> Result<Option<User>, String> {
    if matches!(avatar, ProfileAvatarChange::Keep) || issuer.is_none() {
        return Ok(None);
    }
    let issuer = issuer.unwrap();
    let target_bootstrap = target.bootstrap(http)?;
    if linked(&target_bootstrap, issuer).is_none() {
        return Ok(None);
    }
    let source = source.ok_or("Connect to Friends before changing your account avatar.")?;
    let source_bootstrap = source.bootstrap(http)?;
    if !same_account(&source_bootstrap, &target_bootstrap, issuer) {
        return Err("Friends and this server are not signed in to the same Relay account.".into());
    }
    supported(&source_bootstrap)?;
    supported(&target_bootstrap)?;
    let source_account = account(http, source, issuer)?;
    if source_account != account(http, target, issuer)? {
        return Err("Friends and this server belong to different Relay accounts.".into());
    }
    let bytes = match avatar {
        ProfileAvatarChange::Prepared(bytes) => {
            if bytes.is_empty()
                || bytes.len() > 2 * 1024 * 1024
                || image::guess_format(bytes).ok() != Some(image::ImageFormat::Png)
            {
                return Err("The cropped avatar is invalid.".into());
            }
            Some(bytes.clone())
        }
        ProfileAvatarChange::Remove => None,
        _ => return Ok(None),
    };
    if cancelled() {
        return Err("Profile save cancelled.".into());
    }
    let saved = write_avatar(
        http,
        source,
        source_bootstrap.user.profile.version,
        bytes.clone(),
        &source_account.binding,
    )?;
    if source.origin == target.origin {
        return Ok(Some(saved));
    }
    if cancelled() {
        return Err("Account avatar saved. This server will catch up when connected.".into());
    }
    write_avatar(
        http,
        target,
        target_bootstrap.user.profile.version,
        bytes,
        &source_account.binding,
    )
    .map(Some)
    .map_err(|e| format!("Account avatar saved. This server will retry automatically: {e}"))
}

type Stamp = (CommandOrigin, u64, u64);

fn synchronize(
    http: &Client,
    source: &Endpoint,
    targets: Vec<Endpoint>,
    issuer: &str,
    cached: Option<&Avatar>,
    mut applied: Vec<Stamp>,
    cancelled: impl Fn() -> bool,
) -> Result<(Avatar, Vec<Stamp>), String> {
    let active = || {
        if cancelled() {
            Err("Avatar sync cancelled.".to_string())
        } else {
            Ok(())
        }
    };
    active()?;
    let b = source.bootstrap(http)?;
    supported(&b)?;
    if !same_account(&b, &b, issuer) {
        return Err("Friends account is not linked.".into());
    }
    active()?;
    let source_account = account(http, source, issuer)?;
    active()?;
    let avatar = read_avatar(http, source, cached, &source_account.binding)?;
    if !avatar.configured {
        return Ok((avatar, Vec::new()));
    }
    for target in targets {
        active()?;
        let Ok(target_b) = target.bootstrap(http) else {
            continue;
        };
        if !same_account(&b, &target_b, issuer) || supported(&target_b).is_err() {
            continue;
        }
        active()?;
        if account(http, &target, issuer).ok().as_ref() != Some(&source_account) {
            continue;
        }
        let stamp = (
            target.origin.clone(),
            avatar.version,
            target_b.user.profile.version,
        );
        if applied.contains(&stamp) {
            continue;
        }
        active()?;
        // Recheck the authority before each conditional write. Never retry a
        // rejected destination with newer preconditions and old source bytes.
        let fresh = read_avatar(http, source, Some(&avatar), &source_account.binding)?;
        if fresh.version != avatar.version {
            break;
        }
        active()?;
        if let Ok(user) = write_avatar(
            http,
            &target,
            target_b.user.profile.version,
            avatar.bytes()?,
            &source_account.binding,
        ) {
            applied.retain(|(origin, _, _)| *origin != target.origin);
            if applied.len() >= 64 {
                applied.remove(0);
            }
            applied.push((target.origin, avatar.version, user.profile.version));
        }
    }
    Ok((avatar, applied))
}

#[derive(Default)]
pub(super) struct Sync {
    next: Option<Instant>,
    pending: Option<mpsc::Receiver<SyncResult>>,
    cache: Option<(CommandOrigin, Avatar)>,
    applied: Vec<Stamp>,
    pub generation: Arc<AtomicU64>,
    cursor: usize,
}
struct SyncResult {
    epoch: u64,
    generation: u64,
    cache: Option<(CommandOrigin, Avatar)>,
    applied: Vec<Stamp>,
}
impl Sync {
    pub fn invalidate(&mut self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.next = None;
        self.cache = None;
        self.applied.clear();
    }
}

impl Worker {
    pub(super) fn account_avatar_source(&self) -> (Option<String>, Option<Endpoint>) {
        let Some(provider) = crate::central_account::Provider::configured()
            .ok()
            .flatten()
        else {
            return (None, None);
        };
        let issuer = provider.issuer_url().to_string();
        let source = provider.people_url().ok().and_then(|url| {
            self.connection
                .as_ref()
                .filter(|c| c.url == url)
                .or_else(|| self.saved_connections.get(&url).map(|c| &c.connection))
                .map(Endpoint::from)
        });
        (Some(issuer), source)
    }
    pub(super) fn poll_account_avatar(&mut self) {
        if let Some(rx) = &self.account_avatar.pending {
            match rx.try_recv() {
                Ok(done) => {
                    self.account_avatar.pending = None;
                    if done.epoch == self.transfer_epoch.load(Ordering::SeqCst)
                        && done.generation == self.account_avatar.generation.load(Ordering::SeqCst)
                    {
                        self.account_avatar.cache = done.cache;
                        self.account_avatar.applied = done.applied;
                        self.refresh_pending.store(true, Ordering::Relaxed);
                        for c in self.saved_connections.values() {
                            c.dirty.store(true, Ordering::Relaxed);
                        }
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.account_avatar.pending = None;
                }
                Err(mpsc::TryRecvError::Empty) => return,
            }
        }
        if self
            .account_avatar
            .next
            .is_some_and(|next| Instant::now() < next)
            || self.transfers.keys().any(|k| k.starts_with("profile-"))
        {
            return;
        }
        self.account_avatar.next = Some(Instant::now() + Duration::from_secs(30));
        let (Some(issuer), Some(source)) = self.account_avatar_source() else {
            return;
        };
        let mut targets: Vec<_> = self
            .connection
            .iter()
            .chain(self.saved_connections.values().map(|c| &c.connection))
            .filter(|c| c.url != source.origin.url)
            .take(64)
            .map(Endpoint::from)
            .collect();
        if targets.is_empty() {
            return;
        }
        let count = targets.len();
        targets.sort_by(|a, b| a.origin.url.cmp(&b.origin.url));
        targets.rotate_left(self.account_avatar.cursor % count);
        targets.truncate(4);
        self.account_avatar.cursor = (self.account_avatar.cursor + targets.len()) % count;
        let Ok(http) = self.transfer_client() else {
            return;
        };
        let Ok(permit) = TransferPermit::acquire(self.transfer_active.clone()) else {
            return;
        };
        let epoch = self.transfer_epoch.load(Ordering::SeqCst);
        let epoch_ref = self.transfer_epoch.clone();
        let generation = self.account_avatar.generation.load(Ordering::SeqCst);
        let generation_ref = self.account_avatar.generation.clone();
        let cache = self
            .account_avatar
            .cache
            .clone()
            .filter(|(origin, _)| *origin == source.origin);
        let applied = self.account_avatar.applied.clone();
        let (tx, rx) = mpsc::channel();
        if std::thread::Builder::new()
            .name("relay-account-avatar".into())
            .spawn(move || {
                let _permit = permit;
                let cancelled = || {
                    epoch_ref.load(Ordering::SeqCst) != epoch
                        || generation_ref.load(Ordering::SeqCst) != generation
                };
                let mut done = SyncResult {
                    epoch,
                    generation,
                    cache: None,
                    applied: Vec::new(),
                };
                if let Ok((avatar, applied)) = synchronize(
                    &http,
                    &source,
                    targets,
                    &issuer,
                    cache.as_ref().map(|(_, a)| a),
                    applied,
                    cancelled,
                ) {
                    done.cache = Some((source.origin.clone(), avatar));
                    done.applied = applied;
                }
                let _ = tx.send(done);
            })
            .is_ok()
        {
            self.account_avatar.pending = Some(rx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn bootstrap(user: &str, issuer: &str, handle: &str, people: bool) -> Bootstrap {
        let mut b = Bootstrap::default();
        b.user.id = user.into();
        b.linked_identities.push(LinkedIdentity {
            user_id: user.into(),
            issuer: issuer.into(),
            handle: handle.into(),
        });
        if people {
            b.capabilities.push("people-v1".into());
        }
        b
    }

    #[test]
    fn automatic_avatar_sync_handles_new_pc_removal_races_and_cancellation() {
        use super::super::tests::{MockHttp, Reply};
        use serde_json::json;
        for case in [
            "new-pc",
            "remove",
            "unconfigured",
            "source-changed",
            "target-changed",
            "cancelled",
            "other-account",
        ] {
            let cancel = Arc::new(AtomicBool::new(false));
            let reads = Arc::new(AtomicUsize::new(0));
            let mut png = std::io::Cursor::new(Vec::new());
            image::DynamicImage::new_rgb8(12, 12)
                .write_to(&mut png, image::ImageFormat::Png)
                .unwrap();
            let image = base64::engine::general_purpose::STANDARD.encode(png.into_inner());
            let fixture = |id: &'static str, people: bool| {
                let cancel = cancel.clone();
                let reads = reads.clone();
                let image = image.clone();
                MockHttp::with_request(move |r| {
                    assert_eq!(
                        r.authorization.as_deref(),
                        Some(format!("Bearer {id}-token").as_str())
                    );
                    match r.path.as_str() {
                        "/api/bootstrap" => {
                            if !people && case == "cancelled" {
                                cancel.store(true, Ordering::SeqCst);
                            }
                            let mut b = bootstrap(id, "issuer", "handle", people);
                            b.server_id = format!("{id}-server");
                            b.user.profile.version = 5;
                            b.capabilities.push(CAPABILITY.into());
                            Reply::json(200, serde_json::to_value(b).unwrap())
                        }
                        "/api/profile/account" => Reply::json(
                            200,
                            json!({"issuer":"issuer","binding":
                            if !people && case=="other-account" {"b".repeat(64)} else {"a".repeat(64)}}),
                        ),
                        "/api/profile/avatar" if people => {
                            assert_eq!(r.method, "GET");
                            let revision = if reads.fetch_add(1, Ordering::SeqCst) > 0
                                && case == "source-changed"
                            {
                                6
                            } else {
                                5
                            };
                            Reply::json(
                                200,
                                json!({"configured":case!="unconfigured","version":revision,
                                "image":if case=="remove" || case=="unconfigured" {None} else {Some(image.clone())}}),
                            )
                        }
                        "/api/profile/avatar" => {
                            assert_eq!(r.method, if case == "remove" { "DELETE" } else { "PUT" });
                            if case == "target-changed" {
                                return Reply::json(412, json!({"error":"changed"}));
                            }
                            let mut user = User {
                                id: id.into(),
                                ..Default::default()
                            };
                            user.profile.version = 6;
                            Reply::json(200, serde_json::to_value(user).unwrap())
                        }
                        _ => panic!("Unexpected route"),
                    }
                })
            };
            let friends = fixture("friends", true);
            let community = fixture("community", false);
            let endpoint = |url: &str, id: &str| Endpoint {
                origin: CommandOrigin {
                    url: url.into(),
                    server_id: format!("{id}-server"),
                    user_id: id.into(),
                },
                token: zeroize::Zeroizing::new(format!("{id}-token")),
            };
            let source = endpoint(&friends.url, "friends");
            let target = endpoint(&community.url, "community");
            let http = Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap();
            let result = synchronize(
                &http,
                &source,
                vec![target],
                "issuer",
                None,
                Vec::new(),
                || cancel.load(Ordering::SeqCst),
            );
            let writes = community
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r.path == "/api/profile/avatar")
                .count();
            assert_eq!(
                writes,
                usize::from(matches!(case, "new-pc" | "remove" | "target-changed")),
                "{case}"
            );
            if case == "cancelled" {
                assert!(result.is_err());
            } else {
                assert_eq!(
                    result.unwrap().1.len(),
                    usize::from(matches!(case, "new-pc" | "remove")),
                    "{case}"
                );
            }
        }
    }

    #[test]
    fn account_avatar_save_uses_own_tokens_and_rejects_reused_handles() {
        use super::super::tests::{MockHttp, Reply};
        use serde_json::json;
        for (different_account, remove, cancelled) in [
            (false, false, false),
            (false, true, false),
            (true, false, false),
            (false, false, true),
        ] {
            let fixture = |id: &'static str, people: bool, binding: &'static str| {
                MockHttp::with_request(move |r| {
                    assert_eq!(
                        r.authorization.as_deref(),
                        Some(format!("Bearer {id}-token").as_str())
                    );
                    match r.path.as_str() {
                        "/api/bootstrap" => {
                            let mut b = bootstrap(id, "issuer", "same-handle", people);
                            b.server_id = format!("{id}-server");
                            b.user.profile.version = 5;
                            b.capabilities.push(CAPABILITY.into());
                            Reply::json(200, serde_json::to_value(b).unwrap())
                        }
                        "/api/profile/account" => Reply::json(
                            200,
                            json!({"issuer":"issuer","binding":binding.repeat(64)}),
                        ),
                        "/api/profile/avatar" => {
                            assert_eq!(r.method, if remove { "DELETE" } else { "PUT" });
                            assert_eq!(r.body.is_empty(), remove);
                            let mut user = User {
                                id: id.into(),
                                ..Default::default()
                            };
                            user.profile.version = 6;
                            Reply::json(200, serde_json::to_value(user).unwrap())
                        }
                        _ => panic!("Unexpected avatar route"),
                    }
                })
            };
            let friends = fixture("friends", true, "a");
            let community = fixture(
                "community",
                false,
                if different_account { "b" } else { "a" },
            );
            let endpoint = |url: &str, id: &str| Endpoint {
                origin: CommandOrigin {
                    url: url.into(),
                    server_id: format!("{id}-server"),
                    user_id: id.into(),
                },
                token: zeroize::Zeroizing::new(format!("{id}-token")),
            };
            let source = endpoint(&friends.url, "friends");
            let target = endpoint(&community.url, "community");
            let mut png = std::io::Cursor::new(Vec::new());
            image::DynamicImage::new_rgb8(12, 12)
                .write_to(&mut png, image::ImageFormat::Png)
                .unwrap();
            let avatar = if remove {
                ProfileAvatarChange::Remove
            } else {
                ProfileAvatarChange::Prepared(png.into_inner())
            };
            let http = Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap();
            let result = save(
                &http,
                &target,
                Some(&source),
                Some("issuer"),
                &avatar,
                || cancelled,
            );
            if different_account || cancelled {
                assert!(result.is_err());
                assert!(!friends
                    .requests
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|r| r.method == "PUT" || r.method == "DELETE"));
                assert!(!community
                    .requests
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|r| r.method == "PUT" || r.method == "DELETE"));
            } else {
                assert_eq!(result.unwrap().unwrap().id, "community");
                assert_eq!(
                    friends
                        .requests
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|r| r.path == "/api/profile/avatar")
                        .count(),
                    1
                );
                assert_eq!(
                    community
                        .requests
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|r| r.path == "/api/profile/avatar")
                        .count(),
                    1
                );
            }
        }
    }

    #[test]
    fn avatar_copies_require_same_confirmed_account_at_trusted_issuer() {
        let source = bootstrap("friends-user", "issuer", "account", true);
        assert!(same_account(
            &source,
            &bootstrap("community-user", "issuer", "account", false),
            "issuer"
        ));
        // A changed handle is allowed; the own-only endpoint must still match immutable account bindings.
        assert!(same_account(
            &source,
            &bootstrap("community-user", "issuer", "other", false),
            "issuer"
        ));
        assert!(
            Account {
                issuer: "issuer".into(),
                binding: "a".repeat(64)
            } != Account {
                issuer: "issuer".into(),
                binding: "b".repeat(64)
            }
        );
        assert!(!same_account(
            &source,
            &bootstrap("community-user", "other", "account", false),
            "issuer"
        ));
        assert!(!same_account(&source, &Bootstrap::default(), "issuer"));
        let mut impostor = bootstrap("community-user", "issuer", "account", false);
        impostor.linked_identities[0].user_id = "someone-else".into();
        assert!(!same_account(&source, &impostor, "issuer"));
        assert!(!same_account(&impostor, &source, "issuer"));
    }
    #[test]
    fn avatar_cache_rejects_invalid_images_and_edits_cancel_old_work() {
        let avatar = Avatar {
            configured: true,
            version: 1,
            image: Some("invalid".into()),
        };
        assert!(avatar.bytes().is_err());
        let mut sync = Sync { next: Some(Instant::now()), ..Default::default() };
        sync.invalidate();
        assert!(sync.next.is_none());
        assert_eq!(sync.generation.load(Ordering::SeqCst), 1);
    }
}
