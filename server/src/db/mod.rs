//! The server's database handle: one pool, either backend.
//!
//! Everything durable lives here — the settings tables the Settings UI owns and
//! the actor journal (see [`journal`]) — reached through `sqlx::Any` so the same
//! store code runs on SQLite and PostgreSQL.
//!
//! `Any` was chosen over a hand-rolled `enum { Sqlite(SqlitePool),
//! Postgres(PgPool) }` because the stores use runtime `sqlx::query` throughout
//! (no compile-time `query!` macros, so no offline metadata), and every value
//! the server stores is TEXT, INTEGER or BLOB — exactly the set `AnyValueKind`
//! covers. The enum would have forced a second abstraction over `SqliteRow` vs
//! `PgRow`, which is re-implementing `Any` with less testing.
//!
//! Four things `Any` does *not* do for us, each of which would compile and then
//! fail at runtime (all verified against sqlx 0.8.6, see the design spec):
//!
//! 1. It does not rewrite placeholders — SQL reaches the driver verbatim, so a
//!    `?` would arrive at PostgreSQL as a syntax error. Hence [`Db::q`].
//! 2. `last_insert_id` is always `None` for SQLite through `Any`
//!    (`sqlx-sqlite`'s `map_result` hardcodes it), so inserts that need the
//!    assigned id use `RETURNING id`, which both backends support.
//! 3. SQLite never yields `AnyValueKind::Bool` — values are mapped by runtime
//!    type and SQLite has no boolean. Booleans are stored as INTEGER 0/1 in
//!    both dialects and read as `i64`; `try_get::<bool, _>` is a runtime error
//!    on SQLite even though it works on PostgreSQL.
//! 4. Unknown SQLite URL parameters are a hard connect error (the parser takes
//!    only `mode`, `cache`, `immutable`, `vfs`), and `AnyConnectOptions` cannot
//!    carry `SqliteConnectOptions::busy_timeout`. So WAL and the busy timeout
//!    are set by [`after_connect`] PRAGMAs and `create_if_missing` becomes
//!    `?mode=rwc` on the URL.

pub mod journal;
#[cfg(any(test, feature = "test-util"))]
pub mod testing;

use sqlx::any::{AnyPoolOptions, AnyRow};
use sqlx::{AnyPool, Executor, Row};
use std::borrow::Cow;

/// Which SQL dialect [`Db`] is talking. Selected from the URL scheme, never
/// configured directly.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dialect {
    Sqlite,
    Postgres,
}

impl Dialect {
    /// The name used in config, logs and `/api/config`.
    pub fn as_str(self) -> &'static str {
        match self {
            Dialect::Sqlite => "sqlite",
            Dialect::Postgres => "postgres",
        }
    }

    fn from_url(url: &str) -> Result<Self, String> {
        let scheme = url.split(':').next().unwrap_or_default();
        match scheme {
            "sqlite" => Ok(Dialect::Sqlite),
            "postgres" | "postgresql" => Ok(Dialect::Postgres),
            other => Err(format!(
                "unsupported database url scheme '{other}': expected sqlite:// or postgres://"
            )),
        }
    }
}

/// How long a contended SQLite write waits before failing. The pool hands out
/// several connections and authentication puts a (throttled) token write on the
/// path of every API request, so without this a write landing while another
/// connection holds the database surfaces as an immediate `database is locked`
/// instead of a short wait.
const SQLITE_BUSY_TIMEOUT_MS: u32 = 5_000;

/// The server's database handle: a pool plus the dialect it speaks.
///
/// Cheap to clone — `AnyPool` is an `Arc` internally, which is why every store
/// takes a `Db` by value rather than sharing one behind another `Arc`.
#[derive(Clone)]
pub struct Db {
    pool: AnyPool,
    dialect: Dialect,
}

