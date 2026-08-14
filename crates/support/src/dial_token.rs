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
//! dial knows which secret to check it against. A sandbox learning its own
//! account id is not a disclosure: it is that account's own sandbox.
//!
//! "Account" is whoever owns the secret, which is not always a user: a vendor
//! process signs with a secret of its own and puts its `--name` in the field.
//! Both are caller-chosen strings, which is why the format is parsed from the
//! right — see [`split`].

use hmac::digest::KeyInit;
use hmac::{Hmac, Mac};
use sha2::Sha256;

/// Who a presented token says it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialClaims {
    pub user_id: String,
    pub runtime_id: String,
    /// Which provision of that runtime this token belongs to.
    ///
    /// A runtime id is stable across re-provisions — it is the session — so it
    /// alone cannot tell a live sandbox from one left behind by an earlier
    /// attempt. Both would answer to the same name, and both would receive the
    /// same tool call. This is what separates them.
    pub incarnation: String,
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

/// `<user_id>.<runtime_id>.<incarnation>.<hex tag>`.
#[must_use]
pub fn mint(secret: &[u8], claims: &DialClaims) -> String {
    let payload = format!(
        "{}.{}.{}",
        claims.user_id, claims.runtime_id, claims.incarnation
    );
    let tag = tag(secret, &payload);
    format!("{payload}.{tag}")
}

/// Split a token into its four parts, **from the right**.
///
/// The last three fields are `.`-free by construction — a hex tag, a uuid
/// incarnation, and a runtime id that is a session uuid — so everything before
/// them is the account, dots and all. Splitting from the left instead made a
/// dotted account id (a delegating front layer whose subject is an email) or a
/// dotted vendor name (`horsie connect --name mac.local`, which mints under its
/// own name) produce a token that refused *every* dial-back as malformed.
///
/// Which is why the account stays the *first* field and every field added since
/// goes on the right: the moment two trailing fields could contain a dot, this
/// stops being able to tell them apart, and the failure is a token that
/// verifies for the wrong runtime rather than one that is rejected.
fn split(token: &str) -> Option<(&str, &str, &str, &str)> {
    let (payload, presented) = token.rsplit_once('.')?;
    let (payload, incarnation) = payload.rsplit_once('.')?;
    let (user_id, runtime_id) = payload.rsplit_once('.')?;
    if user_id.is_empty() || runtime_id.is_empty() || incarnation.is_empty() || presented.is_empty()
    {
        return None;
    }
    Some((user_id, runtime_id, incarnation, presented))
}

/// The account a token claims, before anything has verified it.
///
/// Only ever a hint: whoever reads it has to find that account's secret and
/// check the tag before believing any of it. Exposed so the parsing rule lives
/// in one place — a caller that split the token its own way would disagree with
/// [`verify`] on exactly the dotted names this format exists to tolerate.
#[must_use]
pub fn claimed_account(token: &str) -> Option<&str> {
    split(token).map(|(user_id, _, _, _)| user_id)
}

/// Verify a presented token and recover what it claims.
pub fn verify(secret: &[u8], token: &str) -> Result<DialClaims, DialTokenError> {
    let Some((user_id, runtime_id, incarnation, presented)) = split(token) else {
        return Err(DialTokenError::Malformed);
    };
    let expected = tag(secret, &format!("{user_id}.{runtime_id}.{incarnation}"));
    if expected.is_empty() || !constant_time_eq(expected.as_bytes(), presented.as_bytes()) {
        return Err(DialTokenError::BadSignature);
    }
    Ok(DialClaims {
        user_id: user_id.to_string(),
        runtime_id: runtime_id.to_string(),
        incarnation: incarnation.to_string(),
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
            incarnation: "i1".to_string(),
        }
    }

    /// The property the incarnation exists for. A sandbox left over from an
    /// earlier provision of the *same* session holds a token naming the old
    /// incarnation, so it cannot be mistaken for the current one — which is
    /// what stops two sandboxes both receiving one tool call and running it
    /// twice.
    #[test]
    fn a_token_for_one_incarnation_does_not_verify_as_another() {
        let token = mint(b"secret", &claims());
        let forged = token.replacen("i1", "i2", 1);
        assert_eq!(
            verify(b"secret", &forged),
            Err(DialTokenError::BadSignature)
        );
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

    /// A vendor mints under its own `--name`, and an account id can come from a
    /// front layer that uses an email as the subject. Neither is `.`-free, and
    /// splitting from the left turned both into a token nothing could verify —
    /// so every runtime that vendor started was refused at the door.
    #[test]
    fn an_account_with_dots_in_it_still_round_trips() {
        for user_id in ["mac.local", "someone@example.com", "a.b.c.d"] {
            let claims = DialClaims {
                user_id: user_id.to_string(),
                runtime_id: "s1".to_string(),
                incarnation: "i1".to_string(),
            };
            let token = mint(b"secret", &claims);
            assert_eq!(verify(b"secret", &token).unwrap(), claims);
            assert_eq!(claimed_account(&token), Some(user_id));
        }
    }

    #[test]
    fn the_claimed_account_reads_the_same_field_verify_does() {
        assert_eq!(claimed_account("u1.s1.i1.deadbeef"), Some("u1"));
        // Guards the lookup a claim feeds: an empty account must never become a
        // lookup for the empty account.
        assert_eq!(claimed_account(".s1.i1.deadbeef"), None);
        assert_eq!(claimed_account("s1.i1.deadbeef"), None);
        assert_eq!(claimed_account(""), None);
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
