# Session Title Tool Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a server-owned `set_session_title` tool that lets the session agent replace the fallback first-message title with a concise title on any turn, with durable persistence and live sidebar/header updates.

**Architecture:** A `SessionTitleToolbox` wraps the session's existing toolbox and routes `set_session_title` through the owning `SessionActor`. The actor validates and normalizes the title, asks `SessionSupervisor` to persist the existing `SessionNamed` journal event with post-write acknowledgement, and only then updates its local spec and asks the supervisor to publish a dedicated global `TitleChanged` SSE event. The web client applies that tagged event to its session list and detail caches.

**Tech Stack:** Rust 2024, event-sourced actors, fluorite protocol codegen, Tokio, serde_json, React 19, TanStack Query, TypeScript, Playwright.

## Global Constraints

- Follow `docs/superpowers/specs/2026-07-27-session-title-tool-design.md`.
- The existing first-message title derivation remains the immediate fallback.
- Any title may be replaced on any turn; a creation-time name is not locked.
- The latest successful `set_session_title` call wins.
- Titles are trimmed, non-empty, single-line, and at most 60 Unicode characters.
- Tool and parameter descriptions must state that the tool renames the session at any point and that the latest successful call wins.
- Wire protocol types live only in `models/fluorite/*.fl`; regenerate both TypeScript clients after changing them.
- Production Rust denies `unwrap_used`, `expect_used`, `panic`, and wildcard enum match arms. Test modules opt out with the existing `#![cfg_attr(test, allow(...))]` pattern.
- Do not add a React unit-test harness; frontend behavior is verified with typecheck/build and Playwright e2e.
- Commit after each task. Do not include AI attribution in commit messages.

---

## Task 1: Durable session rename and dedicated global title event

**Files:**
- Modify: `models/fluorite/session.fl`
- Modify: `server/src/sessions/supervisor.rs`
- Modify: `server/src/sessions/session_actor.rs`
- Test: `server/src/sessions/supervisor.rs` (`#[cfg(test)] mod tests`)
- Test: `server/src/sessions/session_actor.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Produces:
  - `SessionSupervisorCommand::RenameSession { id: SessionId, name: String, reply: oneshot::Sender<Result<(), JournalError>> }`
  - `SessionSupervisorCommand::PublishSessionTitle { id: SessionId, name: String }`
  - Wire union `GlobalSessionEvent::{StatusChanged, TitleChanged}`
  - Private `SessionActor::rename_session(&mut self, title: String) -> Result<String, String>` returning the accepted title
- Consumes: existing persisted `SessionSupervisorEvent::SessionNamed { id, name }`.

- [ ] **Step 1: Write the failing supervisor tests**

Add these tests to `server/src/sessions/supervisor.rs`'s test module. They intentionally reference commands and generated wire variants that do not exist yet.

```rust
    #[test]
    fn session_named_event_folds_the_latest_title() {
        let state = SessionSupervisor::apply_event(
            SessionSupervisorState::default(),
            SessionSupervisorEvent::SessionCreated {
                id: "s1".into(),
                spec: SessionSpec {
                    name: None,
                    ..spec_fixture()
                },
                created_at: 7,
            },
        );
        let state = SessionSupervisor::apply_event(
            state,
            SessionSupervisorEvent::SessionNamed {
                id: "s1".into(),
                name: "First title".into(),
            },
        );
        let state = SessionSupervisor::apply_event(
            state,
            SessionSupervisorEvent::SessionNamed {
                id: "s1".into(),
                name: "Latest title".into(),
            },
        );
        assert_eq!(
            state.sessions.get("s1").unwrap().spec.name.as_deref(),
            Some("Latest title")
        );
    }

    #[tokio::test]
    async fn rename_session_replies_after_journaling_the_title() {
        let tmp = tempfile::tempdir().unwrap();
        let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
        let (gtx, _grx) = broadcast::channel(16);
        let sup = spawn_root(SessionSupervisor::new(test_deps(&tmp), gtx), journal);

        let id = sup
            .ask(|reply| SessionSupervisorCommand::Create {
                spec: SessionSpec {
                    name: None,
                    ..spec_fixture()
                },
                created_at: 1,
                reply,
            })
            .await
            .unwrap();

        let persisted = sup
            .ask(|reply| SessionSupervisorCommand::RenameSession {
                id: id.clone(),
                name: "Investigate login failure".into(),
                reply,
            })
            .await
            .unwrap();
        assert!(persisted.is_ok());

        let rec = sup
            .ask(|reply| SessionSupervisorCommand::Get {
                id: id.clone(),
                reply,
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            rec.spec.name.as_deref(),
            Some("Investigate login failure")
        );
    }

    #[tokio::test]
    async fn publish_session_title_emits_a_dedicated_title_event() {
        let tmp = tempfile::tempdir().unwrap();
        let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
        let (gtx, mut grx) = broadcast::channel(16);
        let sup = spawn_root(SessionSupervisor::new(test_deps(&tmp), gtx), journal);

        let id = sup
            .ask(|reply| SessionSupervisorCommand::Create {
                spec: SessionSpec {
                    name: None,
                    ..spec_fixture()
                },
                created_at: 1,
                reply,
            })
            .await
            .unwrap();
        sup.ask(|reply| SessionSupervisorCommand::RenameSession {
            id: id.clone(),
            name: "Investigate login failure".into(),
            reply,
        })
        .await
        .unwrap()
        .unwrap();

        sup.tell(SessionSupervisorCommand::PublishSessionTitle {
            id: id.clone(),
            name: "Investigate login failure".into(),
        })
        .await
        .unwrap();

        loop {
            let frame = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                grx.recv(),
            )
            .await
            .unwrap()
            .unwrap();
            match frame {
                horsie_models::session::GlobalSessionEvent::TitleChanged(event) => {
                    assert_eq!(event.session_id, id);
                    assert_eq!(event.name, "Investigate login failure");
                    break;
                }
                horsie_models::session::GlobalSessionEvent::StatusChanged(_) => {}
            }
        }
    }
