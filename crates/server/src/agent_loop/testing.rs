//! The shared harness every module's tests are built on.
//!
//! Split out because the tests do not partition the way the code does. They are
//! organised around *scenario setup* — does this test need a live actor, a
//! journal, a provider that never answers, a channel to hear the parent on —
//! rather than around the unit under test, so the fence tests and the queue
//! tests share `OutcomeChannel` and four modules' fold tests share `user_msg`.
//!
//! Keeping the harness here is what lets a test live beside the module it
//! covers instead of beside the fixture it happens to reuse. Anything used by
//! more than one test module belongs in this file; anything used by exactly one
//! belongs next to it.

#![allow(dead_code)]

use crate::agent_loop::AgentRunDef;
use crate::agent_loop::context::{
    AgentOutcome, AgentOutcomeSink, ContextError, ContextProvider, Contexts,
};
use crate::agent_loop::prelude::*;
use async_trait::async_trait;
use horsie_agentcore::{ContentPart, Message, Role};
use horsie_models::agent::TextPart;

// Shared no-op collaborators for tests that only exercise the actor's own
// bookkeeping and never start a run.
pub(crate) struct StubContext;
#[async_trait]
impl crate::agent_loop::ContextProvider for StubContext {
    async fn provide(
        &self,
    ) -> Result<crate::agent_loop::Contexts, crate::agent_loop::ContextError> {
        Err(crate::agent_loop::ContextError::retryable("no context"))
    }
}
pub(crate) struct StubParent;
#[async_trait]
impl AgentOutcomeSink for StubParent {
    async fn deliver(&self, _: AgentOutcome) {}
}

pub(crate) fn user_msg(text: &str) -> Message {
    Message {
        created_at_ms: 0,
        started_at_ms: None,
        id: "u".into(),
        role: Role::User,
        parts: vec![ContentPart::Text(TextPart { text: text.into() })],
    }
}

pub(crate) fn def_fixture() -> AgentRunDef {
    AgentRunDef {
        system_prompt: None,
        max_iterations: None,
        max_retries: None,
        allowed_tools: None,
    }
}

/// Hears everything an agent reports to whoever spawned it.
pub(crate) struct OutcomeChannel(pub(crate) tokio::sync::mpsc::UnboundedSender<AgentOutcome>);

#[async_trait]
impl AgentOutcomeSink for OutcomeChannel {
    async fn deliver(&self, outcome: AgentOutcome) {
        let _ = self.0.send(outcome);
    }
}

pub(crate) type Outcomes = tokio::sync::mpsc::UnboundedReceiver<AgentOutcome>;

/// A context that never returns, so a run stays genuinely in flight — or, for
/// the recovery tests, is never asked at all.
pub(crate) struct HangingContext;

#[async_trait]
impl ContextProvider for HangingContext {
    async fn provide(&self) -> Result<Contexts, ContextError> {
        std::future::pending().await
    }
}

pub(crate) fn hook_record(plugin: &str, call: &str) -> horsie_models::hooks::HookRecord {
    horsie_models::hooks::HookRecord {
        plugin: plugin.to_string(),
        duration_ms: 3,
        halt: None,
        action: horsie_models::hooks::HookAction::PreToolUse(
            horsie_models::hooks::PreToolUseRecord {
                call: horsie_models::hooks::ToolScope {
                    tool: "bash".to_string(),
                    tool_call_id: call.to_string(),
                },
                system_message: None,
                outcome: horsie_models::hooks::PreToolUseOutcome::Denied(
                    horsie_models::hooks::HookDenied {
                        reason: Some("not allowed".into()),
                    },
                ),
            },
        ),
    }
}

pub(crate) fn with_hook(state: AgentState, plugin: &str, call: &str, seq: usize) -> AgentState {
    AgentActor::apply_event(
        state,
        AgentDomainEvent::HookRan {
            record: hook_record(plugin, call),
            seq,
            at_ms: 5,
        },
    )
}
