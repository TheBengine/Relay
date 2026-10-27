//! Invitations are pasted inside Relay. The bearer code lives in the fragment,
//! never in an HTTP address, preferences, health requests, or diagnostic output.
use crate::community::normalize_url;
use crate::invitation_routes::{self, IrohRoute};
use crate::invite_proof::InviteProof;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use relay_protocol::handover::RoutePin;
use std::collections::HashMap;

pub const BROWSER_INVITATION_URL: &str = "https://thebengine.github.io/Relay/";
pub const MAX_INPUT_BYTES: usize = 4096 + BROWSER_INVITATION_URL.len();

#[derive(Clone, PartialEq, Eq)]
pub struct Invitation {
    pub server: String,
    pub server_id: String,
    pub code: String,
    /// An optional provider share capability, kept in the URI fragment only.
    pub network_share: Option<String>,
    /// Optional device verification (R3.2): the one-time secret and the host's
    /// device, kept in the URI fragment only.
    pub proof: Option<InviteProof>,
    /// Version 4: further direct addresses for the same community. The friend's
    /// Relay tries every route and uses the fastest one that works.
    pub alt: Vec<String>,
    /// Version 4: an account-free route; its token sits in the fragment only.
    pub iroh: Option<IrohRoute>,
    /// Version 5: a trust root from the host who shared this link. It grants no
    /// account access and must be proved afresh before login. Fragment only.
    pub authority: Option<RoutePin>,
}

impl Invitation {
    /// A bare home-network address is not an internet invitation. An offered
    /// iroh or non-local alternative keeps a combined invitation usable away.
    pub fn same_network_only(&self) -> bool {
        self.iroh.is_none()
            && local_network_address(&self.server)
            && self
                .alt
                .iter()
                .all(|address| local_network_address(address))
    }

    pub fn new(server: &str, server_id: &str, code: &str) -> Result<Self, String> {
        let server = normalize_url(server)?;
        let server_id = uuid::Uuid::parse_str(server_id)
            .map_err(|_| "This invitation has an invalid community identity.")?
            .to_string();
        if code.is_empty()
            || code.len() > 128
            || !code
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
        {
            return Err("This invitation has an invalid invite code.".into());
        }
        Ok(Self {
            server,
            server_id,
            code: code.into(),
            network_share: None,
            proof: None,
            alt: Vec::new(),
            iroh: None,
            authority: None,
        })
    }

