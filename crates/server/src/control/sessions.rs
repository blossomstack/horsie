//! The sessions resource: the running work, as opposed to the configuration
//! that starts it.
//!
//! Reading a transcript is here too, and it is the reason an ops agent is worth
//! having: diagnosing last night's failed routine means looking at what it
//! actually said. It is [`Expose::ToolOnly`] because the route it shares a path
//! with is a stream — see `read` below.

use crate::control::{ControlError, Expose, Method, Operation, Resource, ask, op};
use crate::http::handlers;
use crate::projects::ProjectServices;
use crate::sessions::supervisor::RenameSessionError;
use crate::sessions::supervisor::SessionSupervisorCommand;
use horsie_models::session::SessionSummary;
use horsie_models::session_api::{Ack, GetSessionResponse, ListSessionsResponse, MessagesPage};
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
    /// Which agent's turn to cancel: "main", or a subagent or fork id. A
    /// session hosts several conversations and each has a turn of its own.
    pub agent_id: String,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct ReadSession {
    pub id: String,
    /// Whose log: "main" (the default), or a subagent, step or fork id.
    pub aid: Option<String>,
    /// Return the entries immediately before this sequence number. Absent
    /// means the latest ones. This is how you page backwards through a long
    /// transcript.
    pub before: Option<u64>,
    /// How many entries, at most 100. Defaults to 20.
    pub max: Option<usize>,
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
                    // The *detail* document, not a summary: the two differ and
                    // the web UI reads this one.
                    Ok::<GetSessionResponse, ControlError>(GetSessionResponse {
                        session: handlers::detail(&i.id, &rec, snapshot.as_ref()),
                    })
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
                "Read one agent's transcript, newest entries first. Use this to \
                 find out what a session actually did. Ask for a small page and \
                 use `before` to go further back — the whole transcript will not \
                 fit in your context.",
                Expose::ToolOnly,
                |s: Arc<ProjectServices>, i: ReadSession| async move { read(&s, i).await },
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

async fn read(
    services: &ProjectServices,
    input: ReadSession,
) -> Result<MessagesPage, ControlError> {
    let max = input
        .max
        .unwrap_or(TOOL_PAGE_DEFAULT)
        .clamp(1, TOOL_PAGE_MAX);
    crate::http::messages::read_page(
        services,
        input.id,
        input.aid.unwrap_or_else(|| MAIN_AGENT.to_string()),
        input.before,
        max,
    )
    .await
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
        assert_eq!(actions, ["delete", "get", "list", "read", "rename", "stop"]);
        assert_eq!(Sessions.name(), "sessions");
    }

    #[test]
    fn every_path_param_is_a_field_of_its_input() {
        crate::control::tests::assert_path_params_are_inputs(&operations());
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
}