```

Also add a test in `server/src/sessions/session_actor.rs` that proves the existing fallback path uses the durable rename path. First update the planned `Harness` shape (implemented in Step 5) to include `names` and `published_titles` channels, then add:

```rust
    #[tokio::test]
    async fn first_user_message_still_derives_a_fallback_title() {
        let mut h = harness_on(Arc::new(InMemoryJournal::new()), MockVendor::new());

        let result = h
            .actor
            .ask(|reply| SessionCommand::UserMessage {
                text: "  fix the login redirect  \nwith details".into(),
                reply,
            })
            .await
            .unwrap();

        // The test harness has no provider registered, so the turn itself fails
        // after the title is named. This assertion is about the fallback title.
        assert!(matches!(result, Err(UserMessageError::RecoveryFailed(_))));
        assert_eq!(h.names.recv().await.unwrap(), "fix the login redirect");
        assert_eq!(
            h.published_titles.recv().await.unwrap(),
            "fix the login redirect"
        );
    }
```

- [ ] **Step 2: Run the tests to confirm they fail**

Run:

```bash
cargo test -p horsie-server sessions::supervisor::tests::rename_session_replies_after_journaling_the_title
cargo test -p horsie-server sessions::supervisor::tests::publish_session_title_emits_a_dedicated_title_event
cargo test -p horsie-server sessions::session_actor::tests::first_user_message_still_derives_a_fallback_title
```

Expected: compile errors for missing `RenameSession`, `PublishSessionTitle`, and `GlobalSessionEvent::TitleChanged`.

- [ ] **Step 3: Change the global session event wire type to a tagged union**

Replace the `GlobalSessionEvent` struct at the end of `models/fluorite/session.fl` with:

```text
/// A status frame on the global `/api/events` stream.
struct GlobalSessionStatusEvent {
    session_id: String,
    status: SessionStatusKind,
    reason: Option<String>,
}

/// A title frame on the global `/api/events` stream.
struct GlobalSessionTitleEvent {
    session_id: String,
    name: String,
}

/// One frame on the global `/api/events` stream (live session-list updates).
#[type_tag = "type"]
union GlobalSessionEvent {
    StatusChanged(GlobalSessionStatusEvent),
    TitleChanged(GlobalSessionTitleEvent),
}
```

- [ ] **Step 4: Implement durable rename and post-persist publication in the supervisor**

In `server/src/sessions/supervisor.rs`:

1. Import the new generated payload types:

```rust
use horsie_models::session::{
    GlobalSessionEvent, GlobalSessionStatusEvent, GlobalSessionTitleEvent,
};
```

2. Replace `SessionSupervisorCommand::SessionNamed { id, name }` with:

```rust
    /// Internal: a session actor requests a durable rename. Replies only after
    /// the SessionNamed event is journaled.
    RenameSession {
        id: SessionId,
        name: String,
        reply: oneshot::Sender<Result<(), horsie_actor::JournalError>>,
    },
    /// Internal: publish an already-journaled title to the global live feed.
    PublishSessionTitle { id: SessionId, name: String },