    pub fn parse(input: &str) -> Result<Self, String> {
        if input.len() > MAX_INPUT_BYTES {
            return Err("This invitation is too long.".into());
        }
        let link = url::Url::parse(input.trim()).map_err(|_| "This invitation is not valid.")?;
        if link.scheme() == "https"
            && link.host_str() == Some("thebengine.github.io")
            && matches!(link.path(), "/Relay" | "/Relay/")
            && link.username().is_empty()
            && link.password().is_none()
            && link.port().is_none()
            && link.query().is_none()
        {
            let fields = fields_with(
                link.fragment().unwrap_or_default(),
                &["v", "server", "server_id", "invite"],
                &[
                    "alt", "iroh", "irelay", "access", "iid", "hd", "hf", "vk", "it", "hp",
                ],
            )?;
            let mut query = url::form_urlencoded::Serializer::new(String::new());
            let mut fragment = url::form_urlencoded::Serializer::new(String::new());
            for name in ["v", "server", "server_id", "alt", "iroh", "irelay"] {
                if let Some(value) = fields.get(name) {
                    query.append_pair(name, value);
                }
            }
            for name in ["invite", "access", "iid", "hd", "hf", "vk", "it", "hp"] {
                if let Some(value) = fields.get(name) {
                    fragment.append_pair(name, value);
                }
            }
            // The wrapper carries no authority. Reuse every original version,
            // route and proof check, including the native broker's 4096-byte cap.
            return Self::parse(&format!(
                "relay://join?{}#{}",
                query.finish(),
                fragment.finish()
            ));
        }
        if input.len() > 4096 {
            return Err("This invitation is too long.".into());
        }
        // Windows' URL protocol launch rewrites `relay://join?…` to
        // `relay://join/?…` before Relay sees it; both are the same invitation.
        if link.scheme() != "relay"
            || link.host_str() != Some("join")
            || !matches!(link.path(), "" | "/")
            || link.port().is_some()
            || !link.username().is_empty()
            || link.password().is_some()
        {
            return Err("Use the Relay invitation link your friend sent you.".into());
        }
        let query = fields_with(
            link.query().unwrap_or_default(),
            &["v", "server", "server_id"],
            &["alt", "iroh", "irelay"],
        )?;
        let version = query.get("v").map(String::as_str);
        // Versions 4 and 5 may list further routes.
        if !matches!(version, Some("4" | "5"))
            && ["alt", "iroh", "irelay"]
                .iter()
                .any(|key| query.contains_key(*key))
        {
            return Err("This invitation contains unexpected or repeated fields.".into());
        }
        // (required fields, optional fields) in the fragment.
        let (required, optional): (&[&str], &[&str]) = match version {
            Some("1") => (&["invite"], &[]),
            Some("2") => (&["invite", "access"], &[]),
            // Version 3 adds device verification and may also carry `access`.
            Some("3") => (&["invite", "iid", "hd", "hf", "vk"], &["access"]),
            // Version 4 may list further routes; every other part is optional.
            Some("4") => (&["invite"], &["access", "iid", "hd", "hf", "vk", "it"]),
            Some("5") => (
                &["invite", "hp"],
                &["access", "iid", "hd", "hf", "vk", "it"],
            ),
            _ => {
                return Err(
                    "This invitation version is not supported. Update Relay and try again.".into(),
                )
            }
        };
        let fragment = fields_with(link.fragment().unwrap_or_default(), required, optional)?;
        let mut invitation = Self::new(&query["server"], &query["server_id"], &fragment["invite"])?;
        if matches!(version, Some("4" | "5")) {
            if let Some(list) = query.get("alt") {
                invitation.alt = invitation_routes::parse_alternatives(list)?
                    .into_iter()
                    .filter(|address| *address != invitation.server)
                    .collect();
            }
            match (query.get("iroh"), fragment.get("it")) {
                (Some(id), Some(token)) => {
                    invitation.iroh = Some(IrohRoute::new(
                        id,
                        query.get("irelay").map(String::as_str),
                        token,
                    )?)
                }
                (None, None) if !query.contains_key("irelay") => {}
                _ => return Err("This invitation has an incomplete account-free route.".into()),
            }
        }
        if let Some(share) = fragment.get("access") {
            invitation = invitation.with_network_share(share)?;
        }
        if version == Some("3") {
            invitation.proof = Some(InviteProof::from_link_fields(
                &fragment["iid"],
                &fragment["hd"],
                &fragment["hf"],
                &fragment["vk"],
            )?);
        }
        if matches!(version, Some("4" | "5")) {
            let present = ["iid", "hd", "hf", "vk"]
                .iter()
                .filter(|key| fragment.contains_key(**key))
                .count();
            match present {
                0 => {}
                4 => {
                    invitation.proof = Some(InviteProof::from_link_fields(
                        &fragment["iid"],
                        &fragment["hd"],
                        &fragment["hf"],
                        &fragment["vk"],
                    )?)
                }
                _ => return Err("This invitation is incomplete.".into()),
            }
        }
        if let Some(encoded) = fragment.get("hp") {
            if encoded.len() > 512 {
                return Err("This invitation has an invalid host identity.".into());
            }
            let raw = URL_SAFE_NO_PAD
                .decode(encoded)
                .map_err(|_| "This invitation has an invalid host identity.")?;
            let pin: RoutePin = serde_json::from_slice(&raw)
                .map_err(|_| "This invitation has an invalid host identity.")?;
            invitation = invitation.with_authority(Some(pin))?;
        }
        Ok(invitation)
    }

    /// New pinned links need an authenticated transport. Keep the original
    /// format for a host still sharing only plain LAN/Tailscale addresses;
    /// upgrading that flow is separate from inventing trust in a LAN answer.
    pub fn for_sharing(mut self) -> Self {
        if self.iroh.is_none() && !self.routes().iter().any(|url| crate::community::account_destination(url).is_ok()) {
            self.authority=None;
        }
        self
    }

    pub fn with_authority(mut self, pin: Option<RoutePin>) -> Result<Self, String> {
        if pin
            .as_ref()
            .is_some_and(|p| p.server_id != self.server_id || !crate::host_route::valid_pin(p))
        {
            return Err("This invitation has an invalid host identity.".into());
        }
        self.authority = pin;
        Ok(self)
    }

    /// Adds device verification. The secret lives only in the link fragment.
    pub fn with_proof(mut self, proof: InviteProof) -> Self {
        self.proof = Some(proof);
        self
    }

    /// Lists further direct addresses. The primary address is never repeated.
    pub fn with_alternatives(mut self, addresses: &[String]) -> Result<Self, String> {
        let mut alt = Vec::new();
        for address in addresses {
            let address = invitation_routes::valid_direct_address(address)?;
            if address != self.server && !alt.contains(&address) {
                alt.push(address);
            }
        }
        if alt.len() > invitation_routes::MAX_ALTERNATIVES {
            return Err("This invitation lists too many server addresses.".into());
        }
        self.alt = alt;
        Ok(self)
    }

