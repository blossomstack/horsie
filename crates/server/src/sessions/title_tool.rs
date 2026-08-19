//! A server-owned tool that renames the interactive session.
//!
//! The sandboxed runtime must not own session metadata, so this tool is layered
//! on by the session server and routed through the owning SessionActor.

use crate::sessions::addressing::SessionRef;
use crate::sessions::session_actor::CoreCommand;
use crate::sessions::session_actor::SessionCommand;
use async_trait::async_trait;
use horsie_agentcore::{ToolCallError, ToolOutcome, ToolSpec, Toolbox};
use serde_json::{Value, json};
use std::sync::Arc;

/// Name of the built-in session title tool.
pub const SET_SESSION_TITLE_TOOL: &str = "set_session_title";

/// Maximum session title length in Unicode characters.
pub(crate) const SESSION_TITLE_MAX_CHARS: usize = 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SessionTitleError {
    Empty,
    Multiline,
    TooLong { max: usize },
}

impl std::fmt::Display for SessionTitleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionTitleError::Empty => write!(f, "session title must not be empty"),
            SessionTitleError::Multiline => {
                write!(f, "session title must be a single line")
            }
            SessionTitleError::TooLong { max } => {
                write!(f, "session title must be at most {max} characters")
            }
        }
    }
}

/// Normalize and validate a model-supplied title. This is the authoritative
/// validation; the JSON schema is only model-facing documentation.
pub(crate) fn normalize_session_title(input: &str) -> Result<String, SessionTitleError> {
    let title = input.trim();
    if title.is_empty() {
        return Err(SessionTitleError::Empty);
    }
    if title.chars().any(|c| c == '\n' || c == '\r') {
        return Err(SessionTitleError::Multiline);
    }
    if title.chars().count() > SESSION_TITLE_MAX_CHARS {
        return Err(SessionTitleError::TooLong {
            max: SESSION_TITLE_MAX_CHARS,
        });
    }
    Ok(title.to_string())
}

fn set_session_title_spec() -> ToolSpec {
    ToolSpec {
        name: SET_SESSION_TITLE_TOOL.to_string(),
        description: "Rename this session at any point with a concise, specific, \
            single-line title. The latest successful call wins."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "required": ["title"],
            "properties": {
                "title": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": SESSION_TITLE_MAX_CHARS,
                    "description": "A concise single-line session title, at most 60 characters. The latest successful call renames the session."
                }
            }
        }),
    }
}

/// Which conversation a `set_session_title` call renames.
///
/// A target rather than a second toolbox type: the tool's name, schema and
/// description are identical either way, and the model should not have to know
/// what kind of conversation it is in to name the one it is having.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TitleTarget {
    Session,
    Fork(uuid::Uuid),
}

/// Wraps the session toolbox, adding the server-owned title tool.
pub struct SessionTitleToolbox {
    inner: Arc<dyn Toolbox>,
    session: SessionRef,
    target: TitleTarget,
}

impl SessionTitleToolbox {
    pub fn new(inner: Arc<dyn Toolbox>, session: SessionRef) -> Self {
        Self {
            inner,
            session,
            target: TitleTarget::Session,
        }
    }

    /// The same tool, renaming one fork instead of the session it lives in.
    pub fn for_fork(inner: Arc<dyn Toolbox>, session: SessionRef, id: uuid::Uuid) -> Self {
        Self {
            inner,
            session,
            target: TitleTarget::Fork(id),
        }
    }
}

