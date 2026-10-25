//! Explicit links between independent community accounts and one trusted OIDC
//! namespace. No local user, login password, permission or E2EE key is replaced.
use super::*;
use openidconnect::{
    core::{CoreClient, CoreIdToken, CoreJwsSigningAlgorithm, CoreProviderMetadata},
    AsyncHttpClient, ClientId, HttpRequest, HttpResponse, IssuerUrl, Nonce,
};
use serde_json::{json, Value};
use std::{future::Future, io, pin::Pin, time::Duration};
use url::Url;
use zeroize::Zeroizing;

const LINK_SECONDS: u64 = 180;
const MAX_RESPONSE: usize = 256 * 1024;
#[path = "account_login.rs"]
pub(super) mod login;

#[derive(Clone, Debug)]
pub struct Provider {
    issuer: Url,
    client_id: String,
    community_url: Url,
    #[cfg(test)]
    fixture_http: bool,
}

impl Provider {
    pub fn new(issuer: &str, client_id: &str, community_url: &str) -> Result<Self, String> {
        let issuer = Url::parse(issuer.trim().trim_end_matches('/'))
            .map_err(|_| "Invalid Relay account issuer")?;
        if (issuer.scheme() != "https" && !qa_loopback(&issuer))
            || !plain(&issuer)
            || issuer.host_str().is_none()
            || issuer.path() == "/"
            || client_id.is_empty()
            || client_id.len() > 128
            || client_id
                .chars()
                .any(|c| c.is_control() || c.is_whitespace())
        {
            return Err(
                "Relay accounts require an explicit HTTPS issuer and public client ID".into(),
            );
        }
        let community_url = destination_url(community_url)?;
        Ok(Self {
            issuer,
            client_id: client_id.into(),
            community_url,
            #[cfg(test)]
            fixture_http: false,
        })
    }

    pub fn configured() -> Result<Option<Self>, String> {
        let Some(issuer) = std::env::var("RELAY_ACCOUNT_ISSUER")
            .ok()
            .filter(|s| !s.trim().is_empty())
        else {
            return Ok(None);
        };
        let community = std::env::var("RELAY_ACCOUNT_COMMUNITY_URL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "http://127.0.0.1:4580".into());
        Self::new(
            &issuer,
            &std::env::var("RELAY_ACCOUNT_CLIENT_ID").unwrap_or_else(|_| "relay-desktop".into()),
            &community,
        )
        .map(Some)
    }

    pub(super) fn public(&self) -> relay_protocol::AccountProvider {
        relay_protocol::AccountProvider {
            issuer: self.issuer.to_string(),
            client_id: self.client_id.clone(),
            community_url: self.community_url.as_str().trim_end_matches('/').into(),
        }
    }

    fn allows(&self, url: &Url) -> bool {
        let secure = url.scheme() == "https" || qa_loopback(url);
        #[cfg(test)]
        let secure = secure
            || (self.fixture_http && url.scheme() == "http" && url.host_str() == Some("127.0.0.1"));
        secure && plain(url) && url.origin() == self.issuer.origin()
    }
}

/// QA builds only (`qa-loopback-identity`): an owned loopback identity service
/// on 127.0.0.1 with an explicit port may use plain HTTP. Never in a release.
fn qa_loopback(url: &Url) -> bool {
    cfg!(feature = "qa-loopback-identity")
        && url.scheme() == "http"
        && url.host_str() == Some("127.0.0.1")
        && url.port().is_some()
}

fn destination_url(community_url: &str) -> Result<Url, String> {
    let community_url = Url::parse(community_url.trim().trim_end_matches('/'))
        .map_err(|_| "Invalid canonical community URL")?;
    let host = community_url.host_str().unwrap_or_default();
    let iroh = host
        .strip_suffix(".iroh")
        .is_some_and(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()));
    if !plain(&community_url)
        || !(community_url.scheme() == "https"
            || (community_url.scheme() == "http"
                && (host == "127.0.0.1" || host == "[::1]" || iroh)))
    {
        return Err(
            "Account linking needs the community's canonical HTTPS, iroh or loopback URL".into(),
        );
    }
    Ok(community_url)
}

