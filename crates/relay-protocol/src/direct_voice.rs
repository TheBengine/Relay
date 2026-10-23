//! Optional version-4 direct voice control, carried inside authenticated relay UDP.
//! Only two-member rooms qualify. Larger rooms and older servers use relay routing.
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

pub const REQUEST: u8 = 6;
pub const ROSTER: u8 = 7;
pub const MARKER: &[u8; 4] = b"RDV1";
pub const LEASE_MS: u64 = 500;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Peer {
    pub session: u64,
    pub user: String,
    pub address: SocketAddr,
    pub key: [u8; 32],
    pub relay_rtt_us: u64,
    pub send_allowed: bool,
    pub receive_allowed: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Roster {
    pub request: u64,
    pub peer: Option<Peer>,
}

pub fn request(id: u64, enabled: bool, relay_rtt_us: u64) -> Vec<u8> {
    let mut result = vec![REQUEST, enabled as u8];
    result.extend(id.to_le_bytes());
    result.extend(relay_rtt_us.to_le_bytes());
    result
}

pub fn parse_request(bytes: &[u8]) -> Option<(u64, bool, u64)> {
    if bytes.len() != 18 || bytes[0] != REQUEST || bytes[1] > 1 {
        return None;
    }
    let id = u64::from_le_bytes(bytes[2..10].try_into().ok()?);
    let rtt = u64::from_le_bytes(bytes[10..18].try_into().ok()?);
    Some((id, bytes[1] == 1, rtt.min(5_000_000)))
}