#[async_trait]
impl Toolbox for SessionTitleToolbox {
    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = self.inner.specs();
        specs.push(set_session_title_spec());
        specs
    }

    async fn execute(
        &self,
        name: &str,
        input: Value,
        tool_call_id: &str,
    ) -> Result<ToolOutcome, ToolCallError> {
        if name != SET_SESSION_TITLE_TOOL {
            return self.inner.execute(name, input, tool_call_id).await;
        }
        let title = input
            .get("title")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolCallError::InvalidInput("missing 'title'".to_string()))?;
        let title = self
            .session
            .ask(|reply| match self.target {
                TitleTarget::Session => SessionCommand::Core(CoreCommand::SetTitle {
                    title: title.to_string(),
                    reply,
                }),
                TitleTarget::Fork(id) => {
                    SessionCommand::Fork(crate::sessions::session_actor::ForkCommand::SetTitle {
                        id: crate::sessions::session_actor::AgentId(id),
                        title: title.to_string(),
                        reply,
                    })
                }
            })
            .await
            .map_err(|e| ToolCallError::ExecutionFailed(e.to_string()))?
            .map_err(ToolCallError::ExecutionFailed)?;
        Ok(ToolOutcome::Result(Value::String(format!(
            "Session title set to \"{title}\"."
        ))))
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
    use crate::sessions::addressing::SessionInbox;
    use crate::sessions::session_actor::SessionCommand;
    use horsie_actor::{
        ActorContext, CommandEffect, EventSourcedActor, InMemoryJournal, PersistenceId,
    };
    use horsie_agentcore::{EmptyToolbox, ToolCallError, Toolbox};
    use serde::{Deserialize, Serialize};
    use serde_json::json;
    use std::sync::Arc;

    #[test]
    fn normalize_title_trims_and_accepts_unicode() {
        let title = normalize_session_title("  Fix café login ☕  ").unwrap();
        assert_eq!(title, "Fix café login ☕");
    }

    #[test]
    fn normalize_title_rejects_empty_multiline_and_too_long() {
        assert_eq!(
            normalize_session_title("   "),
            Err(SessionTitleError::Empty)
        );
        assert_eq!(
            normalize_session_title("one\ntwo"),
            Err(SessionTitleError::Multiline)
        );
        assert_eq!(
            normalize_session_title("one\rtwo"),
            Err(SessionTitleError::Multiline)
        );
        assert_eq!(
            normalize_session_title(&"é".repeat(61)),
            Err(SessionTitleError::TooLong { max: 60 })
        );
    }

    #[tokio::test]
    async fn tool_spec_documents_rename_any_time_and_latest_wins() {
        let session = SessionRef::new(
            crate::testing::spawn_detached(
                &horsie_actor::ActorSystem::new(Arc::new(InMemoryJournal::new())),
                TitleActor,
            ),
            crate::auth::UserId::bootstrap(),
            uuid::Uuid::new_v4(),
            None,
        );
        let toolbox = SessionTitleToolbox::new(Arc::new(EmptyToolbox), session);
        let spec = toolbox
            .specs()
            .into_iter()
            .find(|s| s.name == SET_SESSION_TITLE_TOOL)
            .unwrap();
        assert!(spec.description.contains("any point"));
        assert!(spec.description.contains("latest successful call wins"));
        assert_eq!(spec.input_schema["required"], json!(["title"]));
        assert_eq!(spec.input_schema["properties"]["title"]["maxLength"], 60);
        assert!(
            spec.input_schema["properties"]["title"]["description"]
                .as_str()
                .unwrap()
                .contains("latest successful call renames the session")
        );
    }

    #[tokio::test]
    async fn execute_returns_the_normalized_title() {
        let session = SessionRef::new(
            crate::testing::spawn_detached(
                &horsie_actor::ActorSystem::new(Arc::new(InMemoryJournal::new())),
                TitleActor,
            ),
            crate::auth::UserId::bootstrap(),
            uuid::Uuid::new_v4(),
            None,
        );
        let toolbox = SessionTitleToolbox::new(Arc::new(EmptyToolbox), session);
        let result = toolbox
            .execute(
                SET_SESSION_TITLE_TOOL,
                json!({"title": "  Improve session titles  "}),
                "tc1",
            )
            .await
            .unwrap();
        assert_eq!(
            result.expect_value(),
            serde_json::Value::String("Session title set to \"Improve session titles\".".into())
        );
    }

    #[tokio::test]
    async fn execute_delegates_other_tools() {
        let session = SessionRef::new(
            crate::testing::spawn_detached(
                &horsie_actor::ActorSystem::new(Arc::new(InMemoryJournal::new())),
                TitleActor,
            ),
            crate::auth::UserId::bootstrap(),
            uuid::Uuid::new_v4(),
            None,
        );
        let toolbox = SessionTitleToolbox::new(Arc::new(EmptyToolbox), session);
        let err = toolbox.execute("bash", json!({}), "tc1").await.unwrap_err();
        assert!(matches!(err, ToolCallError::InvalidInput(_)));
    }

    #[derive(Serialize, Deserialize, Default)]
    struct Empty;

    struct TitleActor;

    #[async_trait::async_trait]
    impl EventSourcedActor for TitleActor {
        type Command = SessionInbox;
        type Event = ();
        type State = Empty;

        fn persistence_id(&self) -> PersistenceId {
            PersistenceId::new("title-test", "title-test")
        }

        fn initial_state() -> Empty {
            Empty
        }

        fn apply_event(state: Empty, _event: ()) -> Empty {
            state
        }

        async fn handle_command(
            &mut self,
            _state: &Empty,
            cmd: SessionInbox,
            _ctx: &mut ActorContext<SessionInbox>,
        ) -> CommandEffect<()> {
            let cmd = cmd.cmd;
            if let SessionCommand::Core(CoreCommand::SetTitle { title, reply }) = cmd {
                let _ =
                    reply.send(normalize_session_title(&title).map_err(|error| error.to_string()));
            }
            CommandEffect::none()
        }
    }
}