    pub fn with_iroh(mut self, route: IrohRoute) -> Self {
        self.iroh = Some(route);
        self
    }

    /// Every address this invitation offers, the primary one first.
    pub fn routes(&self) -> Vec<String> {
        std::iter::once(self.server.clone())
            .chain(self.alt.iter().cloned())
            .collect()
    }

    /// The same invitation with another of its own addresses as the primary one.
    pub fn using_route(&self, address: &str) -> Result<Self, String> {
        if !self.routes().iter().any(|route| route == address) {
            return Err("That address is not part of this invitation.".into());
        }
        let mut chosen = self.clone();
        chosen.alt = self
            .routes()
            .into_iter()
            .filter(|route| route != address)
            .collect();
        chosen.server = address.to_owned();
        Ok(chosen)
    }

    pub fn with_network_share(mut self, share: &str) -> Result<Self, String> {
        if !self.routes().iter().any(|route| private_address(route)) || !valid_share_url(share) {
            return Err("Paste the device share link copied from Tailscale. It must be for this private server.".into());
        }
        self.network_share = Some(share.trim().into());
        Ok(self)
    }

    pub fn link(&self) -> String {
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair(
                "v",
                if self.authority.is_some() {
                    "5"
                } else if !self.alt.is_empty() || self.iroh.is_some() {
                    "4"
                } else if self.proof.is_some() {
                    "3"
                } else if self.network_share.is_some() {
                    "2"
                } else {
                    "1"
                },
            )
            .append_pair("server", &self.server)
            .append_pair("server_id", &self.server_id)
            .finish();
        let mut query = query;
        if !self.alt.is_empty() {
            query.push_str(
                &url::form_urlencoded::Serializer::new(String::from("&"))
                    .append_pair("alt", &invitation_routes::join_alternatives(&self.alt))
                    .finish(),
            );
        }
        if let Some(iroh) = &self.iroh {
            let mut extra = url::form_urlencoded::Serializer::new(String::from("&"));
            extra.append_pair("iroh", &iroh.endpoint_id);
            if let Some(relay) = &iroh.relay {
                extra.append_pair("irelay", relay);
            }
            query.push_str(&extra.finish());
        }
        let mut fragment = url::form_urlencoded::Serializer::new(String::new());
        fragment.append_pair("invite", &self.code);
        if let Some(share) = &self.network_share {
            fragment.append_pair("access", share);
        }
        if let Some(proof) = &self.proof {
            fragment
                .append_pair("iid", &proof.invite_id)
                .append_pair("hd", &proof.host_device_id)
                .append_pair("hf", &proof.host_fingerprint)
                .append_pair("vk", &proof.secret_for_link());
        }
        if let Some(iroh) = &self.iroh {
            fragment.append_pair("it", &iroh.token);
        }
        if let Some(pin) = &self.authority {
            fragment.append_pair(
                "hp",
                &URL_SAFE_NO_PAD.encode(serde_json::to_vec(pin).expect("bounded invitation pin")),
            );
        }
        let fragment = fragment.finish();
        format!("relay://join?{query}#{fragment}")
    }

    /// Shareable HTTPS wrapper. Everything, including the server addresses,
    /// stays after # and is never part of the website's HTTP request.
    pub fn browser_link(&self) -> String {
        let native = self.link();
        format!(
            "{BROWSER_INVITATION_URL}#{}",
            native["relay://join?".len()..].replacen('#', "&", 1)
        )
    }
}

pub fn private_address(address: &str) -> bool {
    url::Url::parse(address).ok().is_some_and(|u| {
        u.scheme() == "http"
            && u.port_or_known_default() == Some(4580)
            && u.host_str()
                .is_some_and(crate::tailscale_connect::valid_address)
    })
}

pub fn valid_share_url(input: &str) -> bool {
    let input = input.trim();
    if input.len() > 1024 || input.chars().any(char::is_control) {
        return false;
    }
    url::Url::parse(input).ok().is_some_and(|u| {
        u.scheme() == "https"
            && matches!(
                u.host_str(),
                Some("login.tailscale.com" | "console.tailscale.com")
            )
            && u.username().is_empty()
            && u.password().is_none()
            && u.port().is_none()
            && u.fragment().is_none()
            && u.path()
                .strip_prefix("/admin/invite/")
                .is_some_and(|token| {
                    !token.is_empty()
                        && token
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
                })
            && u.query().is_none()
    })
}