fn plain(url: &Url) -> bool {
    url.as_str().len() <= 4096
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
}
fn valid_handle(s: &str) -> bool {
    (3..=24).contains(&s.len())
        && s.as_bytes()[0].is_ascii_lowercase()
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

struct Transport {
    client: reqwest::Client,
    provider: Provider,
}
impl<'c> AsyncHttpClient<'c> for Transport {
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = io::Result<HttpResponse>> + Send + 'c>>;
    fn call(&'c self, request: HttpRequest) -> Self::Future {
        Box::pin(async move {
            let fail = || io::Error::other("Relay account verification request failed");
            let url = Url::parse(&request.uri().to_string()).map_err(|_| fail())?;
            if !self.provider.allows(&url) || request.body().len() > 16 * 1024 {
                return Err(fail());
            }
            let (parts, body) = request.into_parts();
            let mut response = self
                .client
                .request(parts.method, url)
                .headers(parts.headers)
                .body(body)
                .send()
                .await
                .map_err(|_| fail())?;
            if response.status().is_redirection()
                || response
                    .content_length()
                    .is_some_and(|n| n > MAX_RESPONSE as u64)
            {
                return Err(fail());
            }
            let mut result = openidconnect::http::Response::builder().status(response.status());
            for (key, value) in response.headers() {
                result = result.header(key, value);
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|_| fail())? {
                if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE {
                    return Err(fail());
                }
                bytes.extend_from_slice(&chunk);
            }
            result.body(bytes).map_err(|_| fail())
        })
    }
}

fn invalid() -> ApiError {
    bad("The Relay account proof is invalid or expired. Start linking again.")
}
fn unavailable() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "The Relay account service cannot be reached. Your local account is unchanged.",
    )
}

async fn verify(provider: &Provider, token: &str, nonce: &str) -> ApiResult<(String, String)> {
    verify_identity(provider, token, nonce, true).await
}
async fn verify_identity(
    provider: &Provider,
    token: &str,
    nonce: &str,
    recent_auth: bool,
) -> ApiResult<(String, String)> {
    let http = Transport {
        provider: provider.clone(),
        client: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|_| unavailable())?,
    };
    let metadata = CoreProviderMetadata::discover_async(
        IssuerUrl::new(provider.issuer.to_string()).map_err(|_| invalid())?,
        &http,
    )
    .await
    .map_err(|_| unavailable())?;
    let client = CoreClient::from_provider_metadata(
        metadata,
        ClientId::new(provider.client_id.clone()),
        None,
    );
    let verifier = client
        .id_token_verifier()
        .set_allowed_algs([
            CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256,
            CoreJwsSigningAlgorithm::EdDsa,
        ])
        .set_issue_time_verifier_fn(|issued| {
            let now = chrono::Utc::now();
            if issued > now + chrono::Duration::seconds(60)
                || issued < now - chrono::Duration::seconds(300)
            {
                Err("Identity is not recent".into())
            } else {
                Ok(())
            }
        });
    let token: CoreIdToken =
        serde_json::from_value(Value::String(token.to_owned())).map_err(|_| invalid())?;
    let nonce = Nonce::new(nonce.to_owned());
    let claims = token.claims(&verifier, &nonce).map_err(|_| invalid())?;
    let timestamp = chrono::Utc::now();
    if recent_auth
        && claims.auth_time().is_none_or(|time| {
            time < timestamp - chrono::Duration::seconds(300)
                || time > timestamp + chrono::Duration::seconds(60)
        })
    {
        return Err(invalid());
    }
    let subject = claims.subject().as_str();
    let handle = claims
        .preferred_username()
        .map(|s| s.as_str())
        .ok_or_else(invalid)?;
    if subject.is_empty()
        || subject.len() > 255
        || subject.chars().any(char::is_control)
        || !valid_handle(handle)
    {
        return Err(invalid());
    }
    Ok((subject.into(), handle.into()))
}