```

3. Replace `publish` and add `publish_title`:

```rust
    fn publish(&self, id: &str, status: &SessionStatus) {
        let _ = self
            .global_tx
            .send(GlobalSessionEvent::StatusChanged(
                GlobalSessionStatusEvent {
                    session_id: id.to_string(),
                    status: status_kind(status),
                    reason: status_reason(status),
                },
            ));
    }

    fn publish_title(&self, id: &str, name: &str) {
        let _ = self
            .global_tx
            .send(GlobalSessionEvent::TitleChanged(
                GlobalSessionTitleEvent {
                    session_id: id.to_string(),
                    name: name.to_string(),
                },
            ));
    }
```

4. Replace the `SessionSupervisorCommand::SessionNamed` match arm with:

```rust
            SessionSupervisorCommand::RenameSession { id, name, reply } => {
                CommandEffect::persist(vec![SessionSupervisorEvent::SessionNamed {
                    id,
                    name,
                }])
                .and_ack(reply)
            }
            SessionSupervisorCommand::PublishSessionTitle { id, name } => {
                // A rename command that was superseded while its publish request
                // was queued must not broadcast a stale title.
                let current = state
                    .sessions
                    .get(&id)
                    .and_then(|rec| rec.spec.name.as_deref());
                if current == Some(name.as_str()) {
                    self.publish_title(&id, &name);
                }
                CommandEffect::none()
            }
```

5. Update existing supervisor tests that inspect global frames to match the union. For example, in `create_list_get_delete_round_trip`, replace direct field access with:

```rust
        match grx.recv().await.unwrap() {
            horsie_models::session::GlobalSessionEvent::StatusChanged(frame) => {
                assert_eq!(frame.session_id, id);
                assert_eq!(frame.status, SessionStatusKind::Provisioning);
            }
            horsie_models::session::GlobalSessionEvent::TitleChanged(_) => {
                panic!("creation must not publish a title frame")
            }
        }
```

- [ ] **Step 5: Route the fallback title through the durable rename path**

In `server/src/sessions/session_actor.rs`, add this private method to `impl SessionActor` near `report`:

```rust
    /// Persist a session title through the supervisor, then update local state
    /// and publish the already-durable title. Live publication is best-effort;
    /// the journal remains the source of truth.
    async fn rename_session(&mut self, title: String) -> Result<String, String> {
        let id = self.id.to_string();
        let persisted = self
            .parent
            .ask(|reply| SessionSupervisorCommand::RenameSession {
                id: id.clone(),
                name: title.clone(),
                reply,
            })
            .await
            .map_err(|e| format!("session supervisor unavailable: {e}"))?;
        persisted.map_err(|e| format!("persist session title: {e}"))?;

        self.spec.name = Some(title.clone());
        let _ = self
            .parent
            .tell(SessionSupervisorCommand::PublishSessionTitle {
                id,
                name: title.clone(),
            })
            .await;
        Ok(title)
    }
```

Replace the current fallback block in `on_user_message` that directly mutates `self.spec.name` and tells `SessionNamed` with:

```rust
        // An unnamed session is titled from its first message, once — like
        // other chat products. A caller-supplied name at creation starts as
        // the title, but can still be replaced later by set_session_title.
        if self.spec.name.is_none()
            && let Some(title) = derive_title(&text)
            && let Err(error) = self.rename_session(title).await
        {
            tracing::warn!(session = %self.id, error, "failed to persist fallback session title");
        }
```

Update `NullSupervisor` in the session actor tests to record rename and publish commands. Give it two extra channels:

```rust
    struct NullSupervisor {
        statuses: tokio::sync::mpsc::UnboundedSender<SessionStatus>,
        names: tokio::sync::mpsc::UnboundedSender<String>,
        published_titles: tokio::sync::mpsc::UnboundedSender<String>,
    }