/// Every `required` field exactly once, `optional` ones at most once, nothing else.
fn fields_with(
    input: &str,
    required: &[&str],
    optional: &[&str],
) -> Result<HashMap<String, String>, String> {
    let mut fields = HashMap::new();
    for (key, value) in url::form_urlencoded::parse(input.as_bytes()) {
        if !(required.contains(&key.as_ref()) || optional.contains(&key.as_ref()))
            || fields
                .insert(key.into_owned(), value.into_owned())
                .is_some()
        {
            return Err("This invitation contains unexpected or repeated fields.".into());
        }
    }
    if !required.iter().all(|key| fields.contains_key(*key)) {
        return Err("This invitation is incomplete.".into());
    }
    Ok(fields)
}

pub fn local_network_address(server: &str) -> bool {
    let Ok(url) = url::Url::parse(server) else {
        return false;
    };
    let host = url.host_str().unwrap_or_default().trim_matches(['[', ']']);
    host.eq_ignore_ascii_case("localhost")
        || host.ends_with(".local")
        || host.parse::<std::net::IpAddr>().is_ok_and(|ip| match ip {
            std::net::IpAddr::V4(ip) => ip.is_loopback() || ip.is_private() || ip.is_link_local(),
            std::net::IpAddr::V6(ip) => {
                ip.is_loopback() || ip.is_unique_local() || ip.is_unicast_link_local()
            }
        })
}

