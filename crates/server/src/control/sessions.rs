//! The sessions resource: the running work, as opposed to the configuration
//! that starts it.
//!
//! Reading a transcript is here too, and it is the reason an ops agent is worth
//! having: diagnosing last night's failed routine means looking at what it
//! actually said. It is [`Expose::ToolOnly`] because the route it shares a path
//! with is a stream — see `read` below.

use crate::agent_loop::{Anchor, LogFilter};
use crate::control::{ControlError, Expose, Method, Operation, Resource, ask, op};
use crate::http::handlers;
use crate::projects::ProjectServices;
use crate::sessions::supervisor::RenameSessionError;
use crate::sessions::supervisor::SessionSupervisorCommand;
use horsie_models::agent::LogEntryKind;
use horsie_models::session::SessionSummary;
use horsie_models::session_api::{
    Ack, GetSessionResponse, ListSessionsResponse, LogSearchPage, MessagesPage,
    SessionEfficiencyReport,
};
use std::sync::Arc;

/// What a model gets by default, and the most it can ask for.
///
/// Far below the HTTP page's 50/1000. A browser scrolls a long transcript for
/// free; a model pays for every entry out of the context it still needs to do
/// the work, and it can always page back with `before`.
const TOOL_PAGE_DEFAULT: usize = 20;
const TOOL_PAGE_MAX: usize = 100;

/// Checked at compile time rather than in a test: the gap between what a model
/// may read and what a browser may is the whole protection here, and raising
/// either HTTP constant later must not be able to close it quietly.
const _: () = assert!(TOOL_PAGE_MAX < crate::http::messages::PAGE_MAX);
const _: () = assert!(TOOL_PAGE_DEFAULT < crate::http::messages::PAGE_DEFAULT);

/// The main agent, when a caller names no other.
const MAIN_AGENT: &str = "main";

