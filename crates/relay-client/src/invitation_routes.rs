//! The extra ways an invitation can offer to reach the same community.
//!
//! An invitation always names one address. Version 4 may also list further
//! direct addresses and an account-free (iroh) route, so the friend's Relay can
//! try them and use the fastest one that works. Addresses and the iroh endpoint
//! are not secret; the iroh admission token is, and only ever sits in the link
//! fragment.
use crate::community::normalize_url;

/// Most extra addresses one invitation may carry.
pub const MAX_ALTERNATIVES: usize = 4;
/// The port every Relay community listens on. Plain HTTP is only accepted here.
const RELAY_PORT: u16 = 4580;

/// A route which needs no account: the host is found by its key, not an address.
#[derive(Clone, PartialEq, Eq)]
pub struct IrohRoute {
    /// The host's public key, 64 lowercase hex characters.
    pub endpoint_id: String,
    /// An optional relay server (HTTPS) which helps when no direct path exists.
    pub relay: Option<String>,
    /// Proves the invitation to the host before any request is forwarded.
    pub token: String,
}

impl std::fmt::Debug for IrohRoute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IrohRoute")
            .field("endpoint_id", &self.endpoint_id)
            .field("token", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl IrohRoute {
    pub fn new(endpoint_id: &str, relay: Option<&str>, token: &str) -> Result<Self, String> {
        if endpoint_id.len() != 64
            || !endpoint_id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err("This invitation has an invalid account-free route.".into());
        }
        if token.len() < 22
            || token.len() > 64
            || !token
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
        {
            return Err("This invitation has an invalid account-free route.".into());
        }
        let relay = match relay {
            None => None,
            Some(value) => Some(valid_relay_url(value)?),
        };
        Ok(Self {
            endpoint_id: endpoint_id.into(),
            relay,
            token: token.into(),
        })
    }
}

fn valid_relay_url(value: &str) -> Result<String, String> {
    let bad = || "This invitation has an invalid relay address.".to_string();
    if value.len() > 200 {
        return Err(bad());
    }
    let url = url::Url::parse(value).map_err(|_| bad())?;
    if url.scheme() != "https"
        || url.host_str().is_none_or(str::is_empty)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !matches!(url.path(), "" | "/")
        || url.port().is_some_and(|p| p == 0)
    {
        return Err(bad());
    }
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

/// A direct address an invitation may list: HTTPS, or plain HTTP on Relay's own
/// port. Anything else is refused, so a hostile invitation cannot make Relay
/// contact arbitrary ports on the friend's network.
pub fn valid_direct_address(input: &str) -> Result<String, String> {
    let bad = || "This invitation lists an invalid server address.".to_string();
    let normalized = normalize_url(input).map_err(|_| bad())?;
    let url = url::Url::parse(&normalized).map_err(|_| bad())?;
    let host = url.host_str().ok_or_else(bad)?;
    if host.is_empty() || normalized.len() > 200 {
        return Err(bad());
    }
    let ok = match url.scheme() {
        "http" => url.port() == Some(RELAY_PORT),
        "https" => {
            url.port().is_none() || url.port() == Some(443) || url.port() == Some(RELAY_PORT)
        }
        _ => false,
    };
    if !ok {
        return Err(bad());
    }
    Ok(normalized)
}

/// The list in the link: addresses separated by single spaces.
pub fn parse_alternatives(value: &str) -> Result<Vec<String>, String> {
    let mut found: Vec<String> = Vec::new();
    for part in value.split(' ').filter(|p| !p.is_empty()) {
        let address = valid_direct_address(part)?;
        if !found.contains(&address) {
            found.push(address);
        }
    }
    if found.len() > MAX_ALTERNATIVES {
        return Err("This invitation lists too many server addresses.".into());
    }
    Ok(found)
}

pub fn join_alternatives(addresses: &[String]) -> String {
    addresses.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const TOKEN: &str = "abcdefghijklmnopqrstuvwxyzABCDEF0123456789_-";

    #[test]
    fn iroh_routes_need_a_real_key_a_strong_token_and_an_https_relay() {
        let route = IrohRoute::new(ID, Some("https://relay.example.org/"), TOKEN).unwrap();
        assert_eq!(route.relay.as_deref(), Some("https://relay.example.org"));
        assert!(IrohRoute::new(ID, None, TOKEN).is_ok());
        for id in [
            "",
            &ID[..63],
            &ID.to_uppercase(),
            &format!("{}g", &ID[..63]),
            &format!("{ID}0"),
        ] {
            assert!(IrohRoute::new(id, None, TOKEN).is_err(), "{id}");
        }
        for token in [
            "",
            "short",
            &"x".repeat(65),
            "has space has space has space",
            "ünicode-token-ünicode-token",
        ] {
            assert!(IrohRoute::new(ID, None, token).is_err(), "{token}");
        }
        for relay in [
            "http://relay.example.org",
            "https://user:pw@relay.example.org",
            "https://relay.example.org/path",
            "https://relay.example.org?x=1",
            "https://relay.example.org#frag",
            "ftp://relay.example.org",
            "https://",
            "not a url",
        ] {
            assert!(IrohRoute::new(ID, Some(relay), TOKEN).is_err(), "{relay}");
        }
        assert!(
            !format!("{route:?}").contains(TOKEN),
            "the token must never be printed"
        );
    }

    #[test]
    fn direct_addresses_are_https_or_plain_http_on_relays_own_port() {
        for ok in [
            "http://192.168.1.50:4580",
            "http://[2001:db8::1]:4580",
            "https://chat.example.org",
            "https://chat.example.org:443/",
            "https://chat.example.org:4580",
            "https://chat.example.org/relay",
        ] {
            assert!(valid_direct_address(ok).is_ok(), "{ok}");
        }
        for bad in [
            "http://192.168.1.50",
            "http://192.168.1.50:22",
            "http://192.168.1.50:4581",
            "https://chat.example.org:8443",
            "ftp://chat.example.org",
            "http://user:pw@192.168.1.50:4580",
            "http://192.168.1.50:4580?x=1",
            "http://192.168.1.50:4580#x",
            "",
            "not an address",
        ] {
            assert!(valid_direct_address(bad).is_err(), "{bad}");
        }
        assert!(valid_direct_address(&format!("https://{}.example.org", "a".repeat(300))).is_err());
    }

    #[test]
    fn the_list_is_deduplicated_bounded_and_all_or_nothing() {
        let list =
            parse_alternatives("http://10.0.0.2:4580 http://10.0.0.2:4580  https://a.example.org")
                .unwrap();
        assert_eq!(list, ["http://10.0.0.2:4580", "https://a.example.org"]);
        assert_eq!(
            join_alternatives(&list),
            "http://10.0.0.2:4580 https://a.example.org"
        );
        assert!(parse_alternatives("").unwrap().is_empty());
        // One bad entry rejects the whole invitation instead of being skipped silently.
        assert!(parse_alternatives("http://10.0.0.2:4580 http://10.0.0.3:22").is_err());
        let five: Vec<String> = (1..=5).map(|i| format!("http://10.0.0.{i}:4580")).collect();
        assert!(parse_alternatives(&five.join(" ")).is_err());
        assert!(parse_alternatives(&five[..4].join(" ")).is_ok());
    }
}