pub fn reachability_hint(server: &str) -> &'static str {
    let Ok(url) = url::Url::parse(server) else {
        return "Paste an invitation or enter a complete server address.";
    };
    let host = url.host_str().unwrap_or_default().trim_matches(['[', ']']);
    let ip = host.parse::<std::net::IpAddr>().ok();
    if host.eq_ignore_ascii_case("localhost") || ip.is_some_and(|ip| ip.is_loopback()) {
        "This address works only on this PC. For friends, use the host's LAN or internet address."
    } else if crate::tailscale_connect::valid_address(host) {
        "This address needs private network access, such as Tailscale. Both PCs must be connected and the host must grant access."
    } else if ip.is_some_and(|ip| match ip {
        std::net::IpAddr::V4(ip) => ip.is_private() || ip.is_link_local(),
        std::net::IpAddr::V6(ip) => ip.is_unique_local() || ip.is_unicast_link_local(),
    }) || host.ends_with(".local")
    {
        "This address is for your local network. Friends must be on the same network or VPN."
    } else if url.scheme() == "http" {
        "For internet access, use HTTPS to protect sign-in. The host must make this address reachable."
    } else {
        "The host must make this address reachable. A connection check cannot prove who owns the server."
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const ID: &str = "4be44aef-8c98-4db6-a9d5-81c2eb311330";

    #[test]
    fn browser_links_preserve_every_native_version_route_and_verification() {
        let share = "https://login.tailscale.com/admin/invite/synthetic-share";
        let plain = Invitation::new("https://chat.example.org/base", ID, "synthetic-code").unwrap();
        let private = Invitation::new("http://100.70.1.2:4580", ID, "synthetic-code").unwrap();
        for invitation in [
            plain.clone(),
            private.clone().with_network_share(share).unwrap(),
            plain.with_proof(proof()),
            multi_route()
                .with_network_share(share)
                .unwrap()
                .with_proof(proof()),
        ] {
            let web = invitation.browser_link();
            let url = url::Url::parse(&web).unwrap();
            assert_eq!(web.split('#').next().unwrap(), BROWSER_INVITATION_URL);
            assert!(url.query().is_none() && url.fragment().is_some());
            let parsed = Invitation::parse(&web).unwrap();
            assert!(parsed == invitation);
            assert_eq!(parsed.link(), invitation.link());
            // The landing page reconstructs exactly this native link; the
            // installed protocol broker continues to receive the old format.
            assert!(invitation.link().len() <= 4096);
        }
    }

    #[test]
    fn browser_wrapper_cannot_bypass_origin_version_proof_or_size_checks() {
        let plain = Invitation::new("https://chat.example.org", ID, "synthetic-code")
            .unwrap()
            .browser_link();
        let verified = multi_route().with_proof(proof()).browser_link();
        for invalid in [
            plain.replace("https://", "http://"),
            plain.replace("thebengine.github.io", "thebengine.github.io.evil.example"),
            plain.replace("https://", "https://user@"),
            plain.replace("/Relay/", "/Other/"),
            plain.replace("/Relay/#", "/Relay/?tracking=1#"),
            format!("{plain}&v=1"),
            format!("{plain}&unknown=1"),
            plain.replace("v=1", "v=3"),
            plain.replace("v=1", "v=99"),
            verified.replace("vk=", "other="),
            format!("{plain}{}", "x".repeat(MAX_INPUT_BYTES)),
        ] {
            assert!(Invitation::parse(&invalid).is_err());
        }
        // A wrapper's extra allowance only covers its prefix; native limits
        // still apply after reconstruction, including oversized server paths.
        let template = format!("relay://join?v=1&server=&server_id={ID}#invite=code");
        let native = template.replace(
            "server=",
            &format!("server={}", "a".repeat(4097 - template.len())),
        );
        let web = format!(
            "{BROWSER_INVITATION_URL}#{}",
            native["relay://join?".len()..].replace('#', "&")
        );
        assert!(web.len() <= MAX_INPUT_BYTES && native.len() > 4096);
        assert!(Invitation::parse(&web).is_err());
    }

    #[test]
    fn a_lan_link_is_explicitly_local_until_an_internet_route_is_offered() {
        let local = Invitation::new("http://192.168.111.6:4580", ID, "code").unwrap();
        assert!(local.same_network_only());
        let mixed = local
            .clone()
            .with_alternatives(&["https://relay.example.com".into()])
            .unwrap();
        assert!(!mixed.same_network_only());
        let mut combined = local;
        combined.iroh = Some(IrohRoute {
            endpoint_id: "ab".repeat(32),
            relay: None,
            token: "cd".repeat(32),
        });
        assert!(!combined.same_network_only());
        for address in [
            "http://127.0.0.1:4580",
            "http://[fd00::1]:4580",
            "http://relay.local:4580",
        ] {
            assert!(Invitation::new(address, ID, "code")
                .unwrap()
                .same_network_only());
        }
        assert!(!Invitation::new("http://100.70.1.2:4580", ID, "code")
            .unwrap()
            .same_network_only());
    }

    #[test]
    fn combined_invitation_keeps_both_capabilities_in_fragment() {
        let share = "https://login.tailscale.com/admin/invite/owned_test-link";
        let invite = Invitation::new("http://100.70.1.2:4580", ID, "relay-secret")
            .unwrap()
            .with_network_share(share)
            .unwrap();
        let link = invite.link();
        let before_fragment = link.split('#').next().unwrap();
        assert!(!before_fragment.contains("relay-secret"));
        assert!(!before_fragment.contains("owned_test-link"));
        assert!(Invitation::parse(&link).unwrap() == invite);
        assert!(!invite.server.contains("invite"));
        assert!(Invitation::parse(&link.replace("v=2", "v=1")).is_err());
        assert!(Invitation::parse(&format!("{link}&access=duplicate")).is_err());
    }

    #[test]
    fn provider_access_links_reject_lookalikes_settings_and_nonprivate_servers() {
        for input in [
            "http://login.tailscale.com/admin/invite/secret",
            "https://login.tailscale.com.evil.example/admin/invite/secret",
            "https://login.tailscale.com@evil.example/admin/invite/secret",
            "https://user:pass@login.tailscale.com/admin/invite/secret",
            "https://login.tailscale.com/admin/machines",
            "https://login.tailscale.com/admin/invite/",
            "https://login.tailscale.com/admin/invite/secret/other",
            "https://login.tailscale.com/admin/invite/secret?redirect=https://evil.example",
            "https://login.tailscale.com/admin/invite/secret#fragment",
        ] {
            assert!(!valid_share_url(input), "{input}");
        }
        assert!(valid_share_url(
            "https://console.tailscale.com/admin/invite/test-token_2"
        ));
        for server in [
            "https://example.com",
            "http://127.0.0.1:4580",
            "http://100.70.1.2:80",
        ] {
            assert!(Invitation::new(server, ID, "code")
                .unwrap()
                .with_network_share("https://login.tailscale.com/admin/invite/test")
                .is_err());
        }
    }

    #[test]
    fn round_trip_preserves_base_path_and_keeps_code_out_of_http_url() {
        let invitation =
            Invitation::new("HTTPS://EXAMPLE.COM:443/relay///", ID, "test-code_123").unwrap();
        let link = invitation.link();
        let parsed = Invitation::parse(&link).unwrap();
        assert!(parsed == invitation);
        assert_eq!(parsed.server, "https://example.com/relay");
        assert!(!link.split('#').next().unwrap().contains("test-code"));
        assert!(url::Url::parse(&parsed.server)
            .unwrap()
            .fragment()
            .is_none());
    }

    #[test]
    fn malformed_invites_never_reach_sign_in() {
        let link = Invitation::new("http://127.0.0.1:4580", ID, "code")
            .unwrap()
            .link();
        for invalid in [
            link.replace("v=1", "v=2"),
            link.replace("v=1", "v=1&v=1"),
            link.replace("v=1", "v=1&extra=yes"),
            link.replace("#invite=code", "#invite=code&invite=other"),
            link.replace("#invite=code", "&invite=code"),
            link.replace("#invite=code", "#invite="),
            link.replace("relay://join?", "relay://user@join?"),
            link.replace("relay://join?", "relay://join/x?"),
            link.replace(ID, "not-a-community"),
            format!("{link}{}", "x".repeat(4096)),
        ] {
            assert!(Invitation::parse(&invalid).is_err());
        }
        for invalid in [
            "https://user:password@example.com",
            "https://example.com?invite=secret",
            "file:///server",
            "https://example.com#secret",
        ] {
            assert!(Invitation::new(invalid, ID, "code").is_err());
        }
        for code in ["a b", "a\nb", "💬", "a/b", "a&b"] {
            assert!(Invitation::new("https://example.com", ID, code).is_err());
        }
    }

    #[test]
    fn reachability_describes_local_and_internet_addresses() {
        for url in [
            "http://localhost:4580",
            "http://127.5.1.2",
            "http://[::1]:4580",
        ] {
            assert!(reachability_hint(url).contains("only on this PC"));
        }
        for url in [
            "http://192.168.1.10",
            "https://10.0.0.1",
            "http://[fd00::1]",
            "http://relay.local",
        ] {
            assert!(reachability_hint(url).contains("same network or VPN"));
        }
        assert!(reachability_hint("http://relay.example.com").contains("use HTTPS"));
        assert!(reachability_hint("https://relay.example.com").contains("cannot prove"));
    }

    fn proof() -> InviteProof {
        InviteProof::new(
            &"ab".repeat(32),
            "host-device",
            &"cd".repeat(32),
            std::array::from_fn(|index| index as u8),
        )
        .unwrap()
    }

    #[test]
    fn verification_fields_round_trip_stay_in_the_fragment_and_older_links_keep_working() {
        let link = Invitation::new("http://127.0.0.1:4580", ID, "code")
            .unwrap()
            .with_proof(proof())
            .link();
        assert!(link.starts_with("relay://join?v=3&"), "{link}");
        let (query, fragment) = link.split_once('#').unwrap();
        for field in ["invite=code", "iid=", "hd=host-device", "hf=", "vk="] {
            assert!(fragment.contains(field), "{field} belongs in the fragment");
            assert!(!query.contains(field), "{field} must never be in the query");
        }
        let parsed = Invitation::parse(&link).unwrap();
        assert_eq!(parsed.proof, Some(proof()));
        assert_eq!(parsed.code, "code");
        assert_eq!(parsed.link(), link);
        // A private server can carry both the provider access link and the proof.
        let both = Invitation::new("http://100.76.3.4:4580", ID, "code")
            .unwrap()
            .with_network_share("https://login.tailscale.com/admin/invite/example_share")
            .unwrap()
            .with_proof(proof())
            .link();
        let parsed = Invitation::parse(&both).unwrap();
        assert!(parsed.network_share.is_some() && parsed.proof.is_some());
        assert_eq!(parsed.link(), both);
        // Links without verification are unchanged and carry none.
        let plain = Invitation::new("http://127.0.0.1:4580", ID, "code")
            .unwrap()
            .link();
        assert!(plain.starts_with("relay://join?v=1&") && !plain.contains("vk="));
        assert!(Invitation::parse(&plain).unwrap().proof.is_none());
    }

    #[test]
    fn verification_links_with_missing_repeated_or_invalid_fields_never_reach_sign_in() {
        let link = Invitation::new("http://127.0.0.1:4580", ID, "code")
            .unwrap()
            .with_proof(proof())
            .link();
        let fragment = link.split_once('#').unwrap().1.to_string();
        let without = |field: &str| {
            let kept: Vec<_> = fragment
                .split('&')
                .filter(|part| !part.starts_with(field))
                .collect();
            link.replace(&fragment, &kept.join("&"))
        };
        for field in ["iid=", "hd=", "hf=", "vk="] {
            assert!(
                Invitation::parse(&without(field)).is_err(),
                "missing {field}"
            );
        }
        let secret = proof().secret_for_link();
        for invalid in [
            link.replace(&fragment, &format!("{fragment}&vk=AAAA")),
            link.replace(&fragment, &format!("{fragment}&extra=1")),
            link.replace(&"ab".repeat(32), "short"),
            link.replace(&"cd".repeat(32), &"CD".repeat(32)),
            link.replace("hd=host-device", "hd=bad%20device"),
            link.replace(secret.as_str(), "AAAA"),
            link.replace("v=3", "v=1"),
            link.replace("v=3", "v=2"),
            link.replace("v=3", "v=5"),
        ] {
            assert!(Invitation::parse(&invalid).is_err(), "{invalid}");
        }
    }
    const IROH_ID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const IROH_TOKEN: &str = "abcdefghijklmnopqrstuvwxyzABCDEF0123456789_-";

    fn multi_route() -> Invitation {
        Invitation::new("http://192.168.1.50:4580", ID, "relay-secret")
            .unwrap()
            .with_alternatives(&[
                "https://chat.example.org".to_owned(),
                "http://100.70.1.2:4580".to_owned(),
            ])
            .unwrap()
            .with_iroh(
                IrohRoute::new(IROH_ID, Some("https://relay.example.org"), IROH_TOKEN).unwrap(),
            )
    }

    #[test]
    fn several_routes_round_trip_and_only_the_token_is_secret() {
        let invitation = multi_route();
        let link = invitation.link();
        assert!(link.starts_with("relay://join?v=4&"), "{link}");
        let (before, fragment) = link.split_once('#').unwrap();
        assert!(
            !before.contains("relay-secret"),
            "the invite code must stay in the fragment"
        );
        assert!(
            !before.contains(IROH_TOKEN),
            "the iroh token must stay in the fragment"
        );
        assert!(fragment.contains(&format!("it={IROH_TOKEN}")));
        assert!(before.contains(&format!("iroh={IROH_ID}")));
        let parsed = Invitation::parse(&link).unwrap();
        assert!(parsed == invitation);
        assert_eq!(
            parsed.routes(),
            [
                "http://192.168.1.50:4580",
                "https://chat.example.org",
                "http://100.70.1.2:4580"
            ]
        );
        assert_eq!(parsed.iroh.as_ref().unwrap().endpoint_id, IROH_ID);
        assert_eq!(
            parsed.link(),
            link,
            "a parsed link serialises to the same link"
        );
    }

    #[test]
    fn routes_can_be_combined_with_a_tailscale_share_and_device_verification() {
        let share = "https://login.tailscale.com/admin/invite/owned_test-link";
        let invitation = multi_route()
            .with_network_share(share)
            .unwrap()
            .with_proof(proof());
        let parsed = Invitation::parse(&invitation.link()).unwrap();
        assert!(parsed == invitation);
        assert_eq!(parsed.network_share.as_deref(), Some(share));
        assert!(parsed.proof.is_some() && parsed.iroh.is_some());
        // A share link needs a Tailscale address somewhere in the invitation.
        let none = Invitation::new("http://192.168.1.50:4580", ID, "c")
            .unwrap()
            .with_alternatives(&["https://chat.example.org".to_owned()])
            .unwrap();
        assert!(none.with_network_share(share).is_err());
    }

    #[test]
    fn an_invitation_without_extra_routes_keeps_its_old_version() {
        let plain = Invitation::new("http://192.168.1.50:4580", ID, "relay-secret").unwrap();
        assert!(plain.link().starts_with("relay://join?v=1&"));
        assert!(plain
            .clone()
            .with_alternatives(&[])
            .unwrap()
            .link()
            .starts_with("relay://join?v=1&"));
        // The primary address listed again is not a second route.
        let again = plain
            .clone()
            .with_alternatives(&["http://192.168.1.50:4580".to_owned()])
            .unwrap();
        assert!(again.alt.is_empty() && again.link().starts_with("relay://join?v=1&"));
    }

    #[test]
    fn extra_route_fields_are_refused_in_old_versions_and_when_incomplete_or_hostile() {
        let link = multi_route().link();
        let (before, fragment) = link.split_once('#').unwrap();
        let alt = before.split('&').find(|p| p.starts_with("alt=")).unwrap();
        let iroh = before.split('&').find(|p| p.starts_with("iroh=")).unwrap();
        let irelay = before
            .split('&')
            .find(|p| p.starts_with("irelay="))
            .unwrap();
        let token = format!("it={IROH_TOKEN}");
        for invalid in [
            // Old versions never carry extra routes.
            link.replace("v=4", "v=1"),
            link.replace("v=4", "v=3"),
            // The key without its token, and the token without its key.
            link.replace(&format!("&{token}"), ""),
            link.replace(&format!("&{iroh}"), "")
                .replace(&format!("&{irelay}"), ""),
            // A relay address without a route to use it for.
            link.replace(&format!("&{iroh}"), "")
                .replace(&format!("&{token}"), ""),
            // Repeated or unexpected fields.
            format!("{before}&alt=https%3A%2F%2Fother.example.org#{fragment}"),
            format!("{before}&extra=1#{fragment}"),
            format!("{before}#{fragment}&it={IROH_TOKEN}"),
            // Hostile addresses: wrong port, wrong scheme, credentials.
            link.replace(alt, "alt=http%3A%2F%2F192.168.1.1%3A22"),
            link.replace(alt, "alt=ftp%3A%2F%2Fexample.org"),
            link.replace(alt, "alt=http%3A%2F%2Fuser%3Apw%40example.org%3A4580"),
            link.replace(irelay, "irelay=http%3A%2F%2Frelay.example.org"),
            // A bad key or token.
            link.replace(IROH_ID, &IROH_ID[..60]),
            link.replace(IROH_TOKEN, "short"),
        ] {
            assert!(Invitation::parse(&invalid).is_err(), "{invalid}");
        }
        // Only some device-verification fields in a version 4 link are refused too.
        let with_proof = multi_route().with_proof(proof()).link();
        let (b, f) = with_proof.split_once('#').unwrap();
        let kept: Vec<_> = f
            .split('&')
            .filter(|part| !part.starts_with("vk="))
            .collect();
        assert!(Invitation::parse(&format!("{b}#{}", kept.join("&"))).is_err());
        // Five extra addresses are too many.
        let many: Vec<String> = (1..=5).map(|i| format!("http://10.0.0.{i}:4580")).collect();
        assert!(Invitation::new("http://192.168.1.50:4580", ID, "c")
            .unwrap()
            .with_alternatives(&many)
            .is_err());
    }

    #[test]
    fn choosing_a_route_keeps_the_others_and_never_invents_one() {
        let invitation = multi_route();
        let chosen = invitation.using_route("https://chat.example.org").unwrap();
        assert_eq!(chosen.server, "https://chat.example.org");
        assert_eq!(
            chosen.routes(),
            [
                "https://chat.example.org",
                "http://192.168.1.50:4580",
                "http://100.70.1.2:4580"
            ]
        );
        assert!(
            chosen.iroh == invitation.iroh
                && chosen.code == invitation.code
                && chosen.server_id == invitation.server_id
        );
        assert!(invitation
            .using_route("https://elsewhere.example.org")
            .is_err());
        let back = chosen.using_route("http://192.168.1.50:4580").unwrap();
        assert_eq!(back.server, invitation.server);
    }
}

