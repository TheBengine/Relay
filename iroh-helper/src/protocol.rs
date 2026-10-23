//! What the two ends say to each other before any chat or voice travels.
//!
//! The first stream a friend opens is a short hello with the admission token
//! from the invitation. Only a connection which sent the right token may open
//! further streams or send voice datagrams. Without this, anyone who learns the
//! host's public key could talk to the community's login and registration.
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const ALPN: &[u8] = b"relay-iroh/1";
const MAGIC: &[u8; 4] = b"RLYI";
const VERSION: u8 = 1;
pub const MIN_TOKEN: usize = 22;
pub const MAX_TOKEN: usize = 64;
pub const ACCEPTED: u8 = 1;
pub const DENIED: u8 = 0;

#[derive(Debug, PartialEq, Eq)]
pub enum HelloError {
    /// The token is not of an acceptable shape. Nothing was sent.
    BadToken,
    /// The other side did not follow the protocol.
    Malformed,
    Io,
}

pub fn valid_token(token: &str) -> bool {
    (MIN_TOKEN..=MAX_TOKEN).contains(&token.len())
        && token
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
}

pub fn encode_hello(token: &str) -> Result<Vec<u8>, HelloError> {
    if !valid_token(token) {
        return Err(HelloError::BadToken);
    }
    let mut hello = Vec::with_capacity(6 + token.len());
    hello.extend_from_slice(MAGIC);
    hello.push(VERSION);
    hello.push(token.len() as u8);
    hello.extend_from_slice(token.as_bytes());
    Ok(hello)
}

/// Reads one hello. Never reads more than the largest hello, whatever the peer sends.
pub async fn read_hello<R: AsyncRead + Unpin>(reader: &mut R) -> Result<String, HelloError> {
    let mut head = [0u8; 6];
    reader
        .read_exact(&mut head)
        .await
        .map_err(|_| HelloError::Io)?;
    if &head[..4] != MAGIC || head[4] != VERSION {
        return Err(HelloError::Malformed);
    }
    let length = head[5] as usize;
    if !(MIN_TOKEN..=MAX_TOKEN).contains(&length) {
        return Err(HelloError::Malformed);
    }
    let mut token = vec![0u8; length];
    reader
        .read_exact(&mut token)
        .await
        .map_err(|_| HelloError::Io)?;
    let token = String::from_utf8(token).map_err(|_| HelloError::Malformed)?;
    if !valid_token(&token) {
        return Err(HelloError::Malformed);
    }
    Ok(token)
}

pub async fn write_reply<W: AsyncWrite + Unpin>(
    writer: &mut W,
    accepted: bool,
) -> Result<(), HelloError> {
    writer
        .write_all(&[if accepted { ACCEPTED } else { DENIED }])
        .await
        .map_err(|_| HelloError::Io)
}

pub async fn read_reply<R: AsyncRead + Unpin>(reader: &mut R) -> Result<bool, HelloError> {
    let mut byte = [0u8; 1];
    reader
        .read_exact(&mut byte)
        .await
        .map_err(|_| HelloError::Io)?;
    match byte[0] {
        ACCEPTED => Ok(true),
        DENIED => Ok(false),
        _ => Err(HelloError::Malformed),
    }
}

/// What the host keeps instead of the token itself.
pub fn digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

/// Compares in constant time, on digests, so neither length nor content leaks.
pub fn token_matches(expected: &[u8; 32], supplied: &str) -> bool {
    bool::from(digest(supplied).ct_eq(expected))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "abcdefghijklmnopqrstuvwxyzABCDEF0123456789_-";

    #[tokio::test]
    async fn a_hello_round_trips_and_only_the_right_token_matches() {
        let wire = encode_hello(TOKEN).unwrap();
        assert_eq!(&wire[..4], b"RLYI");
        assert_eq!(read_hello(&mut &wire[..]).await.unwrap(), TOKEN);
        let expected = digest(TOKEN);
        assert!(token_matches(&expected, TOKEN));
        assert!(!token_matches(
            &expected,
            "abcdefghijklmnopqrstuvwxyzABCDEF0123456789_X"
        ));
        assert!(!token_matches(&expected, &TOKEN[..30]));
        assert!(!token_matches(&expected, ""));
    }

    #[tokio::test]
    async fn malformed_hellos_are_refused_and_never_read_past_their_length() {
        assert_eq!(encode_hello("short"), Err(HelloError::BadToken));
        assert_eq!(encode_hello(&"x".repeat(65)), Err(HelloError::BadToken));
        assert_eq!(
            encode_hello("has space has space has space"),
            Err(HelloError::BadToken)
        );
        let good = encode_hello(TOKEN).unwrap();
        for (name, wire) in [
            ("wrong magic", [b"XXXX".as_slice(), &good[4..]].concat()),
            ("wrong version", [&good[..4], &[9u8], &good[5..]].concat()),
            ("zero length", [&good[..5], &[0u8]].concat()),
            ("too long", [&good[..5], &[200u8]].concat()),
            ("truncated", good[..good.len() - 3].to_vec()),
            ("empty", Vec::new()),
        ] {
            assert!(read_hello(&mut &wire[..]).await.is_err(), "{name}");
        }
        // A non-token byte inside an otherwise well-formed hello is refused.
        let mut hostile = good.clone();
        hostile[10] = b' ';
        assert_eq!(
            read_hello(&mut &hostile[..]).await,
            Err(HelloError::Malformed)
        );
        // Extra bytes after the token are left unread.
        let mut with_more = good.clone();
        with_more.extend_from_slice(b"GET / HTTP/1.1\r\n");
        let mut reader = &with_more[..];
        assert_eq!(read_hello(&mut reader).await.unwrap(), TOKEN);
        assert_eq!(reader, b"GET / HTTP/1.1\r\n");
    }

    #[tokio::test]
    async fn the_reply_is_one_byte_and_anything_else_is_a_protocol_error() {
        let mut out = Vec::new();
        write_reply(&mut out, true).await.unwrap();
        write_reply(&mut out, false).await.unwrap();
        assert_eq!(out, [ACCEPTED, DENIED]);
        let mut reader = &out[..];
        assert!(read_reply(&mut reader).await.unwrap());
        assert!(!read_reply(&mut reader).await.unwrap());
        assert_eq!(
            read_reply(&mut &[7u8][..]).await,
            Err(HelloError::Malformed)
        );
        assert_eq!(read_reply(&mut &[][..]).await, Err(HelloError::Io));
    }
}
