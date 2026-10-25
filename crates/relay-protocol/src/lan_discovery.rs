//! Local-network discovery beacons: one question, one short answer.
//!
//! A client asks "who runs a Relay community here?" with a random nonce; a
//! community server whose host switched discovery on answers the sender
//! directly. The answer carries only what the community already shows anyone
//! who reaches its address: the HTTP port, the community name and its server id.
//! It never carries an address (the client uses the packet's source), a
//! credential, an invitation, a user or a count. Everything here is bounded:
//! packets are at most `MAX_PACKET` bytes, parsing is exact, and a malformed
//! or unrelated packet is dropped without a reply.
use std::net::Ipv4Addr;

/// UDP port the responder listens on and clients ask. Not used for anything else.
pub const PORT: u16 = 47_932;
pub const NONCE_LEN: usize = 16;
pub const REQUEST_LEN: usize = 8 + NONCE_LEN;
pub const MAX_PACKET: usize = 256;
pub const MAX_NAME_BYTES: usize = 48;
pub const SERVER_ID_LEN: usize = 36;
/// Advertised ports below this are refused, so a beacon cannot point a client
/// at a well-known service.
pub const MIN_PORT: u16 = 1024;

const REQUEST_MAGIC: &[u8; 8] = b"RLYLAN1?";
const REPLY_MAGIC: &[u8; 8] = b"RLYLAN1!";

pub type Nonce = [u8; NONCE_LEN];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Advert {
    pub port: u16,
    pub name: String,
    pub server_id: String,
}

/// Addresses discovery will talk to or listen to: private (RFC 1918) or
/// link-local IPv4. Never loopback, public, multicast or unspecified.
pub fn is_lan_ipv4(ip: Ipv4Addr) -> bool {
    ip.is_private() || ip.is_link_local()
}