impl Db {
    /// Open `url`, apply the connection settings the dialect needs, and run the
    /// migrations for it.
    ///
    /// A SQLite file is created if missing, as before. Failure here is fatal at
    /// startup by design: an unreachable database should stop the boot, not the
    /// first request that happens to touch it.
    pub async fn open(url: &str, max_connections: u32) -> Result<Db, String> {
        let dialect = Dialect::from_url(url)?;
        // Registers the compiled-in drivers with the `Any` façade. Idempotent
        // and process-global; called here so every entry point (server, tests)
        // gets it without a separate setup step.
        sqlx::any::install_default_drivers();

        let pool = AnyPoolOptions::new()
            .max_connections(max_connections)
            .after_connect(move |conn, _meta| {
                Box::pin(async move {
                    if dialect == Dialect::Sqlite {
                        // Not expressible in the URL or in `AnyConnectOptions`
                        // (see the module docs), so they are issued per
                        // connection instead.
                        conn.execute("PRAGMA journal_mode = WAL").await?;
                        conn.execute(
                            format!("PRAGMA busy_timeout = {SQLITE_BUSY_TIMEOUT_MS}").as_str(),
                        )
                        .await?;
                    }
                    Ok(())
                })
            })
            .connect(&connect_url(url, dialect))
            .await
            .map_err(|e| format!("open database '{}': {e}", redact_url(url)))?;

        let db = Db { pool, dialect };
        db.migrate().await?;
        Ok(db)
    }

    /// Run the embedded migrations for this dialect.
    ///
    /// The two directories are separate embeds because the DDL genuinely
    /// diverges (AUTOINCREMENT vs BIGSERIAL, BLOB vs BYTEA, the date
    /// functions). They are kept aligned version-for-version by
    /// `migrations_are_in_parity`, so a migration added to one and forgotten in
    /// the other fails CI rather than a deployment.
    async fn migrate(&self) -> Result<(), String> {
        let result = match self.dialect {
            Dialect::Sqlite => sqlx::migrate!("migrations/sqlite").run(&self.pool).await,
            Dialect::Postgres => sqlx::migrate!("migrations/postgres").run(&self.pool).await,
        };
        result.map_err(|e| format!("run migrations: {e}"))
    }

    pub fn pool(&self) -> &AnyPool {
        &self.pool
    }

    pub fn dialect(&self) -> Dialect {
        self.dialect
    }

    /// Rewrite `?` placeholders into `$1..$n` for PostgreSQL; identity on
    /// SQLite.
    ///
    /// Queries are written once, in SQLite's placeholder style, and translated
    /// on the way out. `?` inside a string literal is left alone — `model_cards`
    /// really does issue `LIKE ? ESCAPE '\'` — so this walks the string
    /// tracking quoting rather than doing a blind replace.
    pub fn q<'a>(&self, sql: &'a str) -> Cow<'a, str> {
        match self.dialect {
            Dialect::Sqlite => Cow::Borrowed(sql),
            Dialect::Postgres => Cow::Owned(to_dollar_placeholders(sql)),
        }
    }

    /// Run a query for its row count, wrapping the SQL in [`Db::q`] first.
    /// A convenience for the many statements that bind nothing.
    pub async fn execute(&self, sql: &str) -> Result<(), sqlx::Error> {
        let sql = self.q(sql);
        sqlx::query(&sql).execute(&self.pool).await?;
        Ok(())
    }
}

/// Translate `?` to `$1..$n`, skipping anything inside a single-quoted literal.
///
/// SQL's escape for a quote inside a literal is a doubled quote, which needs no
/// special case: the closing quote flips the state off and the immediately
/// following quote flips it straight back on.
fn to_dollar_placeholders(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len() + 8);
    let mut n = 0_u32;
    let mut in_literal = false;
    for ch in sql.chars() {
        match ch {
            '\'' => {
                in_literal = !in_literal;
                out.push(ch);
            }
            '?' if !in_literal => {
                n += 1;
                out.push('$');
                out.push_str(&n.to_string());
            }
            _ => out.push(ch),
        }
    }
    out
}

/// The URL actually handed to the pool.
///
/// SQLite needs `mode=rwc` to keep creating the database file when it is
/// missing, which `AnyConnectOptions` has no typed setter for. An in-memory or
/// already-parameterised URL is left alone.
fn connect_url(url: &str, dialect: Dialect) -> Cow<'_, str> {
    if dialect != Dialect::Sqlite || url.contains('?') || url.contains(":memory:") {
        return Cow::Borrowed(url);
    }
    Cow::Owned(format!("{url}?mode=rwc"))
}

/// A database URL with any password removed, for logs and error messages.
pub fn redact_url(url: &str) -> String {
    // Only the `scheme://user:password@host/...` form carries a secret.
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let Some((userinfo, host)) = rest.split_once('@') else {
        return url.to_string();
    };
    let user = userinfo.split_once(':').map_or(userinfo, |(u, _)| u);
    format!("{scheme}://{user}:***@{host}")
}

