// Test scaffolding, not production code: a database that will not open is a
// broken test environment, and failing loudly at the point of failure beats
// threading a Result through every helper that calls this.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! The test database, on whichever backend the run selects.
//!
//! Every test that needs storage calls [`db`], and the backend comes from the
//! environment rather than the test: unset means SQLite, and
//! `HORSIE_TEST_POSTGRES_URL` means a freshly created PostgreSQL database. CI
//! runs the whole suite twice, once each way, so *every* test that touches
//! storage — not just the handful written with two backends in mind — is a
//! portability test.
//!
//! That is the cheap version of a guarantee that would otherwise need each test
//! rewritten as a loop: a query that works on SQLite and breaks on PostgreSQL
//! fails in whichever test already covers that code path.

use crate::db::Db;
use sqlx::AssertSqlSafe;
use sqlx::any::AnyPoolOptions;
use uuid::Uuid;

/// A fresh, migrated, empty database on the backend this run selected.
///
/// Returns the `Db` alone, with nothing for the caller to keep alive: helpers
/// that already own a temp dir for their own fixtures can drop this straight
/// into a store constructor.
pub async fn db() -> Db {
    match postgres().await {
        Some(db) => db,
        None => sqlite().await,
    }
}

/// A fresh SQLite database in a temp dir.
///
/// A file rather than `:memory:`: an in-memory database lives per connection
/// unless shared-cache is negotiated, and the pool hands out several.
///
/// The temp dir is deliberately leaked (`keep`) rather than returned for the
/// caller to hold. Tying its lifetime to a guard would mean threading that
/// guard through every test helper, and dropping it early deletes the database
/// file out from under a live pool — a confusing failure for a saving of a few
/// kilobytes in the OS temp directory.
pub async fn sqlite() -> Db {
    let dir = tempfile::tempdir()
        .expect("create temp dir for the test database")
        .keep();
    let url = format!("sqlite://{}/test.db", dir.display());
    Db::open(&url, 5)
        .await
        .expect("open the test sqlite database")
}

/// An empty SQLite pool with **no** migrations applied.
///
/// Only for tests that reconstruct a historical schema by hand and then apply
/// one migration to it — they need the database as it was before, which
/// [`sqlite`] cannot give them because it migrates all the way up.
pub async fn unmigrated_sqlite() -> sqlx::AnyPool {
    sqlx::any::install_default_drivers();
    let dir = tempfile::tempdir()
        .expect("create temp dir for the test database")
        .keep();
    let url = format!("sqlite://{}/test.db?mode=rwc", dir.display());
    AnyPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .expect("open an unmigrated sqlite database")
}

/// A freshly created PostgreSQL database, or `None` when none is configured.
///
/// Each call creates its own database rather than sharing one, because store
/// tests assert on whole-table contents and would otherwise see each other's
/// rows. The databases are not dropped afterwards: the test server is expected
/// to be disposable (a CI service container, or a local scratch instance), and
/// a cleanup pass keyed on a name prefix would race other test binaries running
/// concurrently under `cargo test`.
pub async fn postgres() -> Option<Db> {
    let base = std::env::var("HORSIE_TEST_POSTGRES_URL")
        .ok()
        .filter(|s| !s.is_empty())?;

    sqlx::any::install_default_drivers();
    let name = format!("horsie_test_{}", Uuid::new_v4().simple());
    let admin = AnyPoolOptions::new()
        .max_connections(1)
        .connect(&base)
        .await
        .unwrap_or_else(|e| panic!("connect to HORSIE_TEST_POSTGRES_URL: {e}"));
    // The database name is generated here, never user input, so interpolating
    // it is safe — and `CREATE DATABASE` takes no bind parameters anyway.
    sqlx::query(AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .execute(&admin)
        .await
        .unwrap_or_else(|e| panic!("create test database {name}: {e}"));
    admin.close().await;

    Some(
        Db::open(&swap_database(&base, &name), 5)
            .await
            .unwrap_or_else(|e| panic!("open test database {name}: {e}")),
    )
}

/// Replace the database component of a PostgreSQL URL, preserving any query
/// string (`?sslmode=…`), which managed providers routinely require.
fn swap_database(url: &str, database: &str) -> String {
    let (head, tail) = url
        .split_once('?')
        .map_or((url, None), |(h, q)| (h, Some(q)));
    let base = head.trim_end_matches('/');
    // Everything up to the last '/' is scheme + authority; what follows is the
    // database name this replaces.
    let authority = match base.rfind('/') {
        // Guard against `postgres://host` with no path at all: the last '/' is
        // then part of the scheme separator.
        Some(i) if i > base.find("://").map_or(0, |s| s + 2) => &base[..i],
        _ => base,
    };
    match tail {
        Some(q) => format!("{authority}/{database}?{q}"),
        None => format!("{authority}/{database}"),
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;

    #[test]
    fn swapping_the_database_keeps_the_authority_and_query() {
        assert_eq!(
            swap_database("postgres://u:p@host:5432/postgres", "t1"),
            "postgres://u:p@host:5432/t1"
        );
        assert_eq!(
            swap_database("postgres://host/postgres?sslmode=require", "t1"),
            "postgres://host/t1?sslmode=require"
        );
        // No path component at all — the name is appended, not spliced into
        // the scheme separator.
        assert_eq!(swap_database("postgres://host", "t1"), "postgres://host/t1");
        assert_eq!(
            swap_database("postgres://host/", "t1"),
            "postgres://host/t1"
        );
    }
}
