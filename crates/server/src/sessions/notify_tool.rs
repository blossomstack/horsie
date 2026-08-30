//! A "tell the user something" tool.
//!
//! The counterpart to [`ask_tool`](super::ask_tool), and deliberately its
//! opposite in the one way that matters: calling this does **not** end the run.
//! The message lands in the person's inbox and the agent carries straight on.
//!
//! That is the whole reason it exists. Before it, an agent with something worth
//! saying had exactly two options — park itself on `ask_user` and stop, or bury
//! the remark in a transcript nobody is reading. Neither is right for "I have
//! finished the migration and two of the fixtures looked wrong", which needs to
//! reach a person without costing the agent its momentum.
//!
//! Offered to every kind of agent, unattended runs included. A notice does not
//! wait for anyone, so an overnight routine putting one in the inbox is the
//! case that most justifies there being an inbox at all — unlike `ask_user`,
//! which an unattended session is not given because nobody is there to answer.

use crate::user_inbox::{NoticeRow, UserInboxStore, now_ms_i64};
use async_trait::async_trait;
use horsie_agentcore::{ToolCallError, ToolOutcome, ToolSpec, Toolbox};
use serde_json::{Value, json};
use std::sync::Arc;

/// Name of the always-available "tell the user something" tool.
pub const NOTIFY_USER_TOOL: &str = "notify_user";

/// How long a title may be before it stops being a title.
const MAX_TITLE: usize = 80;

fn notify_user_spec() -> ToolSpec {
    ToolSpec {
        name: NOTIFY_USER_TOOL.to_string(),
        description: "Put a message in the user's inbox. Use it to report something worth their \
            attention that does not need an answer -- work finished, something surprising you \
            found, a decision you took on your own. It does not pause you: you keep working, and \
            they read it whenever they next look. If you actually need an answer before you can \
            continue, call `ask_user` instead. Do not narrate ordinary progress here -- an inbox \
            full of routine updates is an inbox nobody reads."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "required": ["body"],
            "properties": {
                "body": {
                    "type": "string",
                    "description": "The message, in Markdown."
                },
                "title": {
                    "type": "string",
                    "description": "A short subject line for the inbox list -- one specific \
                        phrase, not a sentence. Omit it and the message's first line is used."
                }
            }
        }),
    }
}

/// Wraps an inner toolbox, adding `notify_user` bound to one agent.
///
/// The session and agent are bound here, at construction, rather than asked of
/// the model: a message whose origin the caller supplies is a message that can
/// claim to come from somewhere it did not, and "open the session this came
/// from" is the inbox's most-used control.
pub struct NotifyUserToolbox {
    inner: Arc<dyn Toolbox>,
    inbox: Arc<UserInboxStore>,
    session_id: String,
    /// `"main"`, or the agent's uuid — the vocabulary every agent-scoped route
    /// speaks, so the row is an address without translation.
    agent_id: String,
}

impl NotifyUserToolbox {
    pub fn new(
        inner: Arc<dyn Toolbox>,
        inbox: Arc<UserInboxStore>,
        session_id: String,
        agent_id: String,
    ) -> Self {
        Self {
            inner,
            inbox,
            session_id,
            agent_id,
        }
    }
}

#[async_trait]
impl Toolbox for NotifyUserToolbox {
    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = self.inner.specs();
        specs.push(notify_user_spec());
        specs
    }

    async fn execute(
        &self,
        name: &str,
        input: Value,
        tool_call_id: &str,
    ) -> Result<ToolOutcome, ToolCallError> {
        if name != NOTIFY_USER_TOOL {
            return self.inner.execute(name, input, tool_call_id).await;
        }
        let body = input
            .get("body")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|b| !b.is_empty())
            .ok_or_else(|| ToolCallError::InvalidInput("missing 'body'".to_string()))?;
        let title = input
            .get("title")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map_or_else(|| derive_title(body), truncate_title);

        self.inbox
            .record_notice(
                &NoticeRow {
                    session_id: self.session_id.clone(),
                    agent_id: self.agent_id.clone(),
                    title,
                    body: body.to_string(),
                },
                now_ms_i64(),
            )
            .await
            .map_err(ToolCallError::ExecutionFailed)?;
        Ok(ToolOutcome::Result(Value::String(
            "Delivered to the user's inbox. They will see it when they next look; you have not \
             been paused, so carry on."
                .to_string(),
        )))
    }
}

/// A subject line taken from the message itself.
///
/// Markdown's first line, with any heading marker or list bullet stripped —
/// those are formatting, and a list row that reads "## Migration finished"
/// shows the model's syntax rather than its words.
fn derive_title(body: &str) -> String {
    let first = body
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let stripped = first
        .trim_start_matches('#')
        .trim_start_matches(['-', '*', '>'])
        .trim();
    if stripped.is_empty() {
        "(no subject)".to_string()
    } else {
        truncate_title(stripped)
    }
}

