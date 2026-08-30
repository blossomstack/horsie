//! Storage for the user's inbox, sharing the config store's database.
//!
//! See the 0045 migration for why the table is shaped this way.

use crate::db::Db;
use crate::projects::ProjectId;
use horsie_models::inbox::InboxState;
use sqlx::Row;
use sqlx::any::AnyRow;

/// The tool result a declined question is answered with.
///
/// A real answer, not a synthetic error: declining resumes the agent through
/// the same path an ordinary answer takes, so what the model receives has to be
/// something it can act on. "Nobody answered" is actionable — it says to
/// proceed on the agent's own judgement — where an error would read as a fault
/// in the tool and invite a retry.
pub const AGENT_DECLINED_ASK: &str = "The user declined to answer this question. Continue without an answer, using your own \
     judgement, and say what you assumed.";

const COLS: &str = "id, kind, state, session_id, agent_id, title, body, payload, \
                    tool_call_id, created_at, read_at";

/// The `kind` column's two values. Written rather than inferred from which
/// columns are null, so a third kind can arrive without every reader
/// re-guessing.
const KIND_NOTICE: &str = "notice";
const KIND_ASK: &str = "ask";

/// A notice an agent wrote, before it has an id or a timestamp.
///
/// The tool supplies these few things and nothing else; everything else about
/// the row is the store's own business.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoticeRow {
    pub session_id: String,
    pub agent_id: String,
    pub title: String,
    pub body: String,
}

/// One question an agent is parked on, as the session actor derived it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AskRow {
    pub session_id: String,
    pub agent_id: String,
    pub question: String,
    pub choices: Vec<String>,
    pub multiple: bool,
    /// The parked `tool_use`. Identity: re-asserting the same one is a no-op.
    pub tool_call_id: String,
}

/// One message as the table holds it.
///
/// Deliberately not the wire type. `InboxMessageView` is an adjacently-tagged
/// union with the kind-specific half already unpacked; this is a row, with a
/// `kind` string and a JSON remainder. Keeping them apart is what stops the
/// wire shape needing a migration every time it is rearranged (CLAUDE.md:
/// protocol types are not storage types).
#[derive(Clone, Debug, PartialEq)]
pub struct InboxRow {
    pub id: String,
    pub kind: String,
    pub state: InboxState,
    pub session_id: String,
    pub agent_id: String,
    pub title: String,
    /// A notice's markdown, or an ask's question.
    pub body: String,
    /// An ask's `choices`; empty for every other kind.
    pub choices: Vec<String>,
    /// An ask's `multiple`; false for every other kind.
    pub multiple: bool,
    pub tool_call_id: Option<String>,
    pub created_at: i64,
    pub read_at: Option<i64>,
}

impl InboxRow {
    /// Whether this is a question rather than something merely said.
    #[must_use]
    pub fn is_ask(&self) -> bool {
        self.kind == KIND_ASK
    }

    /// Whether this row can still be acted on — and, for an ask, whether an
    /// agent is still stopped on it.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.state == InboxState::Open
    }
}

/// Which slice of the inbox to list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum InboxStateFilter {
    /// Everything, newest first. History included.
    #[default]
    All,
    /// Only rows nothing has been done with.
    Open,
    /// Only rows never opened.
    Unread,
}

/// What to narrow a listing by.
#[derive(Clone, Debug)]
pub struct InboxFilter {
    pub state: InboxStateFilter,
    pub limit: usize,
    pub offset: usize,
}

impl Default for InboxFilter {
    fn default() -> Self {
        Self {
            state: InboxStateFilter::All,
            limit: 100,
            offset: 0,
        }
    }
}

/// What a listing answers with: the page, and the two counts a badge needs.
///
/// The counts come back with the page rather than from a second route because
/// they are counts over the *whole* inbox: a badge derived from the page would
/// under-report the moment the list was paginated.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct InboxPage {
    pub messages: Vec<InboxRow>,
    pub unread: u32,
    pub open_asks: u32,
}

pub struct UserInboxStore {
    db: Db,
    /// Bound once, here, rather than passed per call.
    project: ProjectId,
}