#[derive(serde::Deserialize, schemars::JsonSchema, Default)]
pub struct ListSessions {
    /// Only runs of this workflow.
    pub workflow: Option<String>,
    /// Only runs of this routine. Absent excludes routine runs entirely, which
    /// is what the session list means by "sessions".
    pub routine: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct SessionRef {
    /// Session id, as a UUID.
    pub id: String,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct RenameSession {
    pub id: String,
    /// The new title.
    pub name: String,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct StopAgent {
    pub id: String,
    /// Which agent's turn to cancel: "main", or a subagent or sub session id. A
    /// session hosts several sessions and each has a turn of its own.
    pub agent_id: String,
}

/// What to keep out of a transcript read, shared by `read` and `search`.
///
/// Flattened into both inputs rather than nested, so a model writes
/// `{"kinds": ["UserMessage"]}` and not `{"filter": {"kinds": [...]}}` — one
/// less level to get wrong, and the two operations stay describable as the same
/// question asked two ways.
#[derive(serde::Deserialize, schemars::JsonSchema, Default)]
pub struct EntryFilter {
    /// Only entries of these kinds. Absent or empty means every kind.
    ///
    /// This is the lever that makes a long run readable: `["UserMessage"]`
    /// answers "what was this agent asked to do" in one small page, where the
    /// unfiltered version of that same page is mostly tool output.
    pub kinds: Option<Vec<LogEntryKind>>,
    /// Drop the model's reasoning from the messages that come back, keeping
    /// what it actually said. Usually what you want: thinking is the bulk of an
    /// assistant message and rarely what you are reading for.
    pub without_thinking: Option<bool>,
}

impl EntryFilter {
    fn resolve(self) -> LogFilter {
        LogFilter {
            kinds: self.kinds.unwrap_or_default(),
            without_thinking: self.without_thinking.unwrap_or(false),
        }
    }
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct ReadSession {
    pub id: String,
    /// Whose log: "main" (the default), or a subagent, step or sub session id.
    pub aid: Option<String>,
    /// Return the entries immediately before this sequence number. Absent
    /// means the latest ones. This is how you page backwards through a long
    /// transcript.
    pub before: Option<u64>,
    /// Return the entries immediately after this sequence number — how you walk
    /// a run forwards from a point of interest. Ignored when `before` is given.
    pub after: Option<u64>,
    /// Anchor on the entry carrying this id rather than on a sequence number,
    /// for when you have an id you saw quoted rather than a position you paged
    /// to. Reads forwards from it unless `before` is also set.
    pub id_anchor: Option<String>,
    /// How many entries, at most 100. Defaults to 20.
    pub max: Option<usize>,
    #[serde(flatten)]
    pub filter: EntryFilter,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct SearchSession {
    pub id: String,
    /// Whose log: "main" (the default), or a subagent, step or sub session id.
    pub aid: Option<String>,
    /// Case-insensitive substring to look for. Not a regular expression.
    pub query: String,
    /// How many matches, at most 100. Defaults to 20.
    pub max: Option<usize>,
    #[serde(flatten)]
    pub filter: EntryFilter,
}

/// The running work: sessions, their transcripts, and stopping them.
pub struct Sessions;

impl Resource for Sessions {
    fn name(&self) -> &'static str {
        "sessions"
    }

    fn operations(&self) -> Vec<Operation> {
        vec![
            op(
                "list",
                Method::Get,
                "/sessions",
                "Sessions, newest first. Routine runs are excluded unless you \
                 name a routine.",
                Expose::ApiAndTool,
                |s: Arc<ProjectServices>, i: ListSessions| async move { list(&s, i).await },
            ),
            op(
                "get",
                Method::Get,
                "/sessions/{id}",
                "One session in detail: its status, settings and agents.",
                Expose::ApiAndTool,
                |s: Arc<ProjectServices>, i: SessionRef| async move {
                    let (rec, snapshot) = ask(&s, |reply| SessionSupervisorCommand::Get {
                        id: i.id.clone(),
                        reply,
                    })
                    .await?
                    .ok_or_else(|| ControlError::NotFound(format!("no such session: {}", i.id)))?;
                    let windows = handlers::context_windows(&s.config_store)
                        .await
                        .map_err(ControlError::Internal)?;
                    // The *detail* document, not a summary: the two differ and
                    // the web UI reads this one.
                    Ok::<GetSessionResponse, ControlError>(GetSessionResponse {
                        session: handlers::detail(&i.id, &rec, snapshot.as_ref(), &windows),
                    })
                },
            ),
            op(
                "efficiency",
                Method::Get,
                "/sessions/{id}/efficiency",
                "Compact lifetime efficiency diagnostics for a session and each agent: provider and tool latency, cache use, failures, and output truncation.",
                Expose::ApiAndTool,
                |s: Arc<ProjectServices>, i: SessionRef| async move {
                    let (_, snapshot) = ask(&s, |reply| SessionSupervisorCommand::Get {
                        id: i.id.clone(),
                        reply,
                    })
                    .await?
                    .ok_or_else(|| ControlError::NotFound(format!("no such session: {}", i.id)))?;
                    let snapshot = snapshot.ok_or_else(|| {
                        ControlError::Internal(
                            "session efficiency is unavailable until its durable state is loaded"
                                .to_string(),
                        )
                    })?;
                    let windows = handlers::context_windows(&s.config_store)
                        .await
                        .map_err(ControlError::Internal)?;
                    Ok::<SessionEfficiencyReport, ControlError>(
                        handlers::session_efficiency_report(&i.id, &snapshot, &windows),
                    )
                },
            ),
            op(
                "rename",
                Method::Put,
                "/sessions/{id}/name",
                "Retitle a session.",
                Expose::ApiAndTool,
                |s: Arc<ProjectServices>, i: RenameSession| async move { rename(&s, i).await },
            ),
            op(
                "stop",
                Method::Post,
                "/sessions/{id}/agents/{agent_id}/stop",
                "Cancel one agent's turn. An agent that is not working answers \
                 fine — nothing to stop is not a failure.",
                Expose::ApiAndTool,
                |s: Arc<ProjectServices>, i: StopAgent| async move {
                    ask(&s, |reply| SessionSupervisorCommand::Stop {
                        id: i.id,
                        agent_id: i.agent_id,
                        reply,
                    })
                    .await?
                    .map_err(ControlError::NotFound)?;
                    Ok::<Ack, ControlError>(Ack {})
                },
            ),
            op(
                "delete",
                Method::Delete,
                "/sessions/{id}",
                "Delete a session and everything it recorded.",
                Expose::ApiAndTool,
                |s: Arc<ProjectServices>, i: SessionRef| async move {
                    ask(&s, |reply| SessionSupervisorCommand::Delete {
                        id: i.id,
                        reply,
                    })
                    .await?
                    .map_err(ControlError::NotFound)?;
                    Ok::<Ack, ControlError>(Ack {})
                },
            ),
            // Tool-only. `GET /api/sessions/{id}/messages` is two things behind
            // one path — a page that returns and an SSE stream, chosen by which
            // query params are present — and a stream cannot be an operation.
            // The route stays hand-mounted; both call `read_page`.
            op(
                "read",
                Method::Get,
                "/sessions/{id}/messages",
                "Read one agent's transcript. Use this to find out what a session \
                 actually did. The whole transcript will not fit in your context, \
                 so narrow it: `kinds` picks which entries come back (filtered \
                 before the page is cut, so asking for 20 user messages gets you \
                 20), `without_thinking` drops the model's reasoning, and \
                 `before`/`after` walk backwards or forwards from a position.",
                Expose::ToolOnly,
                |s: Arc<ProjectServices>, i: ReadSession| async move { read(&s, i).await },
            ),
            // Tool-only for the same reason `read` is: it answers positions in
            // the log a stream shares its path with, and a browser scrolls a
            // transcript instead of searching one.
            op(
                "search",
                Method::Get,
                "/sessions/{id}/messages/search",
                "Find where in one agent's transcript something was said, without \
                 reading the transcript to get there. Answers positions and short \
                 snippets; feed a `seq` back to `read` as `before` or `after` to \
                 see what surrounds it.",
                Expose::ToolOnly,
                |s: Arc<ProjectServices>, i: SearchSession| async move { search(&s, i).await },
            ),
        ]
    }
}

/// The session list, exactly as `GET /api/sessions` answers it.
///
/// `pub(crate)` because the live feed sends this same projection: a frame that
/// filtered or ordered differently from the route would have a reader replace
/// its list with a set it could never have fetched.
pub(crate) async fn list(
    services: &ProjectServices,
    input: ListSessions,
) -> Result<ListSessionsResponse, ControlError> {
    let all = ask(services, |reply| SessionSupervisorCommand::List { reply }).await?;
    let mut sessions: Vec<SessionSummary> = all
        .iter()
        .filter(|(_, rec)| match (&input.workflow, &input.routine) {
            (Some(w), _) => rec.spec.workflow_name() == Some(w.as_str()),
            (_, Some(r)) => rec.spec.routine() == Some(r.as_str()),
            _ => rec.spec.routine().is_none(),
        })
        .map(|(id, rec)| handlers::summary(id, rec))
        .collect();
    sessions.sort_by_key(|s| std::cmp::Reverse(s.created_at));
    // The envelope, not a bare array: that is what the route already answers.
    Ok(ListSessionsResponse { sessions })
}

async fn rename(services: &ProjectServices, input: RenameSession) -> Result<Ack, ControlError> {
    ask(services, |reply| {
        SessionSupervisorCommand::SetSessionTitle {
            id: input.id,
            name: input.name,
            reply,
        }
    })
    .await?
    .map_err(|e| match e {
        RenameSessionError::NotFound(m) => ControlError::NotFound(m),
        RenameSessionError::Invalid(m) => ControlError::Invalid(m),
    })?;
    Ok(Ack {})
}

/// The agent a caller means, which is "main" unless it named another.
fn agent_of(aid: Option<String>) -> String {
    aid.unwrap_or_else(|| MAIN_AGENT.to_string())
}

/// What a model may ask for in one read or search.
fn clamp(max: Option<usize>) -> usize {
    max.unwrap_or(TOOL_PAGE_DEFAULT).clamp(1, TOOL_PAGE_MAX)
}

async fn read(
    services: &ProjectServices,
    input: ReadSession,
) -> Result<MessagesPage, ControlError> {
    let max = clamp(input.max);
    let agent = agent_of(input.aid);
    // Resolved before the read, not during it: `id_anchor` names an entry and
    // an anchor names a position, and only the agent that owns the log can turn
    // one into the other. Failing here rather than answering an empty page is
    // what tells a caller its id was wrong instead of letting it read "there is
    // nothing there" off a log that is full.
    let anchored = match input.id_anchor {
        None => None,
        Some(entry_id) => Some(
            ask(services, |reply| SessionSupervisorCommand::SeqOfId {
                id: input.id.clone(),
                agent_id: Some(agent.clone()),
                entry_id: entry_id.clone(),
                reply,
            })
            .await?
            .ok_or_else(|| ControlError::NotFound("no such agent".to_string()))?
            .ok_or_else(|| {
                ControlError::Invalid(format!("this agent's log has no entry '{entry_id}'"))
            })?,
        ),
    };
    // `before` wins over `after`, and an id anchor supplies the seq for
    // whichever was asked for — defaulting to forwards, since an id you were
    // handed is usually a place to continue from rather than to scroll back
    // above.
    let anchor = match (input.before, input.after, anchored) {
        (Some(_), _, Some(seq)) | (Some(seq), _, None) => Anchor::Before(seq),
        (None, _, Some(seq)) => Anchor::After(seq),
        (None, Some(seq), None) => Anchor::After(seq),
        (None, None, None) => Anchor::Tail,
    };
    crate::http::messages::read_page(
        services,
        input.id,
        agent,
        anchor,
        max,
        input.filter.resolve(),
    )
    .await
}

async fn search(
    services: &ProjectServices,
    input: SearchSession,
) -> Result<LogSearchPage, ControlError> {
    if input.query.trim().is_empty() {
        return Err(ControlError::Invalid("query must not be empty".to_string()));
    }
    let max = clamp(input.max);
    let hits = ask(services, |reply| SessionSupervisorCommand::SearchLog {
        id: input.id,
        agent_id: Some(agent_of(input.aid)),
        needle: input.query,
        max,
        filter: input.filter.resolve(),
        reply,
    })
    .await?
    .ok_or_else(|| ControlError::NotFound("no such agent".to_string()))?;
    Ok(LogSearchPage { hits })
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

    fn operations() -> Vec<Operation> {
        Sessions.operations()
    }

    #[test]
    fn every_action_is_declared_once_on_one_resource() {
        let mut actions: Vec<&str> = operations().iter().map(|o| o.action).collect();
        actions.sort_unstable();
        assert_eq!(
            actions,
            [
                "delete",
                "efficiency",
                "get",
                "list",
                "read",
                "rename",
                "search",
                "stop",
            ]
        );
        assert_eq!(Sessions.name(), "sessions");
    }

    #[test]
    fn every_path_param_is_a_field_of_its_input() {
        crate::control::tests::assert_path_params_are_inputs(&operations());
    }

    #[test]
    fn efficiency_is_a_compact_api_and_tool_read() {
        let operation = operations()
            .into_iter()
            .find(|operation| operation.action == "efficiency")
            .unwrap();
        assert_eq!(operation.method, Method::Get);
        assert_eq!(operation.path, "/sessions/{id}/efficiency");
        assert_eq!(operation.expose, Expose::ApiAndTool);
    }

    /// The transcript read is the one operation the router must not mount: it
    /// shares a path with the SSE stream, which cannot be an operation.
    #[test]
    fn reading_a_transcript_is_tool_only() {
        let read = operations()
            .into_iter()
            .find(|o| o.action == "read")
            .unwrap();
        assert_eq!(read.expose, Expose::ToolOnly);
    }

    /// `kinds` and `without_thinking` are flattened, so they must land as
    /// top-level properties of both schemas. Nested under a `filter` object the
    /// model would still be *told* about them and the deserialize would still
    /// succeed — with the filter silently defaulted, which reads as "the filter
    /// does nothing" rather than as an error.
    #[test]
    fn the_entry_filter_is_flattened_into_both_inputs() {
        for action in ["read", "search"] {
            let operation = operations()
                .into_iter()
                .find(|o| o.action == action)
                .unwrap();
            let properties = &operation.schema["properties"];
            assert!(
                properties.get("kinds").is_some(),
                "{action} does not offer `kinds` at the top level"
            );
            assert!(
                properties.get("without_thinking").is_some(),
                "{action} does not offer `without_thinking` at the top level"
            );
            assert!(
                properties.get("filter").is_none(),
                "{action} nested the filter, so a model setting `kinds` would be ignored"
            );
        }
    }

    /// Every kind the log can hold must be nameable, or a reader could not ask
    /// for it. Anchored to the enum rather than to a hand-copied list: a variant
    /// added to `LogEntryKind` should appear here without anyone remembering.
    #[test]
    fn every_entry_kind_is_offered_to_the_model() {
        let operation = operations()
            .into_iter()
            .find(|o| o.action == "read")
            .unwrap();
        let offered = operation.schema["properties"]["kinds"].to_string();
        for kind in [
            LogEntryKind::UserMessage,
            LogEntryKind::AssistantMessage,
            LogEntryKind::ToolResult,
            LogEntryKind::Hook,
            LogEntryKind::Lifecycle,
            LogEntryKind::Compaction,
        ] {
            let name = serde_json::to_string(&kind).unwrap();
            let name = name.trim_matches('"');
            assert!(
                offered.contains(name),
                "'{name}' is a kind of log entry but no read can ask for it"
            );
        }
    }
}
