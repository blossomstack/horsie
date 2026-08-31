//! Storage for agent presets, sharing the config store's database.
//! List-typed columns are JSON.

use crate::db::Db;
use crate::mcp::selection::de_selections;
use crate::projects::ProjectId;
use crate::revisions::{EntityKind, RevisionStore};
use horsie_models::mcp::McpServerSelection;
use sqlx::Row;
use sqlx::any::AnyRow;

const COLS: &str = "name, description, instructions, model, plugins, \
                    mcp_servers, memory_spaces, thinking_effort, auto_compact, allowed_tools, tunable, revision, created_at, updated_at";

/// One row of the `agents` table.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentRow {
    pub name: String,
    pub description: String,
    /// Standing instructions the preset's agent runs under. `None` and empty
    /// mean the same thing — no section is added to the system prompt.
    pub instructions: Option<String>,
    pub model: String,
    pub plugins: Vec<String>,
    /// The MCP servers this preset selects, and how much of each.
    pub mcp_servers: Vec<McpServerSelection>,
    pub memory_spaces: Vec<String>,
    pub thinking_effort: Option<String>,
    /// `None` means yes — see the 0034 migration.
    pub auto_compact: Option<bool>,
    /// The tools sessions from this preset may call. `None` (a NULL column, not
    /// an empty array) means the default set, and is a different value from
    /// `Some(vec![])` — which means no built-in tools at all.
    pub allowed_tools: Option<Vec<String>>,
    /// Whether this preset opts in to being tuned from its own runs. `None`
    /// means no — see the 0042 migration for why this default is the opposite
    /// of `auto_compact`'s.
    pub tunable: Option<bool>,
    /// Which version of this preset this row is. `None` on a row that predates
    /// versioning and has not been written since — see the 0044 migration.
    ///
    /// Set by the store on every write, never by a caller: it is the store's
    /// own record of what it did, and a caller that could choose it could make
    /// two different presets claim the same version.
    pub revision: Option<i64>,
    pub created_at: String,
    pub updated_at: String,
}

pub struct AgentStore {
    db: Db,
    /// Bound once, here, rather than passed per call.
    user: ProjectId,
    /// Built here rather than injected: every write below has to append a
    /// revision in its own transaction, so a store that could be constructed
    /// without one would have a way to write history-free.
    revisions: RevisionStore,
}

impl AgentStore {
    pub fn new(db: Db, user: ProjectId) -> Self {
        Self {
            revisions: RevisionStore::new(db.clone(), user.clone()),
            db,
            user,
        }
    }

    /// This store's history, for the read-only side.
    #[must_use]
    pub fn revisions(&self) -> &RevisionStore {
        &self.revisions
    }

    pub async fn list(&self) -> Result<Vec<AgentRow>, String> {
        let rows = sqlx::query(&self.db.q(&format!(
            "SELECT {COLS} FROM agents WHERE project_id = ? ORDER BY name"
        )))
        .bind(self.user.as_str())
        .fetch_all(self.db.pool())
        .await
        .map_err(|e| e.to_string())?;
        rows.iter().map(row_to_agent).collect()
    }

    pub async fn get(&self, name: &str) -> Result<Option<AgentRow>, String> {
        let row = sqlx::query(&self.db.q(&format!(
            "SELECT {COLS} FROM agents WHERE project_id = ? AND name = ?"
        )))
        .bind(self.user.as_str())
        .bind(name)
        .fetch_optional(self.db.pool())
        .await
        .map_err(|e| e.to_string())?;
        row.as_ref().map(row_to_agent).transpose()
    }

