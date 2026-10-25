//! Has a newer signed route update already replaced a queued one?
//!
//! The independent directory refuses an old record once a newer one for the same
//! capability ID is stored. A queued record that keeps being refused for that reason
//! is finished, not failed: its work is already done by a newer, authoritative record.
//! This recognises that case, and only that case, from the two records alone. It does
//! no network, file or thread work, decrypts nothing (it never sees an invite verifier
//! or key), and trusts the directory for nothing: both records must stand on their own
//! signatures and complete signed ancestry.
//!
//! "Superseded" is only an acknowledgement that a newer opaque record exists. It does
//! not say the new host is online and it never lets anyone sign in.
use crate::host_route::{history_connects, valid_pin, verify};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use relay_protocol::{
    handover::RoutePin,
    host_locator::{self as locator, Record},
};

const NONCE_BYTES: usize = 12;
const MIN_CIPHERTEXT_BYTES: usize = 16;
const FUTURE_SKEW_MS: u64 = 30_000;
/// Terms and times are stored as signed 64-bit integers by the directory.
const SQL_MAX: u64 = i64::MAX as u64;

fn hex64(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Canonical base64url holding `min..=max` bytes.
fn canonical(value: &str, min: usize, max: usize) -> bool {
    if value.len() > max / 3 * 4 + 4 {
        return false;
    }
    URL_SAFE_NO_PAD
        .decode(value)
        .is_ok_and(|raw| (min..=max).contains(&raw.len()) && URL_SAFE_NO_PAD.encode(&raw) == value)
}

fn text(record: &Record) -> Option<String> {
    serde_json::to_string(record).ok()
}

/// Bounded, canonical, unexpired, not from the future, and standing on its own: valid
/// pins, a complete signed chain from the anchor to exactly `current` (every signature
/// and every ancestry hash), and `current`'s signature over the record.
fn standing(record: &Record, now_ms: u64) -> bool {
    let terms = |pin: &RoutePin| pin.term <= SQL_MAX;
    serde_json::to_vec(record).is_ok_and(|v| v.len() <= locator::MAX_RECORD_BYTES)
        && hex64(&record.id)
        && canonical(&record.nonce, NONCE_BYTES, NONCE_BYTES)
        && canonical(
            &record.ciphertext,
            MIN_CIPHERTEXT_BYTES,
            locator::MAX_CIPHERTEXT_BYTES,
        )
        && record.issued_ms > 0
        && record.issued_ms <= now_ms.saturating_add(FUTURE_SKEW_MS)
        && record.expires_ms > now_ms
        && record.expires_ms >= record.issued_ms
        && record.expires_ms <= SQL_MAX
        && record.expires_ms - record.issued_ms <= locator::MAX_LIFETIME_MS
        && terms(&record.anchor)
        && terms(&record.current)
        && record.history.iter().all(|g| {
            g.receipt.body.term > 0
                && g.next_term > 0
                && g.receipt.body.term <= SQL_MAX
                && g.next_term <= SQL_MAX
        })
        && valid_pin(&record.anchor)
        && valid_pin(&record.current)
        && history_connects(&record.anchor, &record.current, &record.history)
        && verify(
            &record.current.host,
            &record.signature,
            &locator::record_message(record),
        )
}

/// Every pin the record's own chain passes through: its first source, then each target.
fn chain(record: &Record) -> Vec<RoutePin> {
    let Some(first) = record.history.first() else {
        return vec![record.current.clone()];
    };
    let mut pins = vec![RoutePin {
        server_id: record.current.server_id.clone(),
        host: first.source_identity.clone(),
        term: first.receipt.body.term,
    }];
    for grant in &record.history {
        pins.push(RoutePin {
            server_id: record.current.server_id.clone(),
            host: grant.receipt.body.target_identity.clone(),
            term: grant.next_term,
        });
    }
    pins
}

/// True when `stored` is a valid, unexpired record for the very same capability ID and
/// anchor that makes retrying `queued` pointless: it is the same record, a strictly
/// later revision by the same authority over the same history, or the record of a
/// strictly higher-term authority whose complete history extends `queued`'s and so
/// proves `queued`'s current host is its ancestor. A lower or equal term with another
/// key, a different body at the same revision, rewritten or shortened ancestry, another
/// anchor or community, a wrong ID, and anything stale, future, malformed or forged are
/// all false.
pub(crate) fn supersedes(queued: &Record, stored: &Record, now_ms: u64) -> bool {
    if queued.id != stored.id
        || queued.anchor != stored.anchor
        || !standing(queued, now_ms)
        || !standing(stored, now_ms)
    {
        return false;
    }
    // The very same record.
    if text(queued).is_some_and(|q| text(stored).as_deref() == Some(q.as_str())) {
        return true;
    }
    // A strictly later revision by the same authority over the same history.
    if stored.current == queued.current {
        return stored.history == queued.history && stored.issued_ms > queued.issued_ms;
    }
    // A strictly higher authority that keeps the queued history as its own beginning.
    let kept = queued.history.len();
    stored.current.term > queued.current.term
        && stored.history.len() > kept
        && stored.history[..kept] == queued.history[..]
        && chain(stored).contains(&queued.current)
}

#[cfg(test)]
#[path = "host_locator_ack_tests.rs"]
mod tests;
