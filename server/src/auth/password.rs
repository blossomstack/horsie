//! Argon2id password hashing for the single admin account.
//!
//! Passwords, unlike the 256-bit token secrets in `token.rs`, are guessable,
//! so they get a deliberately slow KDF rather than a plain hash.

use argon2::Argon2;
use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use rand::RngExt;

/// Alphanumeric only: the generated password is read off a terminal and typed
/// into a browser, so ambiguity and shell quoting are the real risks, not the
/// handful of bits a wider alphabet would add to an already-142-bit secret.
const INITIAL_PASSWORD_LEN: usize = 24;

pub fn hash(plain: &str) -> Result<String, String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(plain.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| e.to_string())
}

/// Constant-time verification. A stored hash we cannot parse is a `false`, not
/// an error: there is no caller who could do anything useful with the
/// distinction, and returning `false` fails closed.
pub fn verify(plain: &str, phc: &str) -> bool {
    match PasswordHash::new(phc) {
        Ok(parsed) => Argon2::default()
            .verify_password(plain.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

pub fn generate_initial() -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = rand::rng();
    (0..INITIAL_PASSWORD_LEN)
        .map(|_| char::from(ALPHABET[rng.random_range(0..ALPHABET.len())]))
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn a_hashed_password_verifies_and_a_wrong_one_does_not() {
        let phc = hash("correct horse").unwrap();
        assert!(phc.starts_with("$argon2id$"), "{phc}");
        assert!(verify("correct horse", &phc));
        assert!(!verify("Correct Horse", &phc));
        assert!(!verify("", &phc));
    }

    #[test]
    fn the_same_password_hashes_differently_each_time() {
        assert_ne!(hash("x").unwrap(), hash("x").unwrap());
    }

    #[test]
    fn verify_rejects_a_malformed_hash_instead_of_panicking() {
        assert!(!verify("x", "not-a-phc-string"));
        assert!(!verify("x", ""));
    }

    #[test]
    fn the_generated_initial_password_is_long_and_unpredictable() {
        let a = generate_initial();
        assert_eq!(a.chars().count(), 24);
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric()));
        assert_ne!(a, generate_initial());
    }
}
