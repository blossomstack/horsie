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
use std::path::Path;
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

/// Every test database this suite creates is named with one of these, which is
/// what makes them safe to collect: nothing else on the server can match.
const TEST_DB_PREFIXES: [&str; 2] = ["horsie_test_", "horsie_cluster_"];

/// How many abandoned databases one process will drop before getting on with
/// the run.
///
/// Bounded so a backlog cannot turn a test run into a cleanup job — a scratch
/// server with thousands of these would otherwise stall the first run for
/// minutes. It converges over runs instead.
const SWEEP_LIMIT: usize = 256;

/// Whether this process has already swept. One catalogue query per binary, not
/// per test.
static SWEPT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Drop test databases nothing is connected to.
///
/// The suite makes a database per test and cannot drop it at the end: there are
/// eighty-odd call sites and no teardown hook, and tying one to a guard would
/// mean threading it through every helper — the trade [`sqlite`] refuses for
/// its temp directory, for the same reason. So they are collected on the way
/// *in* instead. Without this a developer's scratch server accumulates them
/// forever; this was written after one reached ~7,800.
///
/// **Why this does not race the other binaries `cargo test` runs at the same
/// time**, which is the objection that kept them forever: a live test holds a
/// pool open against its own database, PostgreSQL refuses to drop a database
/// that has a session on it, and the candidates are read *before* this process
/// creates anything of its own. A database another binary makes after that read
/// is never a candidate, one it is still using cannot be dropped, and a drop
/// that fails anyway is ignored rather than retried. A CI service container
/// starts empty and sweeps nothing.
async fn sweep_abandoned(admin: &sqlx::AnyPool) {
    use sqlx::Row;
    use std::sync::atomic::Ordering;
    if SWEPT.swap(true, Ordering::SeqCst) {
        return;
    }
    let matches = TEST_DB_PREFIXES
        .iter()
        .map(|p| format!("d.datname LIKE '{p}%'"))
        .collect::<Vec<_>>()
        .join(" OR ");
    // `pg_stat_activity` is the liveness test. `datname` is PostgreSQL's `name`
    // type, which the `Any` driver cannot decode — hence the cast, which is the
    // same trap every other catalogue query in this crate has had to learn.
    let sql = format!(
        "SELECT CAST(d.datname AS text) FROM pg_database d \
         WHERE ({matches}) \
         AND NOT EXISTS (SELECT 1 FROM pg_stat_activity a WHERE a.datname = d.datname) \
         LIMIT {SWEEP_LIMIT}"
    );
    let Ok(rows) = sqlx::query(AssertSqlSafe(sql)).fetch_all(admin).await else {
        return; // not PostgreSQL, or no rights to look: leave well alone
    };
    for row in rows {
        let Ok(name) = row.try_get::<String, _>(0) else {
            continue;
        };
        // Ignored on purpose: the database may have been claimed between the
        // read above and here, and losing that race is the correct outcome.
        let _ = sqlx::query(AssertSqlSafe(format!("DROP DATABASE {name}")))
            .execute(admin)
            .await;
    }
}

/// Sweep using a connection of one's own, for a test binary that manages its
/// own databases rather than going through [`named_postgres`].
///
/// `base` is a `HORSIE_TEST_POSTGRES_URL`-shaped admin URL.
pub async fn sweep_abandoned_test_databases(base: &str) {
    sqlx::any::install_default_drivers();
    let Ok(admin) = AnyPoolOptions::new().max_connections(1).connect(base).await else {
        return;
    };
    sweep_abandoned(&admin).await;
    admin.close().await;
}

/// A freshly created PostgreSQL database, or `None` when none is configured.
///
/// Each call creates its own database rather than sharing one, because store
/// tests assert on whole-table contents and would otherwise see each other's
/// rows. It is not dropped when the test ends — see [`sweep_abandoned`], which
/// collects the ones nothing is using at the start of a later run.
pub async fn postgres() -> Option<Db> {
    named_postgres(&format!("horsie_test_{}", Uuid::new_v4().simple())).await
}

/// The database belonging to `dir`, on whichever backend this run selected.
///
/// **Reopenable**, which is the whole point: calling this twice for the same
/// directory hands back the *same* database, and that is what a restart test
/// means by a second incarnation coming up on the journal the first one wrote.
/// [`db`] cannot serve that — it makes a fresh database per call.
///
/// Before this existed, the suites that restart a server opened
/// `sqlite://<dir>/config.db` themselves. That worked, but it pinned them to
/// SQLite: the PostgreSQL CI run went straight past the end-to-end tests, which
/// are the heaviest users of the journal's snapshot and compaction paths.
pub async fn db_at(dir: &Path) -> Db {
    match named_postgres(&database_name_for(dir)).await {
        Some(db) => db,
        None => Db::open(&format!("sqlite://{}/config.db", dir.display()), 5)
            .await
            .expect("open the test sqlite database"),
    }
}

/// A PostgreSQL database of exactly this name, created if it is not there yet.
///
/// `CREATE DATABASE` has no `IF NOT EXISTS`, so this asks first. The race that
/// implies — two callers creating the same name at once — cannot happen: a name
/// is either a fresh UUID or derived from one test's own directory.
async fn named_postgres(name: &str) -> Option<Db> {
    let base = std::env::var("HORSIE_TEST_POSTGRES_URL")
        .ok()
        .filter(|s| !s.is_empty())?;

    sqlx::any::install_default_drivers();
    let admin = AnyPoolOptions::new()
        .max_connections(1)
        .connect(&base)
        .await
        .unwrap_or_else(|e| panic!("connect to HORSIE_TEST_POSTGRES_URL: {e}"));
    // Before creating anything of ours, so nothing this process is about to
    // make can be a candidate.
    sweep_abandoned(&admin).await;
    let exists = sqlx::query("SELECT 1 FROM pg_database WHERE datname = $1")
        .bind(name)
        .fetch_optional(&admin)
        .await
        .unwrap_or_else(|e| panic!("look for test database {name}: {e}"))
        .is_some();
    if !exists {
        // The name is generated here, never user input, so interpolating it is
        // safe — and `CREATE DATABASE` takes no bind parameters anyway.
        sqlx::query(AssertSqlSafe(format!("CREATE DATABASE {name}")))
            .execute(&admin)
            .await
            .unwrap_or_else(|e| panic!("create test database {name}: {e}"));
    }
    admin.close().await;

    Some(
        Db::open(&swap_database(&base, name), 5)
            .await
            .unwrap_or_else(|e| panic!("open test database {name}: {e}")),
    )
}

/// A stable, legal PostgreSQL database name for `dir`.
///
/// FNV-1a over the path: the mapping only has to be deterministic and collision
/// -free among the temp directories of one run, and a hash keeps the result
/// inside PostgreSQL's 63-byte identifier limit whatever the path length.
fn database_name_for(dir: &Path) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in dir.as_os_str().as_encoded_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("horsie_test_at_{hash:016x}")
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