/// Clamp to something that fits a list row, on a character boundary.
fn truncate_title(title: &str) -> String {
    if title.chars().count() <= MAX_TITLE {
        return title.to_string();
    }
    let head: String = title.chars().take(MAX_TITLE).collect();
    format!("{}…", head.trim_end())
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
    use crate::user_inbox::InboxFilter;

    struct EmptyToolbox;

    #[async_trait]
    impl Toolbox for EmptyToolbox {
        fn specs(&self) -> Vec<ToolSpec> {
            vec![]
        }

        async fn execute(
            &self,
            name: &str,
            _input: Value,
            _tool_call_id: &str,
        ) -> Result<ToolOutcome, ToolCallError> {
            Err(ToolCallError::InvalidInput(format!(
                "no tool named '{name}'"
            )))
        }
    }

    async fn toolbox() -> (NotifyUserToolbox, Arc<UserInboxStore>) {
        let store = Arc::new(UserInboxStore::new(
            crate::db::testing::db().await,
            crate::projects::ProjectId::new("1"),
        ));
        let tb = NotifyUserToolbox::new(
            Arc::new(EmptyToolbox),
            store.clone(),
            "session-1".to_string(),
            "main".to_string(),
        );
        (tb, store)
    }

    #[tokio::test]
    async fn adds_notify_user_alongside_inner_specs() {
        let (tb, _) = toolbox().await;
        let names: Vec<String> = tb.specs().into_iter().map(|s| s.name).collect();
        assert_eq!(names, vec![NOTIFY_USER_TOOL.to_string()]);
    }

    /// The whole difference from `ask_user`: this one does not stop the agent.
    /// If it ever answered `StopRun`, every notice would park a run that had no
    /// question to be answered and nothing would resume it.
    #[tokio::test]
    async fn notifying_does_not_stop_the_run() {
        let (tb, _) = toolbox().await;
        let outcome = tb
            .execute(
                NOTIFY_USER_TOOL,
                json!({ "body": "the migration is done" }),
                "tc1",
            )
            .await
            .unwrap();
        assert!(
            matches!(outcome, ToolOutcome::Result(_)),
            "a notice is a plain tool result, never a park: {outcome:?}"
        );
    }

    #[tokio::test]
    async fn the_message_lands_in_the_inbox_addressed_to_its_agent() {
        let (tb, store) = toolbox().await;
        tb.execute(
            NOTIFY_USER_TOOL,
            json!({ "title": "Migration done", "body": "All **42** rows moved." }),
            "tc1",
        )
        .await
        .unwrap();

        let page = store.list(&InboxFilter::default()).await.unwrap();
        let [message] = page.messages.as_slice() else {
            panic!("exactly one notice: {:?}", page.messages);
        };
        assert_eq!(message.title, "Migration done");
        assert_eq!(message.body, "All **42** rows moved.");
        assert_eq!(message.session_id, "session-1");
        assert_eq!(message.agent_id, "main");
        assert!(!message.is_ask(), "a notice is not a question");
        assert!(
            message.tool_call_id.is_none(),
            "nothing is parked on a notice, so it has no call to answer"
        );
        assert_eq!(page.unread, 1, "a new notice is unread");
        assert_eq!(
            page.open_asks, 0,
            "a notice must never count towards the number of stopped agents"
        );
    }

    /// A title is optional, so the list row has to come from somewhere. Markdown
    /// syntax in it would be the model's formatting leaking into the UI.
    #[tokio::test]
    async fn an_untitled_notice_takes_its_subject_from_the_first_line() {
        let (tb, store) = toolbox().await;
        tb.execute(
            NOTIFY_USER_TOOL,
            json!({ "body": "## Fixtures look wrong\n\nTwo of them predate the rename." }),
            "tc1",
        )
        .await
        .unwrap();

        let page = store.list(&InboxFilter::default()).await.unwrap();
        assert_eq!(page.messages[0].title, "Fixtures look wrong");
    }

    /// An empty body is a message that says nothing, and a row that renders as
    /// a blank line. Refused at the door rather than stored.
    #[tokio::test]
    async fn a_blank_message_is_refused() {
        let (tb, store) = toolbox().await;
        let err = tb
            .execute(NOTIFY_USER_TOOL, json!({ "body": "   " }), "tc1")
            .await
            .unwrap_err();
        assert!(matches!(err, ToolCallError::InvalidInput(_)));
        assert!(
            store
                .list(&InboxFilter::default())
                .await
                .unwrap()
                .messages
                .is_empty()
        );
    }

    #[tokio::test]
    async fn delegates_other_calls_to_inner() {
        let (tb, _) = toolbox().await;
        let err = tb.execute("bash", json!({}), "tc1").await.unwrap_err();
        assert!(matches!(err, ToolCallError::InvalidInput(_)));
    }
}