```

Handle the new commands in its `handle_command`:

```rust
            match cmd {
                SessionSupervisorCommand::SessionStatusChanged { status, .. } => {
                    let _ = self.statuses.send(status);
                }
                SessionSupervisorCommand::RenameSession { name, reply, .. } => {
                    let _ = self.names.send(name);
                    let _ = reply.send(Ok(()));
                }
                SessionSupervisorCommand::PublishSessionTitle { name, .. } => {
                    let _ = self.published_titles.send(name);
                }
                _ => {}
            }
```

Extend `Harness` with receivers:

```rust
        names: tokio::sync::mpsc::UnboundedReceiver<String>,
        published_titles: tokio::sync::mpsc::UnboundedReceiver<String>,
```

Create all three channels in `harness_custom`, pass their senders into `NullSupervisor`, and store the receivers in `Harness`.

- [ ] **Step 6: Run the focused tests**

Run:

```bash
cargo test -p horsie-server sessions::supervisor::tests
cargo test -p horsie-server sessions::session_actor::tests::first_user_message_still_derives_a_fallback_title
```

Expected: PASS. Fix any existing global-event test that still treats `GlobalSessionEvent` as a struct.

- [ ] **Step 7: Commit**

```bash
git add models/fluorite/session.fl server/src/sessions/supervisor.rs server/src/sessions/session_actor.rs
git commit -m "server: persist session renames and publish title events"
```

---

## Task 2: Add the validated `set_session_title` tool and session command

**Files:**
- Create: `server/src/sessions/title_tool.rs`
- Modify: `server/src/sessions/mod.rs`
- Modify: `server/src/sessions/session_actor.rs`
- Test: `server/src/sessions/title_tool.rs` (`#[cfg(test)] mod tests`)
- Test: `server/src/sessions/session_actor.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes:
  - `SessionSupervisorCommand::RenameSession` and `PublishSessionTitle` from Task 1.
  - Private `SessionActor::rename_session` from Task 1.
- Produces:
  - `pub const SET_SESSION_TITLE_TOOL: &str = "set_session_title"`
  - `pub(crate) const SESSION_TITLE_MAX_CHARS: usize = 60`
  - `pub(crate) fn normalize_session_title(input: &str) -> Result<String, SessionTitleError>`
  - `pub struct SessionTitleToolbox`
  - `SessionCommand::SetSessionTitle { title: String, reply: oneshot::Sender<Result<String, String>> }`

- [ ] **Step 1: Write the failing title-tool tests**

Create `server/src/sessions/title_tool.rs` initially containing only this test module. It references the implementation that Step 3 adds.

```rust
//! Tests for the session-title tool.

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;
    use crate::sessions::session_actor::SessionCommand;
    use horsie_actor::{
        ActorContext, CommandEffect, EventSourcedActor, InMemoryJournal, PersistenceId,
        spawn_root,
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
        assert_eq!(normalize_session_title("   "), Err(SessionTitleError::Empty));
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

    #[test]
    fn tool_spec_documents_rename_any_time_and_latest_wins() {
        let session = spawn_root(TitleActor, Arc::new(InMemoryJournal::new()));
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
        let session = spawn_root(TitleActor, Arc::new(InMemoryJournal::new()));
        let toolbox = SessionTitleToolbox::new(Arc::new(EmptyToolbox), session);
        let result = toolbox
            .execute(
                SET_SESSION_TITLE_TOOL,
                json!({"title": "  Improve session titles  "}),
            )
            .await
            .unwrap();
        assert_eq!(
            result,
            serde_json::Value::String(
                "Session title set to \"Improve session titles\".".into()
            )
        );
    }

    #[tokio::test]
    async fn execute_delegates_other_tools() {
        let session = spawn_root(TitleActor, Arc::new(InMemoryJournal::new()));
        let toolbox = SessionTitleToolbox::new(Arc::new(EmptyToolbox), session);
        let err = toolbox.execute("bash", json!({})).await.unwrap_err();
        assert!(matches!(err, ToolCallError::InvalidInput(_)));
    }

    #[derive(Serialize, Deserialize, Default)]
    struct Empty;

    struct TitleActor;

    #[async_trait::async_trait]
    impl EventSourcedActor for TitleActor {
        type Command = SessionCommand;
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
            cmd: SessionCommand,
            _ctx: &mut ActorContext<Self>,
        ) -> CommandEffect<()> {
            if let SessionCommand::SetSessionTitle { title, reply } = cmd {
                let _ = reply.send(
                    normalize_session_title(&title).map_err(|error| error.to_string()),
                );
            }
            CommandEffect::none()
        }
    }
}
```

Register the module in `server/src/sessions/mod.rs`:

```rust
pub mod title_tool;
```

- [ ] **Step 2: Run the new tests to confirm they fail**

Run:

```bash
cargo test -p horsie-server sessions::title_tool::tests
```

Expected: compile errors for missing `SET_SESSION_TITLE_TOOL`, `SESSION_TITLE_MAX_CHARS`, `SessionTitleError`, `normalize_session_title`, `SessionTitleToolbox`, and `SessionCommand::SetSessionTitle`.

- [ ] **Step 3: Implement the title tool**

Add the production code above the test module in `server/src/sessions/title_tool.rs`:

```rust
//! A server-owned tool that renames the interactive session.
//!
//! The sandboxed runtime must not own session metadata, so this tool is layered
//! on by the session server and routed through the owning SessionActor.

