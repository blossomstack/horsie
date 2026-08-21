//! The alphabet and generator every opaque identifier in this crate is minted
//! from.
//!
//! One place rather than one per id type, because the two constraints below are
//! easy to restate slightly wrong, and an id that violates either is only
//! discovered on a filesystem or in a support ticket.

use rand::Rng;

/// Crockford base32, lowercase: the ten digits and the twenty-six letters less
/// `i`, `l`, `o` and `u` — the four that are misread when somebody copies an id
/// out of a log by hand.
///
/// Case-*insensitive* on purpose. An id of this shape becomes a directory name,
/// and macOS APFS is case-insensitive by default — so a case-sensitive alphabet
/// could collide two distinct ids on one filesystem.
pub(crate) const ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

/// `len` characters at 5 bits each: at the 12 this crate uses, 60 bits, so a
/// collision becomes likely somewhere past a billion ids.
#[must_use]
pub(crate) fn random_base32(len: usize) -> String {
    let mut bytes = vec![0u8; len];
    // The same generator `auth::token::generate` uses, for the same reason:
    // rand 0.10 removed `rngs::OsRng` and its replacement is fallible, whereas
    // `rand::rng()` is an infallible `CryptoRng` seeded from the OS.
    rand::rng().fill_bytes(&mut bytes);
    // Masking to 5 bits indexes the alphabet directly. The bias a modulo would
    // introduce is avoided because 32 divides 256 exactly.
    bytes
        .iter()
        .map(|b| ALPHABET[(b & 0x1f) as usize] as char)
        .collect()
}

/// The length every id in this crate is minted at.
pub(crate) const LEN: usize = 12;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn a_generated_id_is_crockford_base32_at_the_requested_length() {
        let id = random_base32(LEN);
        assert_eq!(id.len(), LEN, "{id}");
        for c in id.chars() {
            assert!(
                ALPHABET.contains(&(c as u8)),
                "{c:?} is not in the alphabet"
            );
        }
    }

    #[test]
    fn the_alphabet_excludes_the_ambiguous_letters() {
        for c in [b'i', b'l', b'o', b'u'] {
            assert!(!ALPHABET.contains(&c), "{} should be excluded", c as char);
        }
        assert_eq!(ALPHABET.len(), 32);
    }

    /// Not a proof of randomness — a guard against a generator that returns a
    /// constant, which is the way this fails in practice.
    #[test]
    fn generated_ids_differ() {
        let ids: HashSet<String> = (0..1000).map(|_| random_base32(LEN)).collect();
        assert_eq!(ids.len(), 1000);
    }
}
