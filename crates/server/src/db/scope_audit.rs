//! A test that reads this crate's own source and fails any SQL that touches a
//! scoped table without naming `project_id`.
//!
//! This is affordable only because two invariants already hold, both stated in
//! [`crate::db`]: every statement is a literal written in this repo, and
//! [`Db::q`](crate::db::Db::q) is the single place they pass through. It is the
//! mechanism that catches the failure a code review does not — somebody adds a
//! method six months from now and forgets the predicate.
//!
//! PostgreSQL row-level security would be the usual answer and is unavailable
//! here: SQLite has none, and SQLite is the backend every self-hoster runs. So
//! RLS could only ever be defence in depth on one deployment shape, never the
//! mechanism. This is the mechanism.
//!
//! What it does *not* catch: a statement that names `project_id` but forgets to
//! `.bind()` it. That is the isolation harness's job
//! (`tests/tests/project_isolation.rs`), and it has caught one already.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

/// Every table with a `project_id` column.
///
/// [`SCOPED_TABLES`] is the list, and `the_scoped_table_list_matches_the_schema`
/// below is what stops it going stale: it reads the migrations and fails if the
/// schema has a scoped table this does not. A hardcoded list in a drift test is
/// blind to exactly the case the test exists for — and this one already was.
/// `runtime_vendors` gained a scope in `0030` and was never audited, which is
/// how it stayed unaudited for ten migrations.
///
/// [`SCOPED_TABLES`]: crate::projects::SCOPED_TABLES
use crate::projects::SCOPED_TABLES as SCOPED;

/// Statements that touch a scoped table and must *not* be scoped, each with the
/// reason. Matched as a substring of the offending line, so the marker is
/// usually the enclosing function's name on the same line as the SQL, or a
/// distinctive fragment of the statement itself.
///
/// Adding an entry here is a decision, not a formality: every one of these
/// reads or writes across accounts by design, and getting one wrong in the
/// other direction destroys data.
const ALLOWED: &[(&str, &str)] = &[
    (
        "SELECT artifact_hash FROM plugins",
        "artifact GC needs the union of hashes across accounts: artifacts are \
         content-addressed and shared, so a scoped keep-set would delete bundle \
         bytes another account still references",
    ),
    (
        "SELECT project_id, {COLS} FROM routines",
        "one timer serves the deployment; each routine is then armed and run in \
         the project that owns it, and the row carries that for exactly this",
    ),
    (
        "DELETE FROM {table} WHERE project_id = ?",
        "deleting a project clears every scoped table by name; the table comes \
         from SCOPED_TABLES and the predicate is right there",
    ),
];

#[test]
fn every_statement_against_a_scoped_table_names_the_scope() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offences = Vec::new();
    for file in rust_files(&root) {
        // This file's own examples are SQL-shaped on purpose.
        if file.ends_with("scope_audit.rs") {
            continue;
        }
        let src = std::fs::read_to_string(&file).unwrap();
        for (line_no, statement) in sql_statements(&src) {
            if ALLOWED.iter().any(|(needle, _)| statement.contains(needle)) {
                continue;
            }
            if statement.contains("project_id") {
                continue;
            }
            if let Some(table) = SCOPED.iter().find(|t| mentions_table(&statement, t)) {
                offences.push(format!(
                    "{}:{line_no}: touches `{table}` without `project_id`\n    {statement}",
                    file.display()
                ));
            }
        }
    }
    assert!(
        offences.is_empty(),
        "unscoped SQL against scoped tables:\n\n{}\n\n\
         If one of these is deliberate, add it to ALLOWED in \
         db/scope_audit.rs — with the reason.",
        offences.join("\n\n")
    );
}

fn rust_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            out.extend(rust_files(&path));
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
    out.sort();
    out
}

/// Every statement-shaped run of source, with the 1-based line it starts on.
///
/// Deliberately crude — it does not parse Rust. A statement is a line
/// containing a SQL verb, joined with the lines that continue it, so that a
/// `WHERE user_id = ?` wrapped onto the next line still counts as part of the
/// same statement. Joining stops at a line that closes the call, which is what
/// keeps two adjacent statements from being read as one.
fn sql_statements(src: &str) -> Vec<(usize, String)> {
    const VERBS: [&str; 4] = ["SELECT ", "INSERT INTO ", "UPDATE ", "DELETE FROM "];
    // Production statements only. A test that writes an unscoped row writes it
    // into its own throwaway database, and whether the *stores* are scoped is
    // proved by the isolation harness rather than by grepping their fixtures.
    // Test modules are last in a file by this codebase's convention, so cutting
    // at the marker is exact rather than approximate.
    let src = match src.find("\nmod tests {") {
        Some(i) => &src[..i],
        None => src,
    };
    let lines: Vec<&str> = src.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        // A doc comment that mentions a statement is prose, not a query.
        let trimmed = lines[i].trim_start();
        if trimmed.starts_with("//") {
            i += 1;
            continue;
        }
        if VERBS.iter().any(|v| lines[i].contains(v)) {
            let start = i;
            let mut joined = lines[i].trim().to_string();
            // A Rust string literal continued with a trailing backslash, or a
            // statement whose closing `"` has not been seen yet, keeps going.
            while i + 1 < lines.len() && !closes_statement(lines[i]) {
                i += 1;
                joined.push(' ');
                joined.push_str(lines[i].trim());
            }
            out.push((start + 1, joined));
        }
        i += 1;
    }
    out
}