#[cfg(test)]
mod authority_tests {
    use super::*;
    #[test]
    fn invitation_host_pin_roundtrips_and_cannot_be_dropped_or_rebound() {
        let (_, _, pin, _) = crate::host_route::tests::fixture();
        let old =
            Invitation::new("https://source.example", &pin.server_id, "owned-invite").unwrap();
        assert!(old.link().contains("v=1&"));
        let invite = old.with_authority(Some(pin.clone())).unwrap();
        for value in [invite.link(), invite.browser_link()] {
            let parsed = Invitation::parse(&value).unwrap();
            assert!(parsed == invite);
            assert!(!value.split('#').next().unwrap().contains("hp="));
        }
        assert!(invite.link().contains("v=5&"));
        // The form Windows hands a URL protocol handler: a `/` before the query.
        let windows = invite.link().replacen("relay://join?", "relay://join/?", 1);
        assert!(Invitation::parse(&windows).unwrap() == invite);
        assert!(Invitation::parse(&invite.link().replacen("relay://join?", "relay://join/x?", 1)).is_err());
        let changed = invite
            .clone()
            .with_alternatives(&["https://other.example".into()])
            .unwrap()
            .using_route("https://other.example")
            .unwrap();
        assert_eq!(changed.authority, Some(pin.clone()));
        let mut wrong = pin.clone();
        wrong.server_id = uuid::Uuid::new_v4().to_string();
        assert!(invite.clone().with_authority(Some(wrong)).is_err());
        for bad in [
            invite.link().replace("v=5&", "v=4&"),
            format!("{}&hp=duplicate", invite.link()),
            invite.link().split("&hp=").next().unwrap().to_owned(),
            invite
                .link()
                .replace(&pin.server_id, &uuid::Uuid::new_v4().to_string()),
        ] {
            assert!(Invitation::parse(&bad).is_err());
        }
        let mut invalid = pin;
        invalid.host.host_id = uuid::Uuid::new_v4().to_string();
        let bad = invite.link().split("&hp=").next().unwrap().to_owned()
            + "&hp="
            + &URL_SAFE_NO_PAD.encode(serde_json::to_vec(&invalid).unwrap());
        assert!(Invitation::parse(&bad).is_err());
    }
}
