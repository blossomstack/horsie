//! The scope key: which account a durable row belongs to.
//!
//! A random string rather than an autoincrementing integer, because a
//! sequential key published as a scope leaks how many accounts a deployment has
//! and makes the set enumerable.

use rand::Rng;

/// Crockford base32, lowercase: the ten digits and the twenty-six letters less
/// `i`, `l`, `o` and `u`.
///
/// Case-*insensitive* on purpose. A user id becomes a directory name under
/// `<state_dir>/server/users/<id>/`, and macOS APFS is case-insensitive by
/// default — so a case-sensitive alphabet could collide two distinct ids on one
/// filesystem.
const ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

/// 12 characters at 5 bits each: 60 bits, so a collision becomes likely
/// somewhere past a billion accounts.
const LEN: usize = 12;

/// The owner of every scoped row.
///
/// Not a secret and not a credential — a caller still presents a token, which
/// is what is actually verified. Unguessability here is defence in depth.
/// `Serialize`/`Deserialize` are transparent, so an id is a bare string on any
/// wire it crosses — including a clustered command, which names the account
/// whose services the actor receiving it must be built against.
#[derive(Clone, PartialEq, Eq, Hash, Debug, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct UserId(String);

impl UserId {
    /// The account every pre-existing row was backfilled to.
    ///
    /// `0024_user_scoping.sql` writes this id onto every row that existed
    /// before scoping, so the *first* account a deployment has must be this id
    /// rather than a random one — otherwise the server comes up unable to see
    /// its own data. It is a legitimate id, not a sentinel; every account after
    /// it gets [`generate`](Self::generate).
    ///
    /// It is also the one predictable id in the system, and that is fine: ids
    /// are random to stop the *set* being enumerable and to stop a count
    /// leaking. Knowing a deployment has a first account leaks neither.
    #[must_use]
    pub fn bootstrap() -> UserId {
        UserId("1".to_string())
    }

    /// A fresh random id.
    #[must_use]
    pub fn generate() -> UserId {
        let mut bytes = [0u8; LEN];
        // The same generator `auth::token::generate` uses, for the same reason:
        // rand 0.10 removed `rngs::OsRng` and its replacement is fallible,
        // whereas `rand::rng()` is an infallible `CryptoRng` seeded from the OS.
        rand::rng().fill_bytes(&mut bytes);
        // Masking to 5 bits indexes the alphabet directly. The bias a modulo
        // would introduce is avoided because 32 divides 256 exactly.
        let s = bytes
            .iter()
            .map(|b| ALPHABET[(b & 0x1f) as usize] as char)
            .collect();
        UserId(s)
    }

    /// Wrap an id read from storage or a request.
    pub fn new(s: impl Into<String>) -> UserId {
        UserId(s.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for UserId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn a_generated_id_is_twelve_crockford_base32_characters() {
        let id = UserId::generate();
        assert_eq!(id.as_str().len(), 12, "{}", id.as_str());
        for c in id.as_str().chars() {
            assert!(
                ALPHABET.contains(&(c as u8)),
                "{c:?} is not in the alphabet, in {}",
                id.as_str()
            );
        }
    }

    /// The four letters Crockford drops are the ones that are misread when
    /// somebody copies an id out of a log by hand.
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
        let ids: HashSet<String> = (0..1000)
            .map(|_| UserId::generate().as_str().to_string())
            .collect();
        assert_eq!(ids.len(), 1000);
    }

    #[test]
    fn an_id_round_trips_through_new_and_as_str() {
        assert_eq!(UserId::new("abc123").as_str(), "abc123");
    }
}
