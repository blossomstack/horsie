//! Token format shared by every authenticated surface: `hsk_<tag>_<secret>`.
//!
//! The tag makes a wrong-kind credential rejectable before touching the
//! database, and the `hsk_` prefix makes tokens greppable and recognisable to
//! secret scanners. Only `SHA-256(secret)` is ever stored: a plain hash is
//! correct for a 256-bit random secret on the hot path of every request —
//! there is nothing to brute-force — whereas passwords, which have no such
//! entropy, use argon2id (see `password.rs`).

use crate::auth::UserId;
use base64::Engine;
use rand::Rng;
use sha2::{Digest, Sha256};

/// What a token authorizes. `Web` is a browser cookie session, `Access` and
/// `Refresh` belong to the CLI, `Agent` to a headless vendor agent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenKind {
    Web,
    Access,
    Refresh,
    Agent,
}

impl TokenKind {
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Web => "web",
            Self::Access => "usr",
            Self::Refresh => "ref",
            Self::Agent => "agt",
        }
    }

    pub fn from_tag(tag: &str) -> Option<Self> {
        match tag {
            "web" => Some(Self::Web),
            "usr" => Some(Self::Access),
            "ref" => Some(Self::Refresh),
            "agt" => Some(Self::Agent),
            _ => None,
        }
    }

    /// The `auth_tokens.kind` column value.
    pub fn as_db(&self) -> &'static str {
        match self {
            Self::Web => "web",
            Self::Access => "access",
            Self::Refresh => "refresh",
            Self::Agent => "agent",
        }
    }

    pub fn from_db(s: &str) -> Option<Self> {
        match s {
            "web" => Some(Self::Web),
            "access" => Some(Self::Access),
            "refresh" => Some(Self::Refresh),
            "agent" => Some(Self::Agent),
            _ => None,
        }
    }
}

/// Who a request is acting as. `Agent` arrives with sub-project C; until then
/// a verified credential is always a user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Principal {
    Anonymous,
    User(UserId),
}

impl Principal {
    pub fn to_db(&self) -> String {
        match self {
            Self::Anonymous => "anonymous".to_string(),
            Self::User(id) => format!("user:{id}"),
        }
    }

    pub fn from_db(s: &str) -> Result<Self, String> {
        if s == "anonymous" {
            return Ok(Self::Anonymous);
        }
        match s.split_once(':') {
            // No parse step: the id is already the string form. An empty one is
            // still rejected, because `user:` names no account.
            Some(("user", id)) if !id.is_empty() => Ok(Self::User(UserId::new(id))),
            _ => Err(format!("unrecognized principal {s:?}")),
        }
    }
}

/// A freshly minted token: the secret to hand out once, and the hash to store.
pub struct GeneratedToken {
    pub secret: String,
    pub hash: Vec<u8>,
}

pub fn generate(kind: TokenKind) -> GeneratedToken {
    let mut bytes = [0u8; 32];
    // rand 0.10 removed `rngs::OsRng`; its replacement (`SysRng`) is fallible,
    // which would make minting a token a `Result` all the way up. `rand::rng()`
    // is a `CryptoRng` seeded from the OS and periodically reseeded — the
    // crate's own recommendation for secrets — and stays infallible.
    rand::rng().fill_bytes(&mut bytes);
    let body = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let secret = format!("hsk_{}_{body}", kind.tag());
    let hash = hash_secret(&secret);
    GeneratedToken { secret, hash }
}

pub fn hash_secret(secret: &str) -> Vec<u8> {
    Sha256::digest(secret.as_bytes()).to_vec()
}

/// The kind a presented secret claims to be, or `None` if it is not one of
/// ours. Claiming a kind is not proof of anything — the caller still looks the
/// hash up — but it lets a wrong-kind credential be rejected for free.
pub fn parse(secret: &str) -> Option<TokenKind> {
    let rest = secret.strip_prefix("hsk_")?;
    let (tag, body) = rest.split_once('_')?;
    if body.is_empty() {
        return None;
    }
    TokenKind::from_tag(tag)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn generated_secret_has_the_documented_shape() {
        let t = generate(TokenKind::Web);
        assert!(t.secret.starts_with("hsk_web_"), "{}", t.secret);
        // 32 random bytes, base64url no-pad
        assert_eq!(t.secret.len(), "hsk_web_".len() + 43);
        assert_eq!(t.hash.len(), 32);
        assert!(
            t.secret
                .trim_start_matches("hsk_web_")
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
    }

    #[test]
    fn two_generated_secrets_differ() {
        assert_ne!(
            generate(TokenKind::Web).secret,
            generate(TokenKind::Web).secret
        );
    }

    #[test]
    fn hash_is_deterministic_and_matches_generation() {
        let t = generate(TokenKind::Agent);
        assert_eq!(hash_secret(&t.secret), t.hash);
        assert_eq!(hash_secret("hsk_web_abc"), hash_secret("hsk_web_abc"));
        assert_ne!(hash_secret("hsk_web_abc"), hash_secret("hsk_web_abd"));
    }

    #[test]
    fn every_kind_round_trips_through_its_tag() {
        for kind in [
            TokenKind::Web,
            TokenKind::Access,
            TokenKind::Refresh,
            TokenKind::Agent,
        ] {
            assert_eq!(TokenKind::from_tag(kind.tag()), Some(kind));
            assert_eq!(parse(&generate(kind).secret), Some(kind));
        }
    }

    #[test]
    fn parse_rejects_junk() {
        assert_eq!(parse("hsk_nope_aaa"), None);
        assert_eq!(parse("bearer-token"), None);
        assert_eq!(parse(""), None);
        assert_eq!(parse("hsk_web"), None);
    }

    #[test]
    fn principals_round_trip_through_the_database_encoding() {
        let id = UserId::new("k3m9x0abc7qr");
        assert_eq!(Principal::User(id.clone()).to_db(), "user:k3m9x0abc7qr");
        assert_eq!(
            Principal::from_db("user:k3m9x0abc7qr"),
            Ok(Principal::User(id))
        );
        // Any non-empty id is now well-formed -- the id is opaque, so there is
        // nothing left to parse and nothing to reject but emptiness.
        assert!(Principal::from_db("user:").is_err());
        assert!(Principal::from_db("wat").is_err());
    }
}