    /// Insert, recording revision 1. Errs when the name is taken (no upsert --
    /// a silent overwrite would discard the existing preset).
    ///
    /// `payload` is the JSON snapshot history keeps. Supplied by the caller
    /// rather than serialized here, because what a restore has to put back is
    /// the *wire* shape a caller understands, not this row.
    pub async fn insert(&self, row: &AgentRow, payload: &str) -> Result<i64, String> {
        let mut tx = self.db.begin_write().await.map_err(|e| e.to_string())?;
        let revision = self
            .revisions
            .append(
                &mut tx,
                EntityKind::Agent,
                &row.name,
                payload,
                false,
                &row.updated_at,
            )
            .await?;
        sqlx::query(&self.db.q(&format!(
            "INSERT INTO agents (project_id, {COLS}) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
        )))
        .bind(self.user.as_str())
        .bind(&row.name)
        .bind(&row.description)
        .bind(&row.instructions)
        .bind(&row.model)
        .bind(to_json(&row.plugins)?)
        .bind(to_json(&row.mcp_servers)?)
        .bind(to_json(&row.memory_spaces)?)
        .bind(&row.thinking_effort)
        .bind(row.auto_compact.map(i64::from))
        .bind(opt_json(row.allowed_tools.as_ref())?)
        .bind(row.tunable.map(i64::from))
        .bind(revision)
        .bind(&row.created_at)
        .bind(&row.updated_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("create agent '{}': {e}", row.name))?;
        tx.commit().await.map_err(|e| e.to_string())?;
        Ok(revision)
    }

    /// Full replace, recording a revision. `None` when no agent has that name.
    pub async fn replace(&self, row: &AgentRow, payload: &str) -> Result<Option<i64>, String> {
        let mut tx = self.db.begin_write().await.map_err(|e| e.to_string())?;
        let revision = self
            .revisions
            .append(
                &mut tx,
                EntityKind::Agent,
                &row.name,
                payload,
                false,
                &row.updated_at,
            )
            .await?;
        let res = sqlx::query(&self.db.q(
            "UPDATE agents SET description = ?, instructions = ?, model = ?, \
             plugins = ?, mcp_servers = ?, memory_spaces = ?, thinking_effort = ?, \
             auto_compact = ?, allowed_tools = ?, tunable = ?, revision = ?, updated_at = ? \
             WHERE project_id = ? AND name = ?",
        ))
        .bind(&row.description)
        .bind(&row.instructions)
        .bind(&row.model)
        .bind(to_json(&row.plugins)?)
        .bind(to_json(&row.mcp_servers)?)
        .bind(to_json(&row.memory_spaces)?)
        .bind(&row.thinking_effort)
        .bind(row.auto_compact.map(i64::from))
        .bind(opt_json(row.allowed_tools.as_ref())?)
        .bind(row.tunable.map(i64::from))
        .bind(revision)
        .bind(&row.updated_at)
        .bind(self.user.as_str())
        .bind(&row.name)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
        if res.rows_affected() == 0 {
            // Nothing was replaced, so nothing happened — including the
            // revision this transaction was about to record.
            return Ok(None);
        }
        tx.commit().await.map_err(|e| e.to_string())?;
        Ok(Some(revision))
    }

    /// Delete, recording what was deleted as a revision of its own — so a
    /// preset a tuning agent removed can still be read back and restored.
    pub async fn delete(&self, name: &str, payload: &str, at: &str) -> Result<bool, String> {
        let mut tx = self.db.begin_write().await.map_err(|e| e.to_string())?;
        let res = sqlx::query(
            &self
                .db
                .q("DELETE FROM agents WHERE project_id = ? AND name = ?"),
        )
        .bind(self.user.as_str())
        .bind(name)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
        if res.rows_affected() == 0 {
            return Ok(false);
        }
        self.revisions
            .append(&mut tx, EntityKind::Agent, name, payload, true, at)
            .await?;
        tx.commit().await.map_err(|e| e.to_string())?;
        Ok(true)
    }
}

fn to_json<T: serde::Serialize>(v: &T) -> Result<String, String> {
    serde_json::to_string(v).map_err(|e| e.to_string())
}

/// A nullable JSON column. `None` stays NULL rather than becoming `"[]"`: for a
/// tool selection those are different answers — "whatever this horsie thinks is
/// sensible" versus "no built-in tools".
fn opt_json<T: serde::Serialize>(v: Option<&T>) -> Result<Option<String>, String> {
    v.map(|v| serde_json::to_string(v).map_err(|e| e.to_string()))
        .transpose()
}

