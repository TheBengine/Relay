//! Types of the community's local voice gateway broker (`/api/hosting/voice-gateways`).
//!
//! A member who holds an accepted `VoiceGateway` role and is in a voice channel
//! registers a private IPv4 address and port for that channel. Other members in
//! the same channel lease their own live session from it. A lease is a short,
//! server-issued grant (fresh random id and hop key) that only that client and
//! that gateway ever receive; it never contains a voice-ticket or E2EE key.
//! These are plain data types; the rules are enforced by the server.
use serde::{Deserialize, Serialize};
use std::net::{Ipv4Addr, SocketAddrV4};

/// A registration lasts this long unless renewed by registering again.
pub const REGISTRATION_TTL_SECS: u64 = 120;
/// A lease is valid this long from the request that created or renewed it.
/// The data plane refuses anything above two seconds.
pub const LEASE_TTL_MS: u32 = 2000;
/// Grants one gateway serves at once (the data plane's route limit).
pub const MAX_GRANTS_PER_GATEWAY: usize = 32;
pub const MIN_PORT: u16 = 1024;

/// `POST /api/hosting/voice-gateways/registration`
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Register {
    /// Random 16 bytes as 32 lowercase hex digits, new for every gateway worker
    /// start (never reused, never all zero). A different instance, address or voice
    /// session replaces the registration and cancels its leases.
    pub instance: String,
    /// The caller's own live voice session id. It must be the session the server
    /// holds for the caller now, or the request is refused with 409 and changes
    /// nothing (a late request of a finished call).
    pub session_id: u64,
    /// Numeric private IPv4 address, such as `192.168.1.20`. Never a name.
    pub ip: String,
    pub port: u16,
}

/// Answer to a registration.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Registered {
    /// The registered instance, echoed.
    pub instance: String,
    pub session_id: u64,
    /// The voice channel the registration is for: the caller's own live session's.
    pub channel_id: String,
    /// Register again before this many seconds pass to keep it.
    pub expires_in_secs: u64,
    pub max_grants: u32,
}

/// One registered gateway, as shown to members of the same channel.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct GatewayOffer {
    /// The gateway's account id.
    pub gateway_id: String,
    /// The instance that is registered now. Lease this one, or nothing.
    pub instance: String,
    pub ip: String,
    pub port: u16,
}

/// `GET /api/hosting/voice-gateways`
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct GatewayList {
    pub gateways: Vec<GatewayOffer>,
}

/// `POST /api/hosting/voice-gateways/leases`
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeaseRequest {
    pub gateway_id: String,
    /// The instance from the offer. If it is no longer the registered one, 404.
    pub instance: String,
    /// The caller's own live voice session id; a mismatch is 409 and changes nothing.
    pub session_id: u64,
}

/// `DELETE /api/hosting/voice-gateways/registration?instance=<hex32>&session_id=<u64>`:
/// drops only the caller's registration that matches BOTH; otherwise nothing.
#[derive(Clone, Debug, Deserialize)]
pub struct DropRegistration {
    pub instance: String,
    pub session_id: u64,
}

/// `DELETE /api/hosting/voice-gateways/leases?session_id=<u64>&lease_id=<hex32>`:
/// drops only the caller's own lease that matches BOTH; otherwise nothing.
#[derive(Clone, Debug, Deserialize)]
pub struct DropLease {
    pub session_id: u64,
    pub lease_id: String,
}

/// `GET /api/hosting/voice-gateways/grants?instance=<32 hex>`. Required: a poll
/// for an instance that is not the registered one gets 409 and no grants.
#[derive(Clone, Debug, Deserialize)]
pub struct GrantsQuery {
    pub instance: String,
}

/// Answer to a client: its own lease. Holds a secret key, so it has no `Debug`.
#[derive(Clone, Serialize, Deserialize)]
pub struct LeaseGrant {
    /// 16 random bytes as 32 lowercase hex digits.
    pub lease_id: String,
    pub gateway_id: String,
    /// The gateway instance this lease is bound to.
    pub instance: String,
    /// The client's own voice session id, which the data plane binds the lease to.
    pub session_id: u64,
    /// The 32-byte hop key, URL-safe base64 without padding.
    pub key: String,
    /// Remaining validity. A renewal while live keeps the id and key.
    pub ttl_ms: u32,
    pub ip: String,
    pub port: u16,
}