use crate::sessions::session_actor::SessionCommand;
use async_trait::async_trait;
use horsie_actor::ActorRef;
use horsie_agentcore::{ToolCallError, ToolSpec, Toolbox};
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

/// Wraps the session toolbox, adding the server-owned title tool.
pub struct SessionTitleToolbox {
    inner: Arc<dyn Toolbox>,
    session: ActorRef<SessionCommand>,
}

impl SessionTitleToolbox {
    pub fn new(inner: Arc<dyn Toolbox>, session: ActorRef<SessionCommand>) -> Self {
        Self { inner, session }
    }
}

#[async_trait]
impl Toolbox for SessionTitleToolbox {
    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = self.inner.specs();
        specs.push(set_session_title_spec());
        specs
    }

    async fn execute(&self, name: &str, input: Value) -> Result<Value, ToolCallError> {
        if name != SET_SESSION_TITLE_TOOL {
            return self.inner.execute(name, input).await;
        }
        let title = input
            .get("title")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolCallError::InvalidInput("missing 'title'".to_string()))?;
        let title = self
            .session
            .ask(|reply| SessionCommand::SetSessionTitle {
                title: title.to_string(),
                reply,
            })
            .await
            .map_err(|e| ToolCallError::ExecutionFailed(e.to_string()))?
            .map_err(ToolCallError::ExecutionFailed)?;
        Ok(Value::String(format!(
            "Session title set to \"{title}\"."
        )))
    }
}
```

- [ ] **Step 4: Add the session command and validation path**

In `server/src/sessions/session_actor.rs`:

1. Import the validation helper and share the title limit:

```rust
use crate::sessions::title_tool::normalize_session_title;
```

Change the existing private `TITLE_MAX_CHARS` constant to reuse the tool's constant:

```rust
const TITLE_MAX_CHARS: usize = crate::sessions::title_tool::SESSION_TITLE_MAX_CHARS;
```

2. Add the command variant:

```rust
    /// Set the session title from the built-in title tool.
    SetSessionTitle {
        title: String,
        reply: oneshot::Sender<Result<String, String>>,
    },
```

3. Add this match arm to `handle_command`:

```rust
            SessionCommand::SetSessionTitle { title, reply } => {
                let result = match normalize_session_title(&title) {
                    Ok(title) => self.rename_session(title).await,
                    Err(error) => Err(error.to_string()),
                };
                let _ = reply.send(result);
                CommandEffect::none()
            }
