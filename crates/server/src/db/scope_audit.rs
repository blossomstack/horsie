//! A test that reads this crate's own source and fails any SQL that touches a
//! scoped table without naming `user_id`.
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
//! What it does *not* catch: a statement that names `user_id` but forgets to
//! `.bind()` it. That is the isolation harness's job
//! (`tests/tests/user_isolation.rs`), and it has caught one already.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

/// Every table with a `user_id` column, per `0024_user_scoping.sql`.
const SCOPED: &[&str] = &[
    "providers",
    "models",
    "settings",
    "mcp_servers",
    "plugins",
    "memory_spaces",
    "agents",
    "routines",
    "environments",
    "workflows",
    "provider_oauth",
    "marketplaces",
    "model_cards",
    "memories",
    "github_credentials",
    "journal_logs",
];

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
        "SELECT user_id, {COLS} FROM routines",
        "one timer serves the deployment; each routine is then armed and run as \
         whoever owns it, and the row carries its owner for exactly that",
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
            if statement.contains("user_id") {
                continue;
            }
            if let Some(table) = SCOPED.iter().find(|t| mentions_table(&statement, t)) {
                offences.push(format!(
                    "{}:{line_no}: touches `{table}` without `user_id`\n    {statement}",
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