fn linked(db: &Connection, user: &str) -> ApiResult<Option<relay_protocol::LinkedIdentity>> {
    Ok(db
        .query_row(
            "SELECT issuer,handle FROM linked_identities WHERE user_id=?1",
            [user],
            |r| {
                Ok(relay_protocol::LinkedIdentity {
                    user_id: user.into(),
                    issuer: r.get(0)?,
                    handle: r.get(1)?,
                })
            },
        )
        .optional()?)
}
pub(super) fn visible(
    db: &Connection,
    provider: Option<&Provider>,
) -> ApiResult<Vec<relay_protocol::LinkedIdentity>> {
    let Some(provider) = provider else {
        return Ok(vec![]);
    };
    let mut q=db.prepare("SELECT l.user_id,l.issuer,l.handle FROM linked_identities l JOIN membership m ON m.user_id=l.user_id WHERE l.issuer=?1 AND m.status='active' ORDER BY l.user_id")?;
    let rows = q
        .query_map([provider.issuer.as_str()], |r| {
            Ok(relay_protocol::LinkedIdentity {
                user_id: r.get(0)?,
                issuer: r.get(1)?,
                handle: r.get(2)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}
type PrivateJson = (HeaderMap, Json<Value>);
fn private(value: Value) -> PrivateJson {
    let mut headers = HeaderMap::new();
    headers.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    (headers, Json(value))
}
fn status_value(state: &AppState, db: &Connection, auth: &Auth) -> ApiResult<Value> {
    Ok(
        json!({"provider":state.0.config.account_provider.as_ref().map(|p|p.public()),"linked":linked(db,&auth.user.id)?}),
    )
}
pub(super) async fn status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<PrivateJson> {
    let auth = state.auth(&headers)?;
    Ok(private(status_value(&state, &lock(&state.0.db), &auth)?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Change {
    action: String,
    #[serde(default)]
    password: Zeroizing<String>,
    #[serde(default)]
    code: Zeroizing<String>,
    #[serde(default)]
    challenge_id: String,
    #[serde(default)]
    id_token: Zeroizing<String>,
}

pub(super) async fn change(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<Change>,
) -> ApiResult<PrivateJson> {
    let auth = state.auth(&headers)?;
    if input.action == "link" {
        if input.id_token.is_empty()
            || input.id_token.len() > 32 * 1024
            || Uuid::parse_str(&input.challenge_id).is_err()
        {
            return Err(invalid());
        }
        let provider = state
            .0
            .config
            .account_provider
            .as_ref()
            .ok_or_else(|| bad("This community has not configured Relay account linking."))?;
        let _permit = state
            .0
            .identity_jobs
            .clone()
            .try_acquire_owned()
            .map_err(|_| {
                ApiError::new(
                    StatusCode::TOO_MANY_REQUESTS,
                    "Account verification is busy. Try linking again shortly.",
                )
            })?;
        // Spend the session-bound challenge once, before any provider request.
        // A failed or interrupted proof needs a fresh local reauthentication.
        let (nonce, hash) = {
            let mut db = lock(&state.0.db);
            let tx = db.transaction()?;
            let row:Option<(String,String,String)>=tx.query_row("SELECT nonce,password_hash,issuer FROM identity_link_challenges WHERE user_id=?1 AND session_hash=?2 AND challenge_id=?3 AND expires>?4",params![auth.user.id,auth.token_hash,input.challenge_id,now()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
            let (nonce, hash, issuer) = row.ok_or_else(invalid)?;
            authenticator::current_auth(&tx, &auth, &hash)?;
            if issuer != provider.issuer.as_str() {
                return Err(invalid());
            }
            tx.execute(
                "DELETE FROM identity_link_challenges WHERE user_id=?1",
                [&auth.user.id],
            )?;
            tx.commit()?;
            (nonce, hash)
        };
        let (subject, handle) = verify(provider, &input.id_token, &nonce).await?;
        let value = {
            let mut db = lock(&state.0.db);
            let tx = db.transaction()?;
            authenticator::current_auth(&tx, &auth, &hash)?;
            if linked(&tx, &auth.user.id)?.is_some() {
                return Err(bad(
                    "This account is already linked. Unlink it before changing its Relay identity.",
                ));
            }
            let taken:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM linked_identities WHERE issuer=?1 AND (subject=?2 OR handle=?3))",params![provider.issuer.as_str(),subject,handle],|r|r.get(0))?;
            if taken {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "This Relay identity is already linked to another account in this community.",
                ));
            }
            tx.execute(
                "INSERT INTO linked_identities VALUES(?1,?2,?3,?4,?5)",
                params![
                    auth.user.id,
                    provider.issuer.as_str(),
                    subject,
                    handle,
                    now()
                ],
            )?;
            roles::audit(&tx, &auth.user.id, "account.linked", &auth.user.id)?;
            let value = status_value(&state, &tx, &auth)?;
            tx.commit()?;
            value
        };
        state.notify("members", None, None, None);
        return Ok(private(value));
    }
    if !matches!(input.action.as_str(), "begin" | "unlink") {
        return Err(bad("Choose link or unlink."));
    }
    if input.action == "begin" && state.0.config.account_provider.is_none() {
        return Err(bad(
            "This community has not configured Relay account linking.",
        ));
    }
    let hash = authenticator::reauthenticate(&state, &auth.user.id, &input.password).await?;
    let proof = authenticator::prepare(&state, &auth.user.id, input.password, &input.code).await?;
    let result = {
        let mut db = lock(&state.0.db);
        let tx = db.transaction()?;
        authenticator::current_auth(&tx, &auth, &hash)?;
        authenticator::consume(&tx, &auth.user.id, proof.as_ref())?;
        if input.action == "unlink" {
            tx.execute(
                "DELETE FROM linked_identities WHERE user_id=?1",
                [&auth.user.id],
            )?;
            tx.execute(
                "DELETE FROM identity_link_challenges WHERE user_id=?1",
                [&auth.user.id],
            )?;
            roles::audit(&tx, &auth.user.id, "account.unlinked", &auth.user.id)?;
            let result = status_value(&state, &tx, &auth)?;
            tx.commit()?;
            result
        } else {
            if linked(&tx, &auth.user.id)?.is_some() {
                return Err(bad("This community account is already linked."));
            }
            let provider = state.0.config.account_provider.as_ref().unwrap();
            let challenge = id();
            let mut entropy = [0u8; 32];
            OsRng.fill_bytes(&mut entropy);
            let destination = format!(
                "{:x}",
                Sha256::digest(
                    provider
                        .community_url
                        .as_str()
                        .trim_end_matches('/')
                        .as_bytes()
                )
            );
            let nonce = format!(
                "relay-link-v1:{}:{}:{}:{}",
                server_id(&tx)?,
                auth.user.id,
                destination,
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(entropy)
            );
            tx.execute(
                "DELETE FROM identity_link_challenges WHERE expires<=?1",
                [now()],
            )?;
            tx.execute("INSERT INTO identity_link_challenges VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(user_id) DO UPDATE SET session_hash=excluded.session_hash,challenge_id=excluded.challenge_id,nonce=excluded.nonce,password_hash=excluded.password_hash,expires=excluded.expires,issuer=excluded.issuer",params![auth.user.id,auth.token_hash,challenge,nonce,hash,now().saturating_add(LINK_SECONDS),provider.issuer.as_str()])?;
            tx.commit()?;
            json!({"challenge_id":challenge,"nonce":nonce,"expires_in":LINK_SECONDS,"provider":provider.public()})
        }
    };
    if input.action == "unlink" {
        state.notify("members", None, None, None);
    }
    Ok(private(result))
}

#[cfg(test)]
mod tests;
