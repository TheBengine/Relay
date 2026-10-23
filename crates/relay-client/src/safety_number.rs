//! R3.3: one shared safety number for a pair of devices.
//!
//! Both people compute the same 60 digits from the two public device
//! fingerprints and compare them aloud (on a call or in person) instead of
//! copying and pasting codes. The number is derived only from public
//! fingerprints, which already bind the community, account, device and
//! signing key; it contains no secret and is never sent anywhere.
//!
//! Design (Signal-like): each fingerprint is hashed 5,200 times with SHA-512
//! (slowing down any search for a colliding key), and the first 30 bytes of
//! each result become six groups of five digits. The two halves are joined in
//! the order of the sorted fingerprints, so the number is symmetric.
use sha2::{Digest, Sha512};

const DOMAIN: &[u8] = b"relay-safety-v1";
const ITERATIONS: usize = 5200;
/// Six groups per fingerprint, two fingerprints.
pub const GROUPS: usize = 12;

/// A full fingerprint is 64 hexadecimal characters (spaces are ignored).
fn normalize(fingerprint: &str) -> Option<String> {
    if fingerprint.len() > 256 {
        return None;
    }
    let clean: String = fingerprint
        .chars()
        .filter(|c| !c.is_ascii_whitespace())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    (clean.len() == 64 && clean.bytes().all(|b| b.is_ascii_hexdigit())).then_some(clean)
}

fn half(fingerprint: &str) -> [u32; 6] {
    let mut digest = Sha512::new()
        .chain_update(DOMAIN)
        .chain_update(fingerprint.as_bytes())
        .finalize();
    for _ in 1..ITERATIONS {
        digest = Sha512::new()
            .chain_update(digest)
            .chain_update(DOMAIN)
            .chain_update(fingerprint.as_bytes())
            .finalize();
    }
    let mut groups = [0u32; 6];
    for (index, group) in groups.iter_mut().enumerate() {
        let mut value = 0u64;
        for byte in &digest[index * 5..index * 5 + 5] {
            value = (value << 8) | u64::from(*byte);
        }
        *group = (value % 100_000) as u32;
    }
    groups
}

/// The safety number of two devices, or `None` when either fingerprint is not
/// a full 64-character hexadecimal code or both are the same device.
/// `safety_number(a, b) == safety_number(b, a)`.
pub fn safety_number(first: &str, second: &str) -> Option<[u32; GROUPS]> {
    let mut pair = [normalize(first)?, normalize(second)?];
    if pair[0] == pair[1] {
        return None;
    }
    pair.sort();
    let mut groups = [0u32; GROUPS];
    groups[..6].copy_from_slice(&half(&pair[0]));
    groups[6..].copy_from_slice(&half(&pair[1]));
    Some(groups)
}

/// Three rows of four five-digit groups, as shown to the people comparing.
pub fn rows(number: &[u32; GROUPS]) -> [String; 3] {
    std::array::from_fn(|row| {
        number[row * 4..row * 4 + 4]
            .iter()
            .map(|group| format!("{group:05}"))
            .collect::<Vec<_>>()
            .join("  ")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a() -> String {
        "a1".repeat(32)
    }
    fn b() -> String {
        "b2".repeat(32)
    }

    #[test]
    fn both_people_see_the_same_number() {
        assert_eq!(safety_number(&a(), &b()), safety_number(&b(), &a()));
    }

    #[test]
    fn matches_an_independent_reference_implementation() {
        // Computed separately with Python's hashlib for the same definition:
        // h = SHA-512(domain || fp); repeat 5199 times h = SHA-512(h || domain
        // || fp); six groups of int(h[5i..5i+5]) % 100000. "a1…" sorts first.
        assert_eq!(
            safety_number(&a(), &b()),
            Some([
                81121, 58403, 89735, 79572, 44364, 23150, 97396, 63687, 63161, 89475, 39566, 50299
            ])
        );
    }

    #[test]
    fn a_changed_fingerprint_changes_the_number() {
        let original = safety_number(&a(), &b()).unwrap();
        let mut other = a();
        other.replace_range(..2, "a2");
        let changed = safety_number(&other, &b()).unwrap();
        assert_ne!(original, changed);
        // Each half depends only on its own fingerprint.
        assert_eq!(original[6..], changed[6..]);
        assert_ne!(original[..6], changed[..6]);
    }

    #[test]
    fn case_and_spacing_do_not_matter_but_invalid_codes_are_refused() {
        let spaced = format!("  {} {}\n", a().to_uppercase(), "");
        assert_eq!(safety_number(&spaced, &b()), safety_number(&a(), &b()));
        for bad in ["", "abc", &"g".repeat(64), &"a".repeat(63), &"a".repeat(65)] {
            assert_eq!(safety_number(bad, &b()), None, "{bad:?}");
        }
        assert_eq!(safety_number(&"a".repeat(1000), &b()), None);
        // The same device twice is not a comparison.
        assert_eq!(safety_number(&a(), &a()), None);
        assert_eq!(safety_number(&a(), &a().to_uppercase()), None);
    }

    #[test]
    fn rows_are_three_lines_of_four_five_digit_groups() {
        let number = safety_number(&a(), &b()).unwrap();
        let rows = rows(&number);
        assert_eq!(rows[0], "81121  58403  89735  79572");
        for row in &rows {
            let groups: Vec<_> = row.split("  ").collect();
            assert_eq!(groups.len(), 4);
            assert!(groups
                .iter()
                .all(|g| g.len() == 5 && g.bytes().all(|b| b.is_ascii_digit())));
        }
        // Small values keep their leading zeros.
        let mut small = [0u32; GROUPS];
        small[0] = 7;
        assert!(super::rows(&small)[0].starts_with("00007"));
    }
}