/// Whether this line ends the statement that began earlier.
///
/// A line ending in `\` is an explicit Rust string continuation. Otherwise the
/// statement ends once the line closes its literal and its call — in practice,
/// once it contains a `"` followed by `)`, `,` or `;`.
fn closes_statement(line: &str) -> bool {
    let t = line.trim_end();
    if t.ends_with('\\') {
        return false;
    }
    let Some(last_quote) = t.rfind('"') else {
        return true;
    };
    let tail = &t[last_quote + 1..];
    tail.contains(')') || tail.contains(',') || tail.contains(';') || tail.is_empty()
}

/// Whether a statement names this table as a whole word.
///
/// Substring matching would make `models` match inside `model_cards`, and
/// `plugins` match inside `plugins_new`.
fn mentions_table(statement: &str, table: &str) -> bool {
    statement
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .any(|w| w == table)
}

/// The list this file audits against must be the schema's list.
///
/// Asked of a *migrated database* rather than parsed out of the migration
/// files, for the reason 0024 learned the hard way: a constraint or a column
/// added by a later `ALTER` is invisible to anyone reading the migration that
/// created the table. The schema is the only thing that knows the schema.
///
/// SQLite only, deliberately. `db::migrations_are_in_parity` already pins that
/// the two dialects have the same migration set, so what is left here is a
/// question about shape, and asking it twice would buy nothing for the cost of
/// a second dialect's catalogue query.
#[tokio::test]
async fn the_scoped_table_list_matches_the_schema() {
    let db = crate::db::testing::sqlite().await;
    let names: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
    )
    .fetch_all(db.pool())
    .await
    .unwrap();

    let mut scoped = std::collections::BTreeSet::new();
    for name in names {
        // The name comes from the catalogue, never from input, and PRAGMA takes
        // no bind parameters.
        let columns: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT name FROM pragma_table_info('{name}')"
        )))
        .fetch_all(db.pool())
        .await
        .unwrap();
        if columns.iter().any(|c| c == "project_id") {
            scoped.insert(name);
        }
    }

    let declared: std::collections::BTreeSet<String> =
        SCOPED.iter().map(|t| (*t).to_string()).collect();
    assert_eq!(
        scoped,
        declared,
        "the schema and `projects::SCOPED_TABLES` disagree.\n\
         In the schema only: {:?}\n\
         In the list only:   {:?}\n\
         A table missing from the list is unaudited SQL *and* rows that outlive \
         the project they belonged to.",
        scoped.difference(&declared).collect::<Vec<_>>(),
        declared.difference(&scoped).collect::<Vec<_>>(),
    );
}

/// Nothing outside `projects` may still be keyed by an account.
///
/// The other half of the split: `project_id` is the scope and `user_id` is an
/// identity, so a `user_id` on a table that is neither `projects` nor an auth
/// table means a resource escaped the move — and it would be *silently*
/// unreachable rather than loud, because every store now binds the other
/// column.
#[tokio::test]
async fn only_identity_tables_are_still_keyed_by_a_user() {
    const IDENTITY: &[&str] = &["projects", "auth_users", "auth_tokens", "auth_device_codes"];

    let db = crate::db::testing::sqlite().await;
    let names: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
    )
    .fetch_all(db.pool())
    .await
    .unwrap();

    let mut offenders = Vec::new();
    for name in names {
        if IDENTITY.contains(&name.as_str()) {
            continue;
        }
        // The name comes from the catalogue, never from input.
        let columns: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT name FROM pragma_table_info('{name}')"
        )))
        .fetch_all(db.pool())
        .await
        .unwrap();
        if columns.iter().any(|c| c == "user_id") {
            offenders.push(name);
        }
    }
    assert!(
        offenders.is_empty(),
        "these tables are still scoped by account rather than by project: {offenders:?}"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_table_name_is_matched_as_a_whole_word() {
        assert!(mentions_table("SELECT x FROM models WHERE a = ?", "models"));
        // The trap this exists for: `models` is a substring of `model_cards`.
        assert!(!mentions_table("SELECT x FROM model_cards", "models"));
        assert!(!mentions_table("DROP TABLE plugins_new", "plugins"));
    }

    #[test]
    fn a_wrapped_statement_is_read_as_one() {
        let src = "\
            let sql = db.q(\n\
            \x20   \"SELECT a FROM memories \\\n\
            \x20    WHERE user_id = ? AND space = ?\",\n\
            );\n";
        let found = sql_statements(src);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].1.contains("user_id"), "{}", found[0].1);
    }

    #[test]
    fn two_adjacent_statements_stay_separate() {
        let src = "\
            sqlx::query(&db.q(\"DELETE FROM plugins WHERE user_id = ?\"));\n\
            sqlx::query(&db.q(\"SELECT a FROM agents WHERE user_id = ?\"));\n";
        assert_eq!(sql_statements(src).len(), 2);
    }
}