/// One grant as the gateway receives it. Holds a secret key, so it has no `Debug`.
#[derive(Clone, Serialize, Deserialize)]
pub struct GatewayGrant {
    pub id: String,
    pub session: u64,
    pub key: String,
    /// Remaining validity. Polling never extends it.
    pub ttl_ms: u32,
}

/// `GET /api/hosting/voice-gateways/grants`
#[derive(Clone, Serialize, Deserialize)]
pub struct GatewayGrants {
    pub grants: Vec<GatewayGrant>,
}

/// A numeric private IPv4 address and a port of at least `MIN_PORT`. Hostnames,
/// public, loopback, link-local and other addresses are refused: the server only
/// stores the address and never connects to it.
pub fn parse_private_addr(ip: &str, port: u16) -> Result<SocketAddrV4, &'static str> {
    let ip: Ipv4Addr = ip
        .parse()
        .map_err(|_| "Use a numeric private IPv4 address such as 192.168.1.20")?;
    if !ip.is_private() {
        return Err("Use a private local network address (10.x, 172.16-31.x or 192.168.x)");
    }
    if port < MIN_PORT {
        return Err("Use a port of 1024 or higher");
    }
    Ok(SocketAddrV4::new(ip, port))
}

/// A gateway instance id: 32 lowercase hex digits, not all zero.
pub fn valid_instance(text: &str) -> bool {
    decode_id(text).is_some_and(|id| id != [0u8; 16])
}

pub fn encode_id(id: &[u8; 16]) -> String {
    id.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn decode_id(text: &str) -> Option<[u8; 16]> {
    if text.len() != 32 || !text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_numeric_private_ipv4_with_a_real_port_is_accepted() {
        assert_eq!(
            parse_private_addr("192.168.1.20", 47000)
                .unwrap()
                .to_string(),
            "192.168.1.20:47000"
        );
        for ok in ["10.0.0.5", "172.16.0.1", "172.31.255.254"] {
            assert!(parse_private_addr(ok, 1024).is_ok(), "{ok}");
        }
        for bad in [
            "gateway.local",
            "localhost",
            "192.168.1.20.example.com",
            "127.0.0.1",
            "8.8.8.8",
            "100.64.0.1",
            "169.254.1.1",
            "172.32.0.1",
            "0.0.0.0",
            "255.255.255.255",
            "192.168.1",
            "192.168.1.20:47000",
            "http://192.168.1.20",
            "::1",
            "fe80::1",
            "",
            " 192.168.1.20",
        ] {
            assert!(parse_private_addr(bad, 47000).is_err(), "{bad:?}");
        }
        assert!(parse_private_addr("192.168.1.20", 1023).is_err());
        assert!(parse_private_addr("192.168.1.20", 0).is_err());
    }

    #[test]
    fn ids_round_trip_as_lowercase_hex_and_nothing_else() {
        let id = [0xab; 16];
        assert_eq!(encode_id(&id), "ab".repeat(16));
        assert_eq!(decode_id(&encode_id(&id)), Some(id));
        for bad in [
            "",
            "ab",
            &"AB".repeat(16),
            &"zz".repeat(16),
            &"ab".repeat(17),
        ] {
            assert_eq!(decode_id(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn an_instance_id_is_32_lowercase_hex_digits_and_not_zero() {
        assert!(valid_instance(&"ab".repeat(16)));
        for bad in [
            "",
            &"00".repeat(16),
            &"AB".repeat(16),
            &"ab".repeat(15),
            "xyz",
        ] {
            assert!(!valid_instance(bad), "{bad:?}");
        }
    }

    #[test]
    fn limits_match_the_data_plane() {
        // The data plane refuses a lease above two seconds and more than 32 routes.
        assert!(LEASE_TTL_MS > 0 && LEASE_TTL_MS <= 2000);
        assert_eq!(MAX_GRANTS_PER_GATEWAY, 32);
        assert!(REGISTRATION_TTL_SECS >= 60 && REGISTRATION_TTL_SECS <= 600);
    }
}
