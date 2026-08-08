//! The credential a runtime presents when it dials back.
//!
//! Derived, never stored: there is no per-runtime row to migrate, nothing to
//! expire, a server restart changes nothing, and rotating one secret
//! invalidates every outstanding token at once.
//!
//! A token authorises exactly one `runtime_id`. That is enough because holding
//! it is already equivalent to *being* that runtime; what it buys is that a
//! stranger cannot become one. Before this existed, a connection announced a
//! `runtime_id` and was registered as that runtime's transport with only a
//! duplicate check as a guard — sound on a private container network and
//! nowhere else.
//!
//! The payload names the account as well as the runtime, so whoever accepts the
//! dial can route it to the right account's services without a database read. A
//! sandbox learning its own account id is not a disclosure: it is that
//! account's own sandbox.

use hmac::digest::KeyInit;
use hmac::{Hmac, Mac};
use sha2::Sha256;

/// Who a presented token says it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialClaims {
    pub user_id: String,
    pub runtime_id: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DialTokenError {
    #[error("malformed dial token")]
    Malformed,
    #[error("dial token signature does not match")]
    BadSignature,
}

fn tag(secret: &[u8], payload: &str) -> String {
    // `new_from_slice` sits on `KeyInit`, not `Mac`, as of hmac 0.13 — the
    // release that pairs with sha2 0.11.
    //
    // HMAC accepts a key of any length so this cannot actually fail; matching
    // rather than unwrapping keeps the crate's no-panic lint satisfied without
    // pretending there is a recoverable case.
    let mut mac = match <Hmac<Sha256> as KeyInit>::new_from_slice(secret) {
        Ok(mac) => mac,
        Err(_) => return String::new(),
    };
    mac.update(payload.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// `<user_id>.<runtime_id>.<hex tag>`.
///
/// Both ids are `.`-free by construction — account ids and session UUIDs — so a
/// plain split is unambiguous.
#[must_use]
pub fn mint(secret: &[u8], claims: &DialClaims) -> String {
    let payload = format!("{}.{}", claims.user_id, claims.runtime_id);
    let tag = tag(secret, &payload);
    format!("{payload}.{tag}")
}

/// Verify a presented token and recover what it claims.
pub fn verify(secret: &[u8], token: &str) -> Result<DialClaims, DialTokenError> {
    let parts: Vec<&str> = token.split('.').collect();
    let [user_id, runtime_id, presented] = parts.as_slice() else {
        return Err(DialTokenError::Malformed);
    };
    if user_id.is_empty() || runtime_id.is_empty() || presented.is_empty() {
        return Err(DialTokenError::Malformed);
    }
    let expected = tag(secret, &format!("{user_id}.{runtime_id}"));
    if expected.is_empty() || !constant_time_eq(expected.as_bytes(), presented.as_bytes()) {
        return Err(DialTokenError::BadSignature);
    }
    Ok(DialClaims {
        user_id: (*user_id).to_string(),
        runtime_id: (*runtime_id).to_string(),
    })
}

/// Compared without an early exit: a byte-by-byte `==` leaks the expected tag
/// one position at a time to anyone who can time the comparison.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn claims() -> DialClaims {
        DialClaims {
            user_id: "u1".to_string(),
            runtime_id: "s1".to_string(),
        }
    }

    #[test]
    fn a_minted_token_verifies_back_to_its_claims() {
        let token = mint(b"secret", &claims());
        assert_eq!(verify(b"secret", &token).unwrap(), claims());
    }

    #[test]
    fn a_token_for_one_runtime_does_not_verify_as_another() {
        // The property the whole scheme buys: possession authorises exactly one
        // runtime. Swapping the id must break the tag rather than silently
        // re-address the token, which is what the old announce-your-own-id
        // listener allowed.
        let token = mint(b"secret", &claims());
        let forged = token.replacen("s1", "s2", 1);
        assert_eq!(
            verify(b"secret", &forged),
            Err(DialTokenError::BadSignature)
        );
    }

    #[test]
    fn a_token_for_one_account_does_not_verify_as_another() {
        let token = mint(b"secret", &claims());
        let forged = token.replacen("u1", "u2", 1);
        assert_eq!(
            verify(b"secret", &forged),
            Err(DialTokenError::BadSignature)
        );
    }

    #[test]
    fn rotating_the_secret_invalidates_every_outstanding_token() {
        let token = mint(b"old", &claims());
        assert_eq!(verify(b"new", &token), Err(DialTokenError::BadSignature));
    }

    #[test]
    fn a_malformed_token_is_rejected_without_panicking() {
        for bad in ["", ".", "..", "no-dot", "a.b", "a.b.c.d", "a..c", ".b.c"] {
            assert!(verify(b"secret", bad).is_err(), "{bad:?} must not verify");
        }
    }

    #[test]
    fn a_tag_of_the_wrong_length_is_rejected() {
        let token = mint(b"secret", &claims());
        let truncated = &token[..token.len() - 4];
        assert_eq!(
            verify(b"secret", truncated),
            Err(DialTokenError::BadSignature)
        );
    }
}