fn bad_char(c: char) -> bool {
    c.is_control() || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// A name a client may show: 1 to `MAX_NAME_BYTES` bytes, no control or
/// direction-override characters, not only whitespace.
pub fn valid_name(name: &str) -> bool {
    !name.trim().is_empty() && name.len() <= MAX_NAME_BYTES && !name.chars().any(bad_char)
}

/// The shape of a UUID (the form of every server id), without a parser.
pub fn valid_server_id(id: &str) -> bool {
    id.len() == SERVER_ID_LEN
        && id.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

/// Make any community name fit a beacon: bad characters become spaces, the
/// text is cut on a character boundary, and an empty result becomes a generic name.
pub fn sanitize_name(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| if bad_char(c) { ' ' } else { c })
        .collect();
    let mut out = String::new();
    for c in cleaned.trim().chars() {
        if out.len() + c.len_utf8() > MAX_NAME_BYTES {
            break;
        }
        out.push(c);
    }
    let out = out.trim().to_string();
    if out.is_empty() {
        "Relay community".into()
    } else {
        out
    }
}

pub fn encode_request(nonce: &Nonce) -> [u8; REQUEST_LEN] {
    let mut out = [0u8; REQUEST_LEN];
    out[..8].copy_from_slice(REQUEST_MAGIC);
    out[8..].copy_from_slice(nonce);
    out
}

/// Exactly a request and nothing else.
pub fn parse_request(buf: &[u8]) -> Option<Nonce> {
    if buf.len() != REQUEST_LEN || &buf[..8] != REQUEST_MAGIC {
        return None;
    }
    buf[8..].try_into().ok()
}

fn valid_advert(advert: &Advert) -> bool {
    advert.port >= MIN_PORT && valid_name(&advert.name) && valid_server_id(&advert.server_id)
}

/// `magic | nonce | port (BE) | name length | name | server id`. `None` if the
/// advert could not be parsed back by a client.
pub fn encode_reply(nonce: &Nonce, advert: &Advert) -> Option<Vec<u8>> {
    if !valid_advert(advert) {
        return None;
    }
    let mut out = Vec::with_capacity(MAX_PACKET);
    out.extend_from_slice(REPLY_MAGIC);
    out.extend_from_slice(nonce);
    out.extend_from_slice(&advert.port.to_be_bytes());
    out.push(advert.name.len() as u8);
    out.extend_from_slice(advert.name.as_bytes());
    out.extend_from_slice(advert.server_id.as_bytes());
    (out.len() <= MAX_PACKET).then_some(out)
}

/// A reply to *this* request only: wrong nonce, wrong size, bad text, a port
/// below `MIN_PORT` or trailing bytes all give `None`.
pub fn parse_reply(buf: &[u8], nonce: &Nonce) -> Option<Advert> {
    const FIXED: usize = 8 + NONCE_LEN + 2 + 1;
    if buf.len() < FIXED + 1 + SERVER_ID_LEN || buf.len() > MAX_PACKET {
        return None;
    }
    if &buf[..8] != REPLY_MAGIC || &buf[8..8 + NONCE_LEN] != nonce {
        return None;
    }
    let port = u16::from_be_bytes([buf[8 + NONCE_LEN], buf[9 + NONCE_LEN]]);
    let name_len = buf[FIXED - 1] as usize;
    if buf.len() != FIXED + name_len + SERVER_ID_LEN {
        return None;
    }
    let name = std::str::from_utf8(&buf[FIXED..FIXED + name_len]).ok()?;
    let server_id = std::str::from_utf8(&buf[FIXED + name_len..]).ok()?;
    let advert = Advert {
        port,
        name: name.to_string(),
        server_id: server_id.to_string(),
    };
    valid_advert(&advert).then_some(advert)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "4be44aef-8c98-4db6-a9d5-81c2eb311330";

    fn advert() -> Advert {
        Advert {
            port: 4580,
            name: "Friends' house".into(),
            server_id: ID.into(),
        }
    }

    #[test]
    fn a_request_and_its_reply_round_trip_only_for_the_same_nonce() {
        let nonce = [7u8; NONCE_LEN];
        let request = encode_request(&nonce);
        assert_eq!(request.len(), REQUEST_LEN);
        assert_eq!(parse_request(&request), Some(nonce));
        let reply = encode_reply(&nonce, &advert()).unwrap();
        assert!(reply.len() <= MAX_PACKET);
        assert_eq!(parse_reply(&reply, &nonce), Some(advert()));
        // A reply to an older question (another nonce) is stale and ignored.
        assert_eq!(parse_reply(&reply, &[8u8; NONCE_LEN]), None);
    }

    #[test]
    fn requests_are_exact_and_replies_are_never_taken_for_requests() {
        let nonce = [1u8; NONCE_LEN];
        let request = encode_request(&nonce);
        assert_eq!(parse_request(&request[..REQUEST_LEN - 1]), None);
        let mut longer = request.to_vec();
        longer.push(0);
        assert_eq!(parse_request(&longer), None);
        assert_eq!(parse_request(&[]), None);
        let mut wrong = request;
        wrong[0] = b'X';
        assert_eq!(parse_request(&wrong), None);
        let reply = encode_reply(&nonce, &advert()).unwrap();
        assert_eq!(parse_request(&reply), None);
        assert_eq!(parse_reply(&request, &nonce), None);
    }

    #[test]
    fn replies_with_bad_content_are_dropped() {
        let nonce = [3u8; NONCE_LEN];
        let good = encode_reply(&nonce, &advert()).unwrap();
        // Trailing bytes, a short packet, an oversized packet.
        let mut trailing = good.clone();
        trailing.push(0);
        assert_eq!(parse_reply(&trailing, &nonce), None);
        assert_eq!(parse_reply(&good[..good.len() - 1], &nonce), None);
        assert_eq!(parse_reply(&vec![0u8; MAX_PACKET + 1], &nonce), None);
        // A lying name length cannot read past the packet.
        let mut lying = good.clone();
        lying[8 + NONCE_LEN + 2] = 250;
        assert_eq!(parse_reply(&lying, &nonce), None);
        // Bad text or ports cannot even be encoded, and a hand-made packet is refused.
        for mutate in [
            (|a: &mut Advert| a.port = 80) as fn(&mut Advert),
            |a| a.port = 0,
            |a| a.name = String::new(),
            |a| a.name = "   ".into(),
            |a| a.name = "x".repeat(MAX_NAME_BYTES + 1),
            |a| a.name = "line\nbreak".into(),
            |a| a.name = "bidi\u{202e}evil".into(),
            |a| a.server_id = "not-an-id".into(),
            |a| a.server_id = ID.replace('4', "g"),
            |a| a.server_id = format!("{ID}x"),
        ] {
            let mut bad = advert();
            mutate(&mut bad);
            assert_eq!(encode_reply(&nonce, &bad), None, "{bad:?}");
        }
        let mut manual = good.clone();
        manual[8 + NONCE_LEN] = 0;
        manual[9 + NONCE_LEN] = 80;
        assert_eq!(
            parse_reply(&manual, &nonce),
            None,
            "port 80 is refused on receipt too"
        );
        let mut invalid_utf8 = good;
        invalid_utf8[8 + NONCE_LEN + 3] = 0xff;
        assert_eq!(parse_reply(&invalid_utf8, &nonce), None);
    }

    #[test]
    fn a_full_length_name_still_fits_one_small_packet() {
        let mut full = advert();
        full.name = "é".repeat(MAX_NAME_BYTES / 2);
        assert_eq!(full.name.len(), MAX_NAME_BYTES);
        let reply = encode_reply(&[0; NONCE_LEN], &full).unwrap();
        assert!(reply.len() < 128, "{}", reply.len());
        assert_eq!(parse_reply(&reply, &[0; NONCE_LEN]), Some(full));
    }

    #[test]
    fn only_private_and_link_local_ipv4_count_as_local() {
        for ok in [
            "10.0.0.5",
            "172.16.3.4",
            "172.31.255.1",
            "192.168.1.20",
            "169.254.8.9",
        ] {
            assert!(is_lan_ipv4(ok.parse().unwrap()), "{ok}");
        }
        for no in [
            "127.0.0.1",
            "0.0.0.0",
            "8.8.8.8",
            "100.64.0.1",
            "172.32.0.1",
            "224.0.0.251",
            "255.255.255.255",
            "192.0.2.1",
        ] {
            assert!(!is_lan_ipv4(no.parse().unwrap()), "{no}");
        }
    }

    #[test]
    fn names_are_sanitized_for_the_beacon() {
        assert_eq!(sanitize_name("  Relay House  "), "Relay House");
        assert_eq!(sanitize_name("a\nb\u{202e}c"), "a b c");
        assert_eq!(sanitize_name(""), "Relay community");
        assert_eq!(sanitize_name("\n\t"), "Relay community");
        let long = sanitize_name(&"é".repeat(100));
        assert!(long.len() <= MAX_NAME_BYTES && long.chars().all(|c| c == 'é'));
        assert!(valid_name(&sanitize_name("x\u{0}y")));
    }
}
