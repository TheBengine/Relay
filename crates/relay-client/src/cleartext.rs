//! Which community addresses carry a password or a saved login in clear text.
//!
//! A plain `http://` address on the home network is read by anyone on the same
//! network (a guest on the Wi-Fi). Relay sends a password or a saved login there
//! only after the person said, once for that exact address, that they accept it.
//! Loopback is this PC talking to itself and stays as it is. HTTPS, iroh (QUIC/TLS
//! to a key) and Relay's private network or Tailscale (WireGuard) are encrypted.

/// How a request to a community address travels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exposure {
    /// This PC talks to its own server; nothing leaves the PC.
    Loopback,
    /// HTTPS, iroh or the private network (Tailscale): encrypted on the way.
    Encrypted,
    /// Plain HTTP to another machine: readable by others on the network.
    Cleartext,
}

pub fn exposure(address: &str) -> Exposure {
    let Ok(url) = url::Url::parse(address.trim()) else {
        return Exposure::Cleartext;
    };
    if matches!(url.scheme(), "https" | "wss") {
        return Exposure::Encrypted;
    }
    if crate::iroh_route::is_iroh(address.trim()) {
        return Exposure::Encrypted;
    }
    let Some(host) = url.host_str() else {
        return Exposure::Cleartext;
    };
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if bare.eq_ignore_ascii_case("localhost")
        || bare
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
    {
        return Exposure::Loopback;
    }
    if crate::tailscale_connect::valid_address(bare) {
        return Exposure::Encrypted;
    }
    Exposure::Cleartext
}

/// The canonical form an acknowledgement is remembered under, or `None` when the
/// address needs none.
pub fn needs_consent(address: &str) -> Option<String> {
    if exposure(address) != Exposure::Cleartext {
        return None;
    }
    Some(crate::community::normalize_url(address).unwrap_or_else(|_| address.trim().to_owned()))
}

/// The worker's refusal when no acknowledgement exists. Nothing was sent.
pub const CONSENT_NEEDED: &str = "This server's address is not encrypted, so Relay has not sent your password or saved login. Confirm that you want to sign in over this network first.";
/// The person chose not to sign in over an unencrypted connection.
pub const DECLINED: &str = "Not signed in. Ask the host for an https:// or iroh invitation to sign in with encryption.";

/// What the dialog says. One short title and two sentences: what can happen and
/// what the choice covers.
pub fn dialog_text(address: &str) -> (String, String) {
    let host = url::Url::parse(address)
        .ok()
        .and_then(|u| {
            u.host_str()
                .map(|h| match u.port() {
                    Some(port) => format!("{h}:{port}"),
                    None => h.to_owned(),
                })
        })
        .unwrap_or_else(|| address.to_owned());
    (
        "This connection is not encrypted".into(),
        format!(
            "{host} uses plain http://. Anyone on this network can read your password and use your login to add a device to your account. Relay remembers your choice for this address only and asks again if the address changes."
        ),
    )
}

/// Remembered acknowledgements, kept with the other preferences.
pub fn acknowledged(list: &[String], address: &str) -> bool {
    match needs_consent(address) {
        None => true,
        Some(canonical) => list.contains(&canonical),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_http_to_another_machine_needs_consent() {
        for local in [
            "http://127.0.0.1:4580",
            "http://127.4.5.6:4580",
            "http://localhost:4580",
            "http://[::1]:4580",
        ] {
            assert_eq!(exposure(local), Exposure::Loopback, "{local}");
            assert!(needs_consent(local).is_none());
        }
        let iroh = format!("http://{}.iroh:4580", "a".repeat(64));
        for encrypted in [
            "https://relay.example",
            "https://192.168.1.20:4580",
            iroh.as_str(),
            "http://100.101.102.103:4580",
        ] {
            assert_eq!(exposure(encrypted), Exposure::Encrypted, "{encrypted}");
        }
        for plain in [
            "http://192.168.1.20:4580",
            "http://10.0.0.5:4580",
            "http://relay.lan:4580",
            "http://203.0.113.9:4580",
            "http://[fd00::5]:4580",
        ] {
            assert_eq!(exposure(plain), Exposure::Cleartext, "{plain}");
        }
        assert_eq!(
            needs_consent(" HTTP://192.168.1.20:4580/ ").as_deref(),
            Some("http://192.168.1.20:4580")
        );
    }

    #[test]
    fn an_acknowledgement_covers_one_exact_address() {
        let list = vec!["http://192.168.1.20:4580".to_owned()];
        assert!(acknowledged(&list, "http://192.168.1.20:4580/"));
        assert!(!acknowledged(&list, "http://192.168.1.21:4580"));
        assert!(!acknowledged(&list, "http://192.168.1.20:4581"));
        assert!(acknowledged(&[], "http://127.0.0.1:4580"));
        assert!(acknowledged(&[], "https://relay.example"));
        let (title, body) = dialog_text("http://192.168.1.20:4580");
        assert_eq!(title, "This connection is not encrypted");
        assert!(body.starts_with("192.168.1.20:4580 uses plain http://."));
    }
}
