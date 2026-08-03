//! Test databases, one per backend the run can reach.
//!
//! Every store test and the journal conformance suite go through [`backends`],
//! so a query that works on SQLite and breaks on PostgreSQL fails in the same
//! test that covers it rather than in a deployment. SQLite is always available;
//! PostgreSQL joins in when `HORSIE_TEST_POSTGRES_URL` points at a scratch
//! server.
//!
//! Set `HORSIE_REQUIRE_POSTGRES_TESTS=1` to turn a missing URL into a failure.
//! CI sets both, so the half of the suite that is the entire point of the
//! two-backend work cannot quietly stop running.

use crate::db::{Db, Dialect};
use sqlx::any::AnyPoolOptions;
use uuid::Uuid;

/// A database a test may use, kept alive for the test's duration.
///
/// The temp dir is held because dropping it deletes the SQLite file out from
/// under the pool.
pub struct TestDb {
    pub db: Db,
    _tmp: Option<tempfile::TempDir>,
}

impl TestDb {
    pub fn db(&self) -> &Db {
        &self.db
    }

    pub fn dialect(&self) -> Dialect {
        self.db.dialect()
    }
}

/// Every backend this run can exercise, each freshly migrated and empty.
///
/// A test iterates the result and runs its assertions once per backend.
pub async fn backends() -> Vec<TestDb> {
    let mut out = vec![sqlite().await];
    if let Some(pg) = postgres().await {
        out.push(pg);
    }
    out
}

/// A fresh SQLite database in a temp dir.
///
/// A file rather than `:memory:`: an in-memory database lives per connection
/// unless shared-cache is negotiated, and the pool hands out several.
pub async fn sqlite() -> TestDb {
    let tmp = tempfile::tempdir().expect("create temp dir for the test database");
    let url = format!("sqlite://{}/test.db", tmp.path().display());
    let db = Db::open(&url, 5).await.expect("open the test sqlite database");
    TestDb {
        db,
        _tmp: Some(tmp),
    }
}

/// A freshly created PostgreSQL database, or `None` when none is configured.
///
/// Each call creates its own database rather than sharing one, because store
/// tests assert on whole-table contents and would otherwise see each other's
/// rows. The databases are not dropped afterwards: the test server is expected
/// to be disposable (a CI service container, or a local scratch instance), and
/// a cleanup pass keyed on a name prefix would race other test binaries running
/// concurrently under `cargo test`.
pub async fn postgres() -> Option<TestDb> {
    let Some(base) = std::env::var("HORSIE_TEST_POSTGRES_URL").ok().filter(|s| !s.is_empty()) else {
        assert!(
            std::env::var("HORSIE_REQUIRE_POSTGRES_TESTS").as_deref() != Ok("1"),
            "HORSIE_REQUIRE_POSTGRES_TESTS=1 but HORSIE_TEST_POSTGRES_URL is unset — \
             the PostgreSQL half of the suite would have been skipped silently"
        );
        return None;
    };

    sqlx::any::install_default_drivers();
    let name = format!("horsie_test_{}", Uuid::new_v4().simple());
    let admin = AnyPoolOptions::new()
        .max_connections(1)
        .connect(&base)
        .await
        .unwrap_or_else(|e| panic!("connect to HORSIE_TEST_POSTGRES_URL: {e}"));
    // The database name is generated here, never user input, so interpolating
    // it is safe — and `CREATE DATABASE` takes no bind parameters anyway.
    sqlx::query(&format!("CREATE DATABASE {name}"))
        .execute(&admin)
        .await
        .unwrap_or_else(|e| panic!("create test database {name}: {e}"));
    admin.close().await;

    let db = Db::open(&swap_database(&base, &name), 5)
        .await
        .unwrap_or_else(|e| panic!("open test database {name}: {e}"));
    TestDb { db, _tmp: None }.into()
}

/// Replace the database component of a PostgreSQL URL, preserving any query
/// string (`?sslmode=…`), which managed providers routinely require.
fn swap_database(url: &str, database: &str) -> String {
    let (head, tail) = url.split_once('?').map_or((url, None), |(h, q)| (h, Some(q)));
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
