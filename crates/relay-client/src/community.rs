use crate::network::Target;
use relay_protocol::User;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct SavedCommunity {
    pub url: String,
    #[serde(default)]
    pub server_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub last_target: Option<Target>,
    /// How to reach a community which was joined without an account-free address.
    /// Holds a secret; kept with the other preferences and never printed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iroh: Option<crate::iroh_route::SavedIroh>,
    /// The protection revision this person accepted for sending without
    /// end-to-end encryption. It applies to that one revision only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_protection_accepted: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct CommunityInfo {
    pub url: String,
    pub server_id: String,
    pub name: String,
    pub user: User,
    pub current: bool,
}

pub fn normalize_url(input: &str) -> Result<String, String> {
    if input.len() > 2048 {
        return Err("The server address is too long.".into());
    }
    let mut url = url::Url::parse(input.trim())
        .map_err(|_| "Enter a complete HTTP or HTTPS server address.".to_string())?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(
            "Use an HTTP or HTTPS address without credentials, queries or fragments.".into(),
        );
    }
    let path = url.path().trim_end_matches('/').to_owned();
    url.set_path(&path);
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

/// Shared account proofs need an authenticated route (or this PC's loopback).
/// Keep the form and the network worker on the same admission rule.
pub(crate) fn account_destination(raw: &str) -> Result<String, String> {
    let normalized = normalize_url(raw)?;
    let url = url::Url::parse(&normalized).map_err(|_| "Invalid server address.")?;
    let host = url.host_str().unwrap_or_default();
    let iroh = host
        .strip_suffix(".iroh")
        .is_some_and(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()));
    if url.scheme() == "https"
        || (url.scheme() == "http" && (host == "127.0.0.1" || host == "[::1]" || iroh))
    {
        Ok(normalized)
    } else {
        Err("This server needs an HTTPS or iroh address to use your Relay account. You can still use its existing server account below.".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn addresses_are_canonical_and_never_embed_credentials() {
        assert_eq!(
            normalize_url(" HTTP://EXAMPLE.COM:80/// ").unwrap(),
            "http://example.com"
        );
        assert!(normalize_url("https://name:secret@example.com").is_err());
        assert!(normalize_url("https://example.com?token=secret").is_err());
        assert!(normalize_url("file:///private").is_err());
        assert_ne!(
            normalize_url("https://example.com/a").unwrap(),
            normalize_url("https://example.com/b").unwrap()
        );
    }
}