/// Read a column that holds a boolean as INTEGER 0/1.
///
/// SQLite never produces `AnyValueKind::Bool` through `Any`, so `try_get::<bool,
/// _>` compiles and then fails at runtime there. Every boolean column goes
/// through this instead.
pub fn get_bool(row: &AnyRow, column: &str) -> Result<bool, sqlx::Error> {
    Ok(row.try_get::<i64, _>(column)? != 0)
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
    fn sqlite_leaves_placeholders_alone() {
        assert_eq!(to_dollar_placeholders("SELECT 1"), "SELECT 1");
    }

    #[test]
    fn placeholders_are_numbered_in_order() {
        assert_eq!(
            to_dollar_placeholders("INSERT INTO t (a, b, c) VALUES (?, ?, ?)"),
            "INSERT INTO t (a, b, c) VALUES ($1, $2, $3)"
        );
    }

    #[test]
    fn a_question_mark_inside_a_literal_is_not_a_placeholder() {
        assert_eq!(
            to_dollar_placeholders("SELECT ? WHERE name = 'who?'"),
            "SELECT $1 WHERE name = 'who?'"
        );
    }

    #[test]
    fn doubled_quotes_do_not_unbalance_the_literal_scan() {
        // '' is SQL's escape for a quote inside a literal: the first quote
        // closes, the second reopens, so the scan stays correct without a
        // special case — and the trailing ? is still a placeholder.
        assert_eq!(
            to_dollar_placeholders("SELECT 'it''s' , ? FROM t"),
            "SELECT 'it''s' , $1 FROM t"
        );
    }

    #[test]
    fn the_model_cards_like_query_translates() {
        // The real statement from `model_cards::search`, which is why literal
        // awareness is load-bearing rather than theoretical.
        assert_eq!(
            to_dollar_placeholders("SELECT x FROM model_cards WHERE model_id LIKE ? ESCAPE '\\'"),
            "SELECT x FROM model_cards WHERE model_id LIKE $1 ESCAPE '\\'"
        );
    }

    #[test]
    fn ten_or_more_placeholders_keep_numbering() {
        let sql = "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)";
        assert!(to_dollar_placeholders(sql).ends_with("$10, $11)"));
    }

    #[test]
    fn dialect_comes_from_the_url_scheme() {
        assert_eq!(Dialect::from_url("sqlite://x.db").unwrap(), Dialect::Sqlite);
        assert_eq!(
            Dialect::from_url("postgres://h/db").unwrap(),
            Dialect::Postgres
        );
        assert_eq!(
            Dialect::from_url("postgresql://h/db").unwrap(),
            Dialect::Postgres
        );
        assert!(Dialect::from_url("mysql://h/db").is_err());
    }

    #[test]
    fn sqlite_urls_gain_create_if_missing() {
        assert_eq!(
            connect_url("sqlite://data.db", Dialect::Sqlite),
            "sqlite://data.db?mode=rwc"
        );
        // Already parameterised, or in-memory: left alone.
        assert_eq!(
            connect_url("sqlite://data.db?mode=ro", Dialect::Sqlite),
            "sqlite://data.db?mode=ro"
        );
        assert_eq!(
            connect_url("sqlite::memory:", Dialect::Sqlite),
            "sqlite::memory:"
        );
        assert_eq!(
            connect_url("postgres://h/db", Dialect::Postgres),
            "postgres://h/db"
        );
    }

    #[test]
    fn migrations_are_in_parity() {
        // Both directories are embedded at compile time, so this compares what
        // will actually ship rather than what is on disk right now. A migration
        // added to one backend and forgotten in the other fails here instead of
        // at someone's deployment.
        let sqlite = sqlx::migrate!("migrations/sqlite");
        let postgres = sqlx::migrate!("migrations/postgres");

        let versions = |m: &sqlx::migrate::Migrator| -> Vec<(i64, String)> {
            m.iter()
                .map(|mig| (mig.version, mig.description.to_string()))
                .collect()
        };
        assert_eq!(
            versions(&sqlite),
            versions(&postgres),
            "the sqlite and postgres migration directories must declare the same \
             versions with the same descriptions"
        );
    }

    #[test]
    fn redaction_hides_the_password_only() {
        assert_eq!(
            redact_url("postgres://user:hunter2@host:5432/horsie"),
            "postgres://user:***@host:5432/horsie"
        );
        // Nothing to redact in these shapes.
        assert_eq!(redact_url("sqlite://data.db"), "sqlite://data.db");
        assert_eq!(
            redact_url("postgres://host/horsie"),
            "postgres://host/horsie"
        );
    }
}
