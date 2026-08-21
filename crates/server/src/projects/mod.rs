//! Projects: the scope every durable row belongs to, and the account that owns
//! them.
//!
//! A user holds one or more projects and nothing is shared between them — a
//! second project starts with no providers, no agents, no memories and no
//! sessions. The isolation is structural rather than filtered: two projects
//! hold different stores, different actors and different vendor maps, which is
//! the same rule the account bundle was built on.
//!
//! [`ProjectId`] is therefore the *scope* key, and [`crate::auth::UserId`] is
//! now only an *identity* key: it says who may reach a project, and appears on
//! `auth_users`, `auth_tokens`, `auth_device_codes` and `projects.user_id` —
//! nowhere else.

/// The per-project bundle: the actors, clients and stores one project owns.
///
/// Named for what the code below has always called it rather than
/// `services`, which would sit next to `service` — the CRUD over the
/// `projects` table — and read as a plural of it. They are unrelated: one is
/// what a project *is made of*, the other is how a project is *created*.
pub mod bundle;
mod service;
mod store;

pub use bundle::{
    ProjectRegistry, ProjectServices, Shared, node_system, register_session_shards, resolve,
};
pub use service::{ProjectError, ProjectService, SCOPED_TABLES};
pub use store::{DEFAULT_NAME, ProjectRow, ProjectStore};

/// Which project a durable row, an actor and a bus topic belong to.
///
/// Not a secret and not a credential — a caller still presents a token, and
/// [`crate::http::Scope`] still checks that the token's user owns this project.
/// Unguessability is defence in depth, and it also keeps the *set* of projects
/// from being enumerable.
///
/// `Serialize`/`Deserialize` are transparent, so an id is a bare string on any
/// wire it crosses — including a clustered command, which names the project
/// whose services the actor receiving it must be built against.
#[derive(Clone, PartialEq, Eq, Hash, Debug, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct ProjectId(String);

impl ProjectId {
    /// A fresh random id.
    ///
    /// The only way an id is minted in Rust — for a new account's *default*
    /// project as much as for any other. `0040_projects.sql` writes ids too,
    /// but it copies existing user ids rather than generating: it needs every
    /// pre-existing actor address and bus topic to keep rendering identically,
    /// and a migration that mints has no way to tell Rust what it chose. That
    /// is how 0024 came to backfill a hardcoded `'1'` against randomly
    /// generated accounts and leave upgraded deployments unable to see their
    /// own data.
    ///
    /// So: nothing derives a project id from a user id. The two happening to
    /// match on a migrated deployment is a fact about that database, not a rule
    /// any code may rely on.
    #[must_use]
    pub fn generate() -> ProjectId {
        ProjectId(crate::ids::random_base32(crate::ids::LEN))
    }

    /// Wrap an id read from storage or a request.
    pub fn new(s: impl Into<String>) -> ProjectId {
        ProjectId(s.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ProjectId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn an_id_round_trips_through_new_and_as_str() {
        assert_eq!(ProjectId::new("abc123").as_str(), "abc123");
    }

    /// The migration copies a user id into a project id; nothing in Rust does.
    /// This is the assertion that notices if somebody adds a `default_for`.
    #[test]
    fn a_generated_id_does_not_derive_from_anything() {
        let a = ProjectId::generate();
        let b = ProjectId::generate();
        assert_ne!(a, b);
        assert_eq!(a.as_str().len(), crate::ids::LEN);
    }
}
