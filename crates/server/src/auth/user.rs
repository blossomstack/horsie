//! The identity key: which account a credential belongs to.
//!
//! A random string rather than an autoincrementing integer, because a
//! sequential key published as an identity leaks how many accounts a deployment
//! has and makes the set enumerable.
//!
//! Since `0040_projects.sql` this is an *identity* key only. The **scope** — the
//! thing every durable row, actor address and bus topic is keyed by — is
//! [`crate::projects::ProjectId`]. A `UserId` now appears on `auth_users`,
//! `auth_tokens`, `auth_device_codes` and `projects.user_id`, and nowhere else.

/// Which account a credential belongs to.
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
        UserId(crate::ids::random_base32(crate::ids::LEN))
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

    /// The alphabet and the length are `crate::ids`' to prove; what belongs
    /// here is that a `UserId` is minted from them at all.
    #[test]
    fn a_generated_id_uses_the_shared_alphabet() {
        let id = UserId::generate();
        assert_eq!(id.as_str().len(), crate::ids::LEN, "{}", id.as_str());
        for c in id.as_str().chars() {
            assert!(
                crate::ids::ALPHABET.contains(&(c as u8)),
                "{c:?} is not in the alphabet, in {}",
                id.as_str()
            );
        }
    }

    #[test]
    fn an_id_round_trips_through_new_and_as_str() {
        assert_eq!(UserId::new("abc123").as_str(), "abc123");
    }
}