```

- [ ] **Step 5: Add session-command tests for unrestricted replacement and validation**

Add these tests to `server/src/sessions/session_actor.rs`'s test module:

```rust
    #[tokio::test]
    async fn set_session_title_replaces_a_creation_name() {
        let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
        let mut spec = spec_fixture("mock");
        spec.name = Some("Creation name".into());
        let mut h = harness_custom(
            journal,
            MockVendor::new(),
            Uuid::new_v4(),
            spec,
            None,
        );

        let first = h
            .actor
            .ask(|reply| SessionCommand::SetSessionTitle {
                title: "  Better model title  ".into(),
                reply,
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first, "Better model title");
        assert_eq!(h.names.recv().await.unwrap(), "Better model title");
        assert_eq!(
            h.published_titles.recv().await.unwrap(),
            "Better model title"
        );

        let latest = h
            .actor
            .ask(|reply| SessionCommand::SetSessionTitle {
                title: "Latest title wins".into(),
                reply,
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(latest, "Latest title wins");
        assert_eq!(h.names.recv().await.unwrap(), "Latest title wins");
        assert_eq!(
            h.published_titles.recv().await.unwrap(),
            "Latest title wins"
        );
    }

    #[tokio::test]
    async fn set_session_title_rejects_invalid_titles_without_renaming() {
        let mut h = harness_on(Arc::new(InMemoryJournal::new()), MockVendor::new());

        let too_long = "é".repeat(61);
        for title in ["   ", "one\ntwo", too_long.as_str()] {
            let error = h
                .actor
                .ask(|reply| SessionCommand::SetSessionTitle {
                    title: title.to_string(),
                    reply,
                })
                .await
                .unwrap()
                .unwrap_err();
            assert!(!error.is_empty());
            assert!(h.names.try_recv().is_err());
            assert!(h.published_titles.try_recv().is_err());
        }
    }
```

- [ ] **Step 6: Run the focused tests**

Run:

```bash
cargo test -p horsie-server sessions::title_tool::tests
cargo test -p horsie-server sessions::session_actor::tests::set_session_title
cargo test -p horsie-server sessions::session_actor::tests::derive_title_uses_trimmed_first_line
```

Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add server/src/sessions/title_tool.rs server/src/sessions/mod.rs server/src/sessions/session_actor.rs
git commit -m "server: add set_session_title tool"
```

---

## Task 3: Wire the tool into session runs and update the system prompt

**Files:**
- Modify: `server/src/sessions/session_actor.rs`
- Modify: `server/src/sessions/system_prompt.md`
- Test: `server/src/sessions/session_actor.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `SessionTitleToolbox::new(inner, session)` and `SET_SESSION_TITLE_TOOL` from Task 2.
- Produces: every interactive session run advertises `set_session_title` alongside runtime, MCP, and `ask_user` tools.

- [ ] **Step 1: Write the failing prompt test**

Add this test to `server/src/sessions/session_actor.rs`'s test module:

```rust
    #[test]
    fn system_prompt_instructs_the_agent_to_title_the_session() {
        assert!(SESSION_AGENT_PROMPT.contains("## Session title"));
        assert!(SESSION_AGENT_PROMPT.contains("set_session_title"));
        assert!(SESSION_AGENT_PROMPT.contains("first turn"));
        assert!(SESSION_AGENT_PROMPT.contains("latest successful call wins"));
    }
```

- [ ] **Step 2: Run it to confirm it fails**

Run:

```bash
cargo test -p horsie-server sessions::session_actor::tests::system_prompt_instructs_the_agent_to_title_the_session
```

Expected: FAIL because `system_prompt.md` does not contain the section.

- [ ] **Step 3: Add the prompt section**

Append this section to `server/src/sessions/system_prompt.md`:

```markdown
## Session title

On the first turn, call `set_session_title` with a concise, specific title that
summarizes the user's request. The server may already have set a fallback title
from the first user message; replace it when you can provide a clearer title.
You may call the tool again later if the conversation's purpose changes; the
latest successful call wins.
```

- [ ] **Step 4: Pass the session actor reference into the per-run context provider**

Add a field to `SessionContextProvider` in `server/src/sessions/session_actor.rs`:

```rust
    session: ActorRef<SessionCommand>,
```

In `ensure_agent`, include it when constructing the provider:

```rust
        let context_provider = Arc::new(SessionContextProvider {
            runtime_client: runtime.runtime_client.clone(),
            provider,
            mcp: self.deps.mcp.clone(),
            settings: self.spec.agent.clone(),
            session_id: self.id,
            session: ctx.self_ref(),
            frames: self.frames.clone(),
        });
```

- [ ] **Step 5: Wrap the composed toolbox with the title tool**

In `SessionContextProvider::provide`, replace the current `AskUserToolbox` construction with:

```rust
        let inner: Arc<dyn Toolbox> = Arc::new(AskUserToolbox::new(
            DefaultToolboxFactory.for_agent(
                &def,
                self.runtime_client.clone(),
                ws.names(),
                use_plugins,
                mcp,
            ),
        ));
        let toolbox: Arc<dyn Toolbox> = Arc::new(SessionTitleToolbox::new(
            inner,
            self.session.clone(),
        ));
```

Import the toolbox:

```rust
use crate::sessions::title_tool::SessionTitleToolbox;
```

- [ ] **Step 6: Run focused tests and compile checks**

Run:

```bash
cargo test -p horsie-server sessions::session_actor::tests::system_prompt_instructs_the_agent_to_title_the_session
cargo test -p horsie-server sessions::title_tool::tests
cargo check -p horsie-server
```

Expected: PASS. `cargo check` proves the context provider wiring is type-correct.

- [ ] **Step 7: Commit**

```bash
git add server/src/sessions/session_actor.rs server/src/sessions/system_prompt.md
git commit -m "server: expose session title tool to agents"
```

---

## Task 4: Regenerate clients and handle `TitleChanged` in the web UI

**Files:**
- Modify: `clients/web/src/hooks/useSessions.ts`
- Create: `clients/web/e2e/k-session-title.spec.ts`
- Regenerate: `clients/ts/src/generated/session/globalSession*.ts`
- Regenerate: `clients/web/src/generated/session/globalSession*.ts`

**Interfaces:**
- Consumes: generated union `GlobalSessionEvent` from Task 1 and live `TitleChanged` frames from the supervisor.
- Produces: session list and detail caches update immediately on title changes.

- [ ] **Step 1: Regenerate TypeScript protocol types**

Run:

```bash
cd clients/ts && bun run generate-types && cd ../..
cd clients/web && bun run generate-types && cd ../..
```

Expected generated shape in both clients' `session/globalSessionEvent.ts`:

```ts
export type GlobalSessionEvent =
  | { type: "StatusChanged"; value: GlobalSessionStatusEvent }
  | { type: "TitleChanged"; value: GlobalSessionTitleEvent };
```

The exact file names may use fluorite's lower-camel output (`globalSessionStatusEvent.ts`, `globalSessionTitleEvent.ts`). Commit all generated changes, including new index exports.

- [ ] **Step 2: Update the global event cache handler**

Replace `applyGlobalEvent` in `clients/web/src/hooks/useSessions.ts` with:

```ts
function applyGlobalStatus(
  client: QueryClient,
  ev: Extract<GlobalSessionEvent, { type: "StatusChanged" }>["value"],
) {
  let matched = false;
  client.setQueryData<ListSessionsResponse>(qk.sessions, (prev) => {
    if (!prev) return prev;
    const sessions = prev.sessions.map((s) => {
      if (s.id !== ev.sessionId) return s;
      matched = true;
      return { ...s, status: ev.status, lastError: ev.reason ?? s.lastError };
    });
    return { sessions };
  });
  if (!matched) client.invalidateQueries({ queryKey: qk.sessions });

  client.setQueryData<GetSessionResponse>(
    qk.session(ev.sessionId),
    (prev) =>
      prev
        ? {
            session: {
              ...prev.session,
              status: ev.status,
              lastError: ev.reason ?? prev.session.lastError,
            },
          }
        : prev,
  );
}

function applyGlobalTitle(
  client: QueryClient,
  ev: Extract<GlobalSessionEvent, { type: "TitleChanged" }>["value"],
) {
  let matched = false;
  client.setQueryData<ListSessionsResponse>(qk.sessions, (prev) => {
    if (!prev) return prev;
    const sessions = prev.sessions.map((s) => {
      if (s.id !== ev.sessionId) return s;
      matched = true;
      return { ...s, name: ev.name };
    });
    return { sessions };
  });
  if (!matched) client.invalidateQueries({ queryKey: qk.sessions });

  client.setQueryData<GetSessionResponse>(
    qk.session(ev.sessionId),
    (prev) =>
      prev
        ? {
            session: {
              ...prev.session,
              name: ev.name,
            },
          }
        : prev,
  );
}

function applyGlobalEvent(client: QueryClient, ev: GlobalSessionEvent) {
  switch (ev.type) {
    case "StatusChanged":
      applyGlobalStatus(client, ev.value);
      return;
    case "TitleChanged":
      applyGlobalTitle(client, ev.value);
      return;
  }
}
```

Update the `useGlobalSessionFeed` comment from “statuses change” to “session status or title changes”.

- [ ] **Step 3: Add a Playwright regression test for live title updates**

Create `clients/web/e2e/k-session-title.spec.ts`:

```ts
// Group K — the model-owned session title tool. The server first derives a
// fallback title from the user's message, then the model replaces it and the
// dedicated TitleChanged global event updates the header and sidebar live.

import { test, expect } from "./fixtures";
import { createSession, expectStatus, sendMessage } from "./helpers";

test.beforeEach(async ({ mock }) => {
  await mock.reset();
});

test("K1: set_session_title renames the session live", async ({
  page,
  appBase,
  mock,
}) => {
  await mock.queueToolCall("set_session_title", {
    title: "Fix login redirect",
  });
  await mock.queueText("I’ll investigate the redirect behavior.");
  await createSession(page, appBase);

  const id = await sendMessage(page, "the login redirects to the wrong page");

  await expect(page.getByTestId("session-title")).toHaveText(
    "Fix login redirect",
  );
  await expect(
    page.locator(
      `[data-testid="session-row"][data-session-id="${id}"]`,
    ),
  ).toContainText("Fix login redirect");
  await expect(page.getByTestId("assistant-text")).toContainText(
    "I’ll investigate the redirect behavior.",
  );
  await expectStatus(page, "Idle");
});
```

- [ ] **Step 4: Run frontend checks**

Run:

```bash
cd clients/ts && bun run typecheck && cd ../..
cd clients/web && bun run typecheck && bun run build && cd ../..
```

Expected: PASS with exhaustive handling of both global event variants.

- [ ] **Step 5: Run the focused Playwright test**

Run:

```bash
cd clients/web && bun run test:e2e -- e2e/k-session-title.spec.ts
```

Expected: PASS. The mock LLM calls the server-owned tool, the tool result is journaled, the final assistant message renders, and both header and sidebar show the model-set title.

- [ ] **Step 6: Commit**

```bash
git add clients/ts/src/generated clients/web/src/generated clients/web/src/hooks/useSessions.ts clients/web/e2e/k-session-title.spec.ts
git commit -m "web: apply live session title events"
```

---

## Task 5: Full verification

**Files:**
- Verify all files changed by Tasks 1–4.

- [ ] **Step 1: Format**

Run:

```bash
cargo fmt --check
```

Expected: PASS. If formatting fails, run `cargo fmt`, rerun the check, and include the formatting changes in the next commit.

- [ ] **Step 2: Rust tests**

Run:

```bash
cargo test --workspace
```

Expected: PASS.

- [ ] **Step 3: Clippy**

Run:

```bash
cargo clippy --all-targets --all-features -- -D warnings
```

Expected: PASS.

- [ ] **Step 4: Web checks and full e2e suite**

Run:

```bash
cd clients/ts && bun run typecheck && cd ../..
cd clients/web && bun run typecheck && bun run build && bun run test:e2e && cd ../..
```

Expected: PASS, including `K1: set_session_title renames the session live`.

- [ ] **Step 5: Confirm protocol generation is stable**

Run:

```bash
cd clients/ts && bun run generate-types && git diff --exit-code src/generated && cd ../..
cd clients/web && bun run generate-types && git diff --exit-code src/generated && cd ../..
```

Expected: no diff.

- [ ] **Step 6: Review the final diff and commit any verification fixes**

Run:

```bash
git status --short
git diff --check
git log --oneline -5
```

If verification required fixes, stage and commit them with a focused message such as:

```bash
git add <changed-files>
git commit -m "server: address session title verification findings"
```

Expected: working tree contains only intentional uncommitted changes (ideally none).

---

## Self-review notes

- **Spec coverage:** fallback first-message title preserved (Task 1); server-owned tool (Task 2); any-turn/latest-wins semantics (Task 2 tests, including creation-name replacement); validation (Task 2); prompt instruction (Task 3); durable post-write acknowledgement (Task 1); dedicated global `TitleChanged` event (Tasks 1 and 4); live list and detail updates (Task 4); e2e coverage (Task 4); full gates (Task 5).
- **No placeholders:** every implementation step names exact files, interfaces, commands, and expected results.
- **Type consistency:** `rename_session` should return `Result<String, String>` (the normalized title) as clarified in Task 2 Step 4. `RenameSession` replies with `Result<(), JournalError>`; `PublishSessionTitle` is fire-and-forget after durability.