fn from_json<T: serde::de::DeserializeOwned>(col: &str, text: String) -> Result<T, String> {
    serde_json::from_str(&text).map_err(|e| format!("agents.{col}: {e}"))
}

/// `agents.mcp_servers`, tolerating the list-of-names shape every row written
/// before tool selection still holds.
fn selections_from_json(text: String) -> Result<Vec<McpServerSelection>, String> {
    let mut de = serde_json::Deserializer::from_str(&text);
    de_selections(&mut de).map_err(|e| format!("agents.mcp_servers: {e}"))
}

fn row_to_agent(row: &AnyRow) -> Result<AgentRow, String> {
    let get = |c: &str| row.try_get::<String, _>(c).map_err(|e| e.to_string());
    let get_opt = |c: &str| {
        row.try_get::<Option<String>, _>(c)
            .map_err(|e| e.to_string())
    };
    Ok(AgentRow {
        name: get("name")?,
        description: get("description")?,
        instructions: get_opt("instructions")?,
        model: get("model")?,
        plugins: from_json("plugins", get("plugins")?)?,
        // Through the tolerant reader: rows written before tools could be
        // selected hold a list of bare names.
        mcp_servers: selections_from_json(get("mcp_servers")?)?,
        memory_spaces: from_json("memory_spaces", get("memory_spaces")?)?,
        thinking_effort: get_opt("thinking_effort")?,
        // `i64`, not `bool`: the Any driver has no mapping for SQLite's
        // BOOLEAN, so this column is an INTEGER like every other flag here.
        auto_compact: row
            .try_get::<Option<i64>, _>("auto_compact")
            .map_err(|e| e.to_string())?
            .map(|v| v != 0),
        allowed_tools: get_opt("allowed_tools")?
            .map(|t| from_json("allowed_tools", t))
            .transpose()?,
        tunable: row
            .try_get::<Option<i64>, _>("tunable")
            .map_err(|e| e.to_string())?
            .map(|v| v != 0),
        revision: row.try_get("revision").map_err(|e| e.to_string())?,
        created_at: get("created_at")?,
        updated_at: get("updated_at")?,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
pub(super) mod tests {
    use super::*;
    use horsie_models::mcp::McpServerSelection;

    pub(super) async fn store() -> (AgentStore, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let pool = crate::db::testing::db().await;
        (
            AgentStore::new(pool, crate::projects::ProjectId::new("1")),
            tmp,
        )
    }

    /// A preset saved before tools could be selected holds a plain list of
    /// names in its JSON column. It has to keep loading — there is no
    /// migration that can rewrite JSON in both dialects, and a preset that
    /// will not load is a preset nobody can invoke.
    #[tokio::test]
    async fn a_stored_list_of_names_loads_as_whole_servers() {
        let (s, _t) = store().await;
        s.insert(&row("legacy"), "{}").await.unwrap();
        sqlx::query(
            &s.db
                .q("UPDATE agents SET mcp_servers = ? WHERE project_id = ? AND name = ?"),
        )
        .bind(r#"["linear","github"]"#)
        .bind("1")
        .bind("legacy")
        .execute(s.db.pool())
        .await
        .unwrap();

        let loaded = s.get("legacy").await.unwrap().unwrap();
        assert_eq!(
            loaded
                .mcp_servers
                .iter()
                .map(|m| m.name.as_str())
                .collect::<Vec<_>>(),
            ["linear", "github"]
        );
        assert!(loaded.mcp_servers.iter().all(|m| m.tools.is_none()));
    }

    #[tokio::test]
    async fn a_narrowed_selection_round_trips_through_the_column() {
        let (s, _t) = store().await;
        let mut r = row("narrowed");
        r.mcp_servers = vec![
            McpServerSelection {
                name: "linear".into(),
                tools: Some(vec!["search_issues".into()]),
            },
            McpServerSelection {
                name: "github".into(),
                tools: None,
            },
        ];
        s.insert(&r, "{}").await.unwrap();
        let loaded = s.get("narrowed").await.unwrap().unwrap();
        assert_eq!(loaded.mcp_servers, r.mcp_servers);
    }

    pub(super) fn row(name: &str) -> AgentRow {
        AgentRow {
            name: name.into(),
            description: "d".into(),
            instructions: None,
            model: "sonnet".into(),
            plugins: vec!["superpowers".into()],
            mcp_servers: vec![],
            memory_spaces: vec!["default".into()],
            thinking_effort: Some("high".into()),
            created_at: "1".into(),
            updated_at: "1".into(),
            auto_compact: None,
            allowed_tools: None,
            tunable: None,
            revision: None,
        }
    }

    #[tokio::test]
    async fn insert_get_list_roundtrip_including_json_columns() {
        let (s, _t) = store().await;
        s.insert(&row("a"), "{}").await.unwrap();
        let got = s.get("a").await.unwrap().unwrap();
        assert_eq!(
            got,
            AgentRow {
                revision: Some(1),
                ..row("a")
            }
        );
        assert_eq!(s.list().await.unwrap().len(), 1);
        assert!(s.get("ghost").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn duplicate_insert_is_rejected() {
        let (s, _t) = store().await;
        s.insert(&row("a"), "{}").await.unwrap();
        assert!(s.insert(&row("a"), "{}").await.is_err());
    }

    #[tokio::test]
    async fn replace_updates_and_reports_misses() {
        let (s, _t) = store().await;
        assert!(s.replace(&row("ghost"), "{}").await.unwrap().is_none());
        s.insert(&row("a"), "{}").await.unwrap();
        let mut r = row("a");
        r.description = "new".into();
        r.updated_at = "2".into();
        assert!(s.replace(&r, "{}").await.unwrap().is_some());
        let got = s.get("a").await.unwrap().unwrap();
        assert_eq!(got.description, "new");
        assert_eq!(got.updated_at, "2");
        assert_eq!(got.created_at, "1", "replace must not touch created_at");
    }

    /// A flag stored one bind out of order writes a neighbouring column and
    /// still round-trips *something*, so this pins the value rather than just
    /// the absence of an error. Both writers, because they bind separately.
    #[tokio::test]
    async fn auto_compact_round_trips_through_insert_and_replace() {
        let (s, _t) = store().await;
        let mut r = row("a");
        r.auto_compact = Some(false);
        s.insert(&r, "{}").await.unwrap();
        let got = s.get("a").await.unwrap().unwrap();
        assert_eq!(got.auto_compact, Some(false));
        // The neighbours a mis-ordered bind would have landed in.
        assert_eq!(got.thinking_effort, r.thinking_effort);
        assert_eq!(got.updated_at, r.updated_at);

        r.auto_compact = Some(true);
        assert!(s.replace(&r, "{}").await.unwrap().is_some());
        assert_eq!(s.get("a").await.unwrap().unwrap().auto_compact, Some(true));

        // Absent stays absent, which is what "the server decides" looks like on
        // the way back out.
        r.auto_compact = None;
        assert!(s.replace(&r, "{}").await.unwrap().is_some());
        assert_eq!(s.get("a").await.unwrap().unwrap().auto_compact, None);
    }

    /// Same shape as the `auto_compact` test above, and for the same reason:
    /// `tunable` was appended to `COLS` between `allowed_tools` and
    /// `created_at`, so a bind left in the old position writes a timestamp into
    /// the flag and the flag into a timestamp — both of which still round-trip
    /// *something*.
    #[tokio::test]
    async fn tunable_round_trips_through_insert_and_replace() {
        let (s, _t) = store().await;
        let mut r = row("a");
        r.tunable = Some(true);
        s.insert(&r, "{}").await.unwrap();
        let got = s.get("a").await.unwrap().unwrap();
        assert_eq!(got.tunable, Some(true));
        // The neighbours a mis-ordered bind would have landed in.
        assert_eq!(got.allowed_tools, r.allowed_tools);
        assert_eq!(got.created_at, r.created_at);
        assert_eq!(got.updated_at, r.updated_at);

        r.tunable = Some(false);
        assert!(s.replace(&r, "{}").await.unwrap().is_some());
        assert_eq!(s.get("a").await.unwrap().unwrap().tunable, Some(false));

        r.tunable = None;
        assert!(s.replace(&r, "{}").await.unwrap().is_some());
        assert_eq!(s.get("a").await.unwrap().unwrap().tunable, None);
    }

    /// The default is the one that matters here: a preset that never mentioned
    /// tuning must not be tunable, or the migration silently opts every
    /// existing preset in to being rewritten by another agent.
    #[tokio::test]
    async fn a_preset_that_never_asked_is_not_tunable() {
        let (s, _t) = store().await;
        s.insert(&row("a"), "{}").await.unwrap();
        assert_eq!(s.get("a").await.unwrap().unwrap().tunable, None);
    }

    #[tokio::test]
    async fn delete_reports_misses() {
        let (s, _t) = store().await;
        s.insert(&row("a"), "{}").await.unwrap();
        assert!(s.delete("a", "{}", "1").await.unwrap());
        assert!(!s.delete("a", "{}", "1").await.unwrap());
    }

    #[tokio::test]
    async fn null_effort_round_trips() {
        let (s, _t) = store().await;
        let mut r = row("a");
        r.thinking_effort = None;
        s.insert(&r, "{}").await.unwrap();
        let got = s.get("a").await.unwrap().unwrap();
        assert_eq!(got.thinking_effort, None);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod revision_tests {
    use super::tests::*;
    use crate::revisions::EntityKind;

    /// Every write records what it wrote, and the head on the row is what the
    /// history says. Without the head, a caller has to read the history to
    /// find out what version it holds — which is a second query on the path
    /// that exists to save one.
    #[tokio::test]
    async fn writes_number_the_row_and_the_history_together() {
        let (s, _t) = store().await;
        assert_eq!(s.insert(&row("a"), "v1").await.unwrap(), 1);
        assert_eq!(s.get("a").await.unwrap().unwrap().revision, Some(1));

        let mut r = row("a");
        r.description = "second".into();
        assert_eq!(s.replace(&r, "v2").await.unwrap(), Some(2));
        assert_eq!(s.get("a").await.unwrap().unwrap().revision, Some(2));

        let history = s.revisions().list(EntityKind::Agent, "a").await.unwrap();
        assert_eq!(
            history
                .iter()
                .map(|h| h.payload.as_str())
                .collect::<Vec<_>>(),
            vec!["v2", "v1"],
            "newest first, and both saves are there"
        );
    }

    /// A replace that matched nothing must leave no trace. Appending the
    /// revision first and rolling back is the whole reason that write is a
    /// transaction — a history entry for a preset that was never touched is a
    /// version number nothing can be restored to.
    #[tokio::test]
    async fn a_replace_that_matched_nothing_records_no_revision() {
        let (s, _t) = store().await;
        assert!(s.replace(&row("ghost"), "v1").await.unwrap().is_none());
        assert!(
            s.revisions()
                .list(EntityKind::Agent, "ghost")
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// What was deleted stays readable. A tuning agent that removes a preset it
    /// decided was redundant has to be undoable, and "undo" needs the bytes.
    #[tokio::test]
    async fn a_delete_keeps_what_it_deleted() {
        let (s, _t) = store().await;
        s.insert(&row("a"), "the-preset").await.unwrap();
        assert!(s.delete("a", "the-preset", "2").await.unwrap());
        assert!(s.get("a").await.unwrap().is_none());

        let history = s.revisions().list(EntityKind::Agent, "a").await.unwrap();
        assert_eq!(history.len(), 2);
        assert!(history[0].deleted);
        assert_eq!(history[0].payload, "the-preset");
    }

    #[tokio::test]
    async fn deleting_nothing_records_nothing() {
        let (s, _t) = store().await;
        assert!(!s.delete("ghost", "{}", "1").await.unwrap());
        assert!(
            s.revisions()
                .list(EntityKind::Agent, "ghost")
                .await
                .unwrap()
                .is_empty()
        );
    }
}