impl UserInboxStore {
    pub fn new(db: Db, project: ProjectId) -> Self {
        Self { db, project }
    }

    /// Put a notice in the inbox.
    ///
    /// Unconditional: a notice has no identity beyond being said, so two
    /// identical ones are two things the agent said and both belong here.
    pub async fn record_notice(&self, notice: &NoticeRow, at_ms: i64) -> Result<String, String> {
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query(&self.db.q(&format!(
            "INSERT INTO inbox_messages (project_id, {COLS}, resolved_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
        )))
        .bind(self.project.as_str())
        .bind(&id)
        .bind(KIND_NOTICE)
        .bind(state_str(&InboxState::Open))
        .bind(&notice.session_id)
        .bind(&notice.agent_id)
        .bind(&notice.title)
        .bind(&notice.body)
        .bind("{}")
        .bind(Option::<String>::None)
        .bind(at_ms)
        .bind(Option::<i64>::None)
        .bind(Option::<i64>::None)
        .execute(self.db.pool())
        .await
        .map_err(|e| format!("record notice for {}: {e}", notice.session_id))?;
        Ok(id)
    }

    /// Put questions in the inbox, skipping any already there.
    ///
    /// Idempotent by `tool_call_id`, which is what lets the session actor
    /// re-assert its whole pending set without checking first — at load, after
    /// a crash, or simply because a batch of events was replayed.
    ///
    /// Read-then-insert rather than an upsert on the partial unique index:
    /// inferring a *partial* index in `ON CONFLICT` needs the predicate
    /// repeated, and the two dialects disagree about when they will accept it.
    /// Inside `begin_write` the read and the insert are one write transaction,
    /// so there is no window for the race an upsert would be closing.
    pub async fn record_asks(&self, asks: &[AskRow], at_ms: i64) -> Result<(), String> {
        if asks.is_empty() {
            return Ok(());
        }
        let mut tx = self.db.begin_write().await.map_err(|e| e.to_string())?;
        for ask in asks {
            let existing: Option<String> = sqlx::query_scalar(&self.db.q(
                "SELECT id FROM inbox_messages WHERE project_id = ? AND session_id = ? \
                 AND agent_id = ? AND tool_call_id = ?",
            ))
            .bind(self.project.as_str())
            .bind(&ask.session_id)
            .bind(&ask.agent_id)
            .bind(&ask.tool_call_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
            if existing.is_some() {
                continue;
            }
            let payload = serde_json::json!({
                "choices": ask.choices,
                "multiple": ask.multiple,
            })
            .to_string();
            sqlx::query(&self.db.q(&format!(
                "INSERT INTO inbox_messages (project_id, {COLS}, resolved_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
            )))
            .bind(self.project.as_str())
            .bind(uuid::Uuid::new_v4().to_string())
            .bind(KIND_ASK)
            .bind(state_str(&InboxState::Open))
            .bind(&ask.session_id)
            .bind(&ask.agent_id)
            .bind(title_of(&ask.question))
            .bind(&ask.question)
            .bind(payload)
            .bind(&ask.tool_call_id)
            .bind(at_ms)
            .bind(Option::<i64>::None)
            .bind(Option::<i64>::None)
            .execute(&mut *tx)
            .await
            .map_err(|e| format!("record ask {}: {e}", ask.tool_call_id))?;
        }
        tx.commit().await.map_err(|e| e.to_string())
    }

    /// Settle one agent's open asks, naming the calls that were answered.
    ///
    /// The whole of resolution, in one statement pair, because the two halves
    /// have to agree: a call in `answered` reaches `state`, and every *other*
    /// open ask of the same agent reaches `Closed`. An answer must cover an
    /// agent's pending set exactly (see `AnswerError::Incomplete`), so anything
    /// left over is a question that never got one.
    ///
    /// The blanket close only touches rows still `Open`, so it can never
    /// downgrade an `Answered` or a `Declined`, and is safe to run on every
    /// persist. The named calls may also settle a row already `Closed`, and
    /// that asymmetry is load-bearing: the two writers race. The projection
    /// sees the agent resume and closes, the answer handler records what the
    /// answer *was*, and either can land first. `Closed` means "settled, reason
    /// unknown", so learning the reason afterwards is better information rather
    /// than a rewrite of history — without this, a question answered in the
    /// session page could read "closed" purely because the agent was quick.
    pub async fn settle_agent_asks(
        &self,
        session_id: &str,
        agent_id: &str,
        answered: &[String],
        state: InboxState,
        at_ms: i64,
    ) -> Result<(), String> {
        let mut tx = self.db.begin_write().await.map_err(|e| e.to_string())?;
        for tool_call_id in answered {
            sqlx::query(
                &self
                    .db
                    .q("UPDATE inbox_messages SET state = ?, resolved_at = ? \
                 WHERE project_id = ? AND session_id = ? AND agent_id = ? \
                 AND tool_call_id = ? AND state IN (?, ?)"),
            )
            .bind(state_str(&state))
            .bind(at_ms)
            .bind(self.project.as_str())
            .bind(session_id)
            .bind(agent_id)
            .bind(tool_call_id)
            .bind(state_str(&InboxState::Open))
            .bind(state_str(&InboxState::Closed))
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
        }
        sqlx::query(
            &self
                .db
                .q("UPDATE inbox_messages SET state = ?, resolved_at = ? \
             WHERE project_id = ? AND session_id = ? AND agent_id = ? \
             AND tool_call_id IS NOT NULL AND state = ?"),
        )
        .bind(state_str(&InboxState::Closed))
        .bind(at_ms)
        .bind(self.project.as_str())
        .bind(session_id)
        .bind(agent_id)
        .bind(state_str(&InboxState::Open))
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
        tx.commit().await.map_err(|e| e.to_string())
    }

    /// Settle one message that is not an ask.
    pub async fn set_state(&self, id: &str, state: InboxState, at_ms: i64) -> Result<(), String> {
        sqlx::query(&self.db.q(
            "UPDATE inbox_messages SET state = ?, resolved_at = ? WHERE project_id = ? AND id = ?",
        ))
        .bind(state_str(&state))
        .bind(at_ms)
        .bind(self.project.as_str())
        .bind(id)
        .execute(self.db.pool())
        .await
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Note that these have been opened. Already-read rows keep their first
    /// stamp: "when did I first see this" does not change on a second look.
    pub async fn mark_read(&self, ids: &[String], at_ms: i64) -> Result<(), String> {
        if ids.is_empty() {
            return Ok(());
        }
        let mut tx = self.db.begin_write().await.map_err(|e| e.to_string())?;
        for id in ids {
            sqlx::query(&self.db.q("UPDATE inbox_messages SET read_at = ? \
                 WHERE project_id = ? AND id = ? AND read_at IS NULL"))
            .bind(at_ms)
            .bind(self.project.as_str())
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
        }
        tx.commit().await.map_err(|e| e.to_string())
    }

    /// Fetch messages by id, in no particular order.
    ///
    /// What a mutation reads first: deleting or replying has to know each row's
    /// kind and whether it is still open before it can decide what that means.
    pub async fn get_many(&self, ids: &[String]) -> Result<Vec<InboxRow>, String> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = vec!["?"; ids.len()].join(", ");
        let mut query = sqlx::query(&self.db.q(&format!(
            "SELECT {COLS} FROM inbox_messages WHERE project_id = ? AND id IN ({placeholders})"
        )))
        .bind(self.project.as_str());
        for id in ids {
            query = query.bind(id);
        }
        let rows = query
            .fetch_all(self.db.pool())
            .await
            .map_err(|e| e.to_string())?;
        rows.iter().map(row_to_message).collect()
    }

    /// Remove messages for good.
    ///
    /// A real delete and not an archive: the inbox already keeps history, so
    /// the only reason to reach for this is to be rid of something. Whether
    /// deleting an *open ask* also declines it is not decided here — that is
    /// the caller's, because it needs a supervisor to say it to.
    pub async fn delete(&self, ids: &[String]) -> Result<(), String> {
        if ids.is_empty() {
            return Ok(());
        }
        let placeholders = vec!["?"; ids.len()].join(", ");
        let mut query = sqlx::query(&self.db.q(&format!(
            "DELETE FROM inbox_messages WHERE project_id = ? AND id IN ({placeholders})"
        )))
        .bind(self.project.as_str());
        for id in ids {
            query = query.bind(id);
        }
        query
            .execute(self.db.pool())
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Close every question in this session that no longer holds an agent.
    ///
    /// Runs once at session load. It covers the direction the incremental
    /// writes cannot: a row left `Open` against an agent that moved on while
    /// nothing was watching, because the process died between the answer and
    /// the write.
    ///
    /// `awaiting` is the session's own list of agents parked right now, so this
    /// asks nothing of the agents themselves. That is deliberate rather than
    /// lazy: resolving an agent in order to read its questions *spawns* it, so
    /// a reconcile that read them would wake every parked subagent and sub
    /// session on every load — and a session that cannot go quiet is a session
    /// that never offloads.
    ///
    /// The cost is the other direction: a row lost to a crash between the park
    /// and the write does not come back, so that question is answerable only in
    /// its session. That is where it was answerable before there was an inbox,
    /// and it is a far smaller price than pinning every parked agent resident.
    ///
    /// Notices are untouched. They are derived from nothing, so there is
    /// nothing here to make them agree with.
    pub async fn reconcile_session(
        &self,
        session_id: &str,
        awaiting: &[String],
        at_ms: i64,
    ) -> Result<(), String> {
        let mut sql = String::from(
            "UPDATE inbox_messages SET state = ?, resolved_at = ? \
             WHERE project_id = ? AND session_id = ? AND tool_call_id IS NOT NULL AND state = ?",
        );
        if !awaiting.is_empty() {
            let placeholders = vec!["?"; awaiting.len()].join(", ");
            sql.push_str(&format!(" AND agent_id NOT IN ({placeholders})"));
        }
        let mut query = sqlx::query(&self.db.q(&sql))
            .bind(state_str(&InboxState::Closed))
            .bind(at_ms)
            .bind(self.project.as_str())
            .bind(session_id)
            .bind(state_str(&InboxState::Open));
        for agent in awaiting {
            query = query.bind(agent);
        }
        query
            .execute(self.db.pool())
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Drop a whole session's messages, because the session is going.
    ///
    /// A message pointing at a transcript that no longer exists is worse than
    /// no message: every "open the session" on it is a dead end, and an ask
    /// among them offers to resume an agent that is gone.
    pub async fn forget_session(&self, session_id: &str) -> Result<(), String> {
        sqlx::query(
            &self
                .db
                .q("DELETE FROM inbox_messages WHERE project_id = ? AND session_id = ?"),
        )
        .bind(self.project.as_str())
        .bind(session_id)
        .execute(self.db.pool())
        .await
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// A page of the inbox, newest first, with the counts a badge needs.
    ///
    /// Ordered by `created_at` and then by `id`, because several asks from one
    /// turn share a millisecond: without the tiebreak two pages of one listing
    /// can interleave and a reader paging through misses rows it never saw.
    pub async fn list(&self, filter: &InboxFilter) -> Result<InboxPage, String> {
        let mut sql = format!("SELECT {COLS} FROM inbox_messages WHERE project_id = ?");
        match filter.state {
            InboxStateFilter::All => {}
            InboxStateFilter::Open => sql.push_str(" AND state = 'open'"),
            InboxStateFilter::Unread => sql.push_str(" AND read_at IS NULL"),
        }
        sql.push_str(" ORDER BY created_at DESC, id LIMIT ? OFFSET ?");
        let rows = sqlx::query(&self.db.q(&sql))
            .bind(self.project.as_str())
            .bind(i64::try_from(filter.limit).unwrap_or(i64::MAX))
            .bind(i64::try_from(filter.offset).unwrap_or(0))
            .fetch_all(self.db.pool())
            .await
            .map_err(|e| e.to_string())?;
        let messages = rows
            .iter()
            .map(row_to_message)
            .collect::<Result<Vec<_>, _>>()?;

        let unread: i64 = sqlx::query_scalar(
            &self
                .db
                .q("SELECT COUNT(*) FROM inbox_messages WHERE project_id = ? AND read_at IS NULL"),
        )
        .bind(self.project.as_str())
        .fetch_one(self.db.pool())
        .await
        .map_err(|e| e.to_string())?;
        let open_asks: i64 = sqlx::query_scalar(&self.db.q(
            "SELECT COUNT(*) FROM inbox_messages WHERE project_id = ? AND kind = 'ask' \
             AND state = 'open'",
        ))
        .bind(self.project.as_str())
        .fetch_one(self.db.pool())
        .await
        .map_err(|e| e.to_string())?;

        Ok(InboxPage {
            messages,
            unread: u32::try_from(unread).unwrap_or(u32::MAX),
            open_asks: u32::try_from(open_asks).unwrap_or(u32::MAX),
        })
    }
}

/// Now, as the `*_at` columns hold it.
///
/// The columns are signed because SQLite's INTEGER and Postgres's BIGINT both
/// are; `now_ms` is unsigned. One conversion, here, rather than at each of the
/// half-dozen call sites that would each pick their own saturation.
#[must_use]
pub fn now_ms_i64() -> i64 {
    i64::try_from(horsie_models::now_ms()).unwrap_or(i64::MAX)
}

/// A one-line label for a question, for the list row.
///
/// Derived rather than asked for: `ask_user` takes a question and nothing else,
/// and inventing a second field on it so the inbox could have a heading would
/// be the inbox leaking into a tool that predates it.
fn title_of(question: &str) -> String {
    const MAX: usize = 80;
    let line = question.lines().next().unwrap_or(question).trim();
    if line.chars().count() <= MAX {
        return line.to_string();
    }
    // Truncate on a character boundary, not a byte one.
    let head: String = line.chars().take(MAX).collect();
    format!("{}…", head.trim_end())
}

/// The `state` column's spelling. One function, so the writes and the two
/// literal `state = 'open'` filters in `list` cannot drift apart — and the
/// match is exhaustive, so a fifth state fails to compile here.
fn state_str(state: &InboxState) -> &'static str {
    match state {
        InboxState::Open => "open",
        InboxState::Answered => "answered",
        InboxState::Declined => "declined",
        InboxState::Closed => "closed",
    }
}

/// An unknown `state` reads as `Closed` rather than failing the row.
///
/// The honest fallback for "settled, in some way this build does not know
/// about": it is the one state that promises nothing beyond "not waiting on
/// you", so a row written by a newer server degrades to inert rather than
/// offering an answer box that would go nowhere.
fn state_from(raw: &str) -> InboxState {
    match raw {
        "open" => InboxState::Open,
        "answered" => InboxState::Answered,
        "declined" => InboxState::Declined,
        _ => InboxState::Closed,
    }
}

fn row_to_message(row: &AnyRow) -> Result<InboxRow, String> {
    let payload: String = row.try_get("payload").map_err(|e| e.to_string())?;
    // A payload that will not parse costs the row its choices, never the row
    // itself: the question is in its own column and is the part that matters.
    let payload: serde_json::Value =
        serde_json::from_str(&payload).unwrap_or(serde_json::Value::Null);
    let state: String = row.try_get("state").map_err(|e| e.to_string())?;
    Ok(InboxRow {
        id: row.try_get("id").map_err(|e| e.to_string())?,
        kind: row.try_get("kind").map_err(|e| e.to_string())?,
        state: state_from(&state),
        session_id: row.try_get("session_id").map_err(|e| e.to_string())?,
        agent_id: row.try_get("agent_id").map_err(|e| e.to_string())?,
        title: row.try_get("title").map_err(|e| e.to_string())?,
        body: row.try_get("body").map_err(|e| e.to_string())?,
        choices: payload
            .get("choices")
            .and_then(serde_json::Value::as_array)
            .map(|cs| {
                cs.iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        multiple: payload
            .get("multiple")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        tool_call_id: row.try_get("tool_call_id").map_err(|e| e.to_string())?,
        created_at: row.try_get("created_at").map_err(|e| e.to_string())?,
        read_at: row.try_get("read_at").map_err(|e| e.to_string())?,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    async fn store() -> UserInboxStore {
        UserInboxStore::new(
            crate::db::testing::db().await,
            crate::projects::ProjectId::new("1"),
        )
    }

    fn ask(tool_call_id: &str, question: &str) -> AskRow {
        AskRow {
            session_id: "s1".into(),
            agent_id: "main".into(),
            question: question.into(),
            choices: vec!["yes".into(), "no".into()],
            multiple: false,
            tool_call_id: tool_call_id.into(),
        }
    }

    async fn one(store: &UserInboxStore) -> InboxRow {
        let page = store.list(&InboxFilter::default()).await.unwrap();
        let [row] = page.messages.as_slice() else {
            panic!("exactly one message: {:?}", page.messages);
        };
        row.clone()
    }

    /// The session actor re-asserts its whole pending set on every load and
    /// after every replayed batch. If that duplicated, one question would grow
    /// a row per restart and the inbox would fill with copies of itself.
    #[tokio::test]
    async fn re_asserting_the_same_question_writes_nothing_twice() {
        let store = store().await;
        store
            .record_asks(&[ask("tc-1", "which db?")], 100)
            .await
            .unwrap();
        store
            .record_asks(&[ask("tc-1", "which db?")], 200)
            .await
            .unwrap();

        let page = store.list(&InboxFilter::default()).await.unwrap();
        assert_eq!(page.messages.len(), 1, "one call id, one row");
        assert_eq!(
            page.messages[0].created_at, 100,
            "the first sighting is when it was asked; a re-assert is not a new question"
        );
    }

    #[tokio::test]
    async fn an_ask_carries_its_choices_back_out() {
        let store = store().await;
        store
            .record_asks(&[ask("tc-1", "which db?")], 100)
            .await
            .unwrap();
        let row = one(&store).await;
        assert!(row.is_ask());
        assert!(row.is_open());
        assert_eq!(row.choices, vec!["yes".to_string(), "no".to_string()]);
        assert_eq!(row.tool_call_id.as_deref(), Some("tc-1"));
    }

    /// The two writers race: the projection notices the agent resumed and
    /// closes, the answer handler records what the answer was. Whichever lands
    /// second must leave the row reading `Answered` — a question the person
    /// answered in the session page must never show as merely closed because
    /// the agent happened to resume quickly.
    #[tokio::test]
    async fn an_answer_landing_after_the_close_still_reads_answered() {
        let store = store().await;
        store
            .record_asks(&[ask("tc-1", "which db?")], 100)
            .await
            .unwrap();

        // The projection gets there first: the agent left AwaitingInput.
        store
            .settle_agent_asks("s1", "main", &[], InboxState::Closed, 200)
            .await
            .unwrap();
        assert_eq!(one(&store).await.state, InboxState::Closed);

        // Then the answer handler reports what the answer actually was.
        store
            .settle_agent_asks("s1", "main", &["tc-1".into()], InboxState::Answered, 300)
            .await
            .unwrap();
        assert_eq!(
            one(&store).await.state,
            InboxState::Answered,
            "learning the reason afterwards is better information, not a rewrite"
        );
    }

    /// The reverse order, which is the ordinary one. The blanket close runs on
    /// every persist, so if it could downgrade, every answered question would
    /// flip to `Closed` a moment later.
    #[tokio::test]
    async fn a_later_close_never_downgrades_an_answer() {
        let store = store().await;
        store
            .record_asks(&[ask("tc-1", "which db?")], 100)
            .await
            .unwrap();
        store
            .settle_agent_asks("s1", "main", &["tc-1".into()], InboxState::Answered, 200)
            .await
            .unwrap();
        store
            .settle_agent_asks("s1", "main", &[], InboxState::Closed, 300)
            .await
            .unwrap();
        assert_eq!(one(&store).await.state, InboxState::Answered);
    }

    /// A person who types a new message instead of answering. The agent is
    /// resumed with a "not answered" result and moves on, and nothing names an
    /// outcome for the question — so the row must stop being open, or the inbox
    /// keeps offering to answer something that no longer holds anything.
    #[tokio::test]
    async fn an_unanswered_question_closes_when_its_agent_moves_on() {
        let store = store().await;
        store
            .record_asks(
                &[ask("tc-1", "which db?"), ask("tc-2", "which branch?")],
                100,
            )
            .await
            .unwrap();
        store
            .settle_agent_asks("s1", "main", &["tc-1".into()], InboxState::Answered, 200)
            .await
            .unwrap();

        let page = store.list(&InboxFilter::default()).await.unwrap();
        let by_call = |id: &str| {
            page.messages
                .iter()
                .find(|m| m.tool_call_id.as_deref() == Some(id))
                .cloned()
                .expect("row is there")
        };
        assert_eq!(by_call("tc-1").state, InboxState::Answered);
        assert_eq!(
            by_call("tc-2").state,
            InboxState::Closed,
            "an answer covers an agent's pending set exactly, so anything left over never got one"
        );
        assert_eq!(page.open_asks, 0, "no agent is stopped any more");
    }

    /// A notice is not an ask and must never be settled by the ask sweep — it
    /// is not derived from anything, so nothing is entitled to close it on the
    /// person's behalf.
    #[tokio::test]
    async fn settling_asks_leaves_notices_alone() {
        let store = store().await;
        store
            .record_notice(
                &NoticeRow {
                    session_id: "s1".into(),
                    agent_id: "main".into(),
                    title: "done".into(),
                    body: "the migration finished".into(),
                },
                100,
            )
            .await
            .unwrap();
        store
            .settle_agent_asks("s1", "main", &[], InboxState::Closed, 200)
            .await
            .unwrap();
        assert_eq!(one(&store).await.state, InboxState::Open);
    }

    /// The load-time repair: a row whose agent has stopped waiting is closed,
    /// and a row whose agent is still parked is left answerable.
    ///
    /// Both halves matter. Closing everything would silently swallow live
    /// questions on every session load; closing nothing would leave a row
    /// stranded `Open` whenever a process died between the answer and the
    /// write, which is the case this exists for.
    #[tokio::test]
    async fn reconcile_closes_only_the_agents_that_stopped_waiting() {
        let store = store().await;
        let mut still_parked = ask("tc-live", "fresh?");
        still_parked.agent_id = "agent-2".into();
        store
            .record_asks(&[ask("tc-old", "stale?"), still_parked], 100)
            .await
            .unwrap();

        // Only `agent-2` is still parked; `main` moved on unobserved.
        store
            .reconcile_session("s1", &["agent-2".to_string()], 200)
            .await
            .unwrap();

        let page = store.list(&InboxFilter::default()).await.unwrap();
        let state_of = |id: &str| {
            page.messages
                .iter()
                .find(|m| m.tool_call_id.as_deref() == Some(id))
                .map(|m| m.state.clone())
        };
        assert_eq!(state_of("tc-old"), Some(InboxState::Closed));
        assert_eq!(
            state_of("tc-live"),
            Some(InboxState::Open),
            "an agent that is still parked still has a question to answer"
        );
    }

    /// A session with nothing parked closes everything still open — the plain
    /// case, and the one an empty list must not be read as "leave it all".
    #[tokio::test]
    async fn reconcile_with_nothing_parked_closes_every_open_question() {
        let store = store().await;
        store
            .record_asks(&[ask("tc-1", "stale?")], 100)
            .await
            .unwrap();
        store.reconcile_session("s1", &[], 200).await.unwrap();
        assert_eq!(one(&store).await.state, InboxState::Closed);
    }

    /// Reconcile runs on every load, including loads where nothing was lost.
    /// If it re-opened settled rows, every session load would resurrect history
    /// the person had already dealt with.
    #[tokio::test]
    async fn reconcile_does_not_reopen_history() {
        let store = store().await;
        store
            .record_asks(&[ask("tc-1", "which db?")], 100)
            .await
            .unwrap();
        store
            .settle_agent_asks("s1", "main", &["tc-1".into()], InboxState::Answered, 200)
            .await
            .unwrap();

        store.reconcile_session("s1", &[], 300).await.unwrap();

        assert_eq!(one(&store).await.state, InboxState::Answered);
    }

    #[tokio::test]
    async fn reading_is_recorded_once_and_drives_the_unread_count() {
        let store = store().await;
        store
            .record_asks(&[ask("tc-1", "which db?")], 100)
            .await
            .unwrap();
        let id = one(&store).await.id;

        assert_eq!(store.list(&InboxFilter::default()).await.unwrap().unread, 1);
        store
            .mark_read(std::slice::from_ref(&id), 500)
            .await
            .unwrap();
        store.mark_read(&[id], 900).await.unwrap();

        let page = store.list(&InboxFilter::default()).await.unwrap();
        assert_eq!(page.unread, 0);
        assert_eq!(
            page.messages[0].read_at,
            Some(500),
            "when it was *first* seen does not change on a second look"
        );
    }

    #[tokio::test]
    async fn the_filters_narrow_to_what_they_name() {
        let store = store().await;
        store
            .record_asks(&[ask("tc-1", "open one"), ask("tc-2", "settled one")], 100)
            .await
            .unwrap();
        let settled = store
            .list(&InboxFilter::default())
            .await
            .unwrap()
            .messages
            .iter()
            .find(|m| m.tool_call_id.as_deref() == Some("tc-2"))
            .expect("row is there")
            .id
            .clone();
        store
            .set_state(&settled, InboxState::Closed, 200)
            .await
            .unwrap();
        store.mark_read(&[settled], 200).await.unwrap();

        let open = store
            .list(&InboxFilter {
                state: InboxStateFilter::Open,
                ..InboxFilter::default()
            })
            .await
            .unwrap();
        assert_eq!(open.messages.len(), 1);
        assert_eq!(open.messages[0].tool_call_id.as_deref(), Some("tc-1"));

        let unread = store
            .list(&InboxFilter {
                state: InboxStateFilter::Unread,
                ..InboxFilter::default()
            })
            .await
            .unwrap();
        assert_eq!(unread.messages.len(), 1);
        assert_eq!(unread.messages[0].tool_call_id.as_deref(), Some("tc-1"));
    }

    /// A message that outlives its session is a dead link, and an ask among
    /// them offers to resume an agent that no longer exists.
    #[tokio::test]
    async fn deleting_a_session_takes_its_messages() {
        let store = store().await;
        store
            .record_asks(&[ask("tc-1", "which db?")], 100)
            .await
            .unwrap();
        store
            .record_notice(
                &NoticeRow {
                    session_id: "s2".into(),
                    agent_id: "main".into(),
                    title: "elsewhere".into(),
                    body: "from another session".into(),
                },
                100,
            )
            .await
            .unwrap();

        store.forget_session("s1").await.unwrap();

        let page = store.list(&InboxFilter::default()).await.unwrap();
        assert_eq!(page.messages.len(), 1);
        assert_eq!(page.messages[0].session_id, "s2");
    }

    /// The counts ride with the page because they are counts over the whole
    /// inbox: derived from the page, a badge would under-report the moment the
    /// list was paginated.
    #[tokio::test]
    async fn the_counts_span_the_whole_inbox_not_the_page() {
        let store = store().await;
        for n in 0..5 {
            store
                .record_asks(&[ask(&format!("tc-{n}"), "which db?")], 100 + n)
                .await
                .unwrap();
        }
        let page = store
            .list(&InboxFilter {
                limit: 2,
                ..InboxFilter::default()
            })
            .await
            .unwrap();
        assert_eq!(page.messages.len(), 2, "the page is what was asked for");
        assert_eq!(page.unread, 5);
        assert_eq!(page.open_asks, 5);
    }
}
