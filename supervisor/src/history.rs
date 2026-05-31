//! Render a job's "history so far" for `october job logs` by replaying its durable
//! journals. Token-by-token streaming deltas are never journaled, so this reflects
//! coarse messages, tool calls/results, and workflow lifecycle transitions — not
//! the live character stream. Works on any job, including finished ones whose live
//! broadcaster is gone.

use actor::{EventSourcedActor, Journal, PersistenceId};
use agentcore::{ContentPart, Message, Role};
use futures_util::StreamExt;
use models::daemon::JobEventFrame;
use serde_json::Value;
use std::sync::Arc;
use uuid::Uuid;
use workflow::{AgentActor, AgentDomainEvent, AgentState, WorkflowActor, WorkflowDomainEvent};

/// Cap on a rendered tool-result body so a huge output can't flood the log.
const MAX_OUTPUT_CHARS: usize = 500;

/// Replay the job's workflow + agent journals into terminal-friendly log frames.
pub async fn render_history(journal: &Arc<dyn Journal>, job_id: &str) -> Vec<JobEventFrame> {
    let mut lines: Vec<String> = Vec::new();
    for ev in workflow_events(journal, job_id).await {
        match ev {
            WorkflowDomainEvent::WorkflowStarted => lines.push("● workflow started\n".to_string()),
            WorkflowDomainEvent::AgentStarted {
                agent_name,
                session_id,
                ..
            } => {
                lines.push(format!("\n▸ agent {agent_name}\n"));
                lines.extend(agent_session_lines(journal, session_id).await);
            }
            WorkflowDomainEvent::AgentTransitioned {
                from,
                to,
                condition,
                ..
            } => {
                let cond = condition.map(|c| format!(" [{c}]")).unwrap_or_default();
                lines.push(format!("↳ {from} → {to}{cond}\n"));
            }
            WorkflowDomainEvent::WorkflowPaused { .. } => {
                lines.push("⏸ awaiting user input\n".to_string());
            }
            WorkflowDomainEvent::WorkflowResumed => lines.push("▶ resumed\n".to_string()),
            WorkflowDomainEvent::WorkflowSuspended => lines.push("⏸ suspended\n".to_string()),
            WorkflowDomainEvent::WorkflowFinished { output } => {
                lines.push(format!("\n✓ finished: {}\n", compact(&output)));
            }
            WorkflowDomainEvent::WorkflowFailed { error, .. } => {
                lines.push(format!("\n✗ failed: {error}\n"));
            }
        }
    }
    lines
        .into_iter()
        .map(|text| JobEventFrame {
            job_id: job_id.to_string(),
            text,
        })
        .collect()
}

/// All workflow events for a job, in order. The workflow actor does not snapshot
/// (its event log is tiny and kept whole for exactly this), so replaying from the
/// snapshot seq returns the complete history for any run created by this version.
async fn workflow_events(journal: &Arc<dyn Journal>, job_id: &str) -> Vec<WorkflowDomainEvent> {
    let pid = WorkflowActor::persistence_id_for(job_id);
    let seq = snapshot_seq(journal, &pid).await;
    let mut out = Vec::new();
    let mut stream = journal.replay(&pid, seq).await;
    while let Some(item) = stream.next().await {
        if let Ok(bytes) = item
            && let Ok(ev) = serde_json::from_slice::<WorkflowDomainEvent>(&bytes)
        {
            out.push(ev);
        }
    }
    out
}

/// Fold an agent session's journal (snapshot + events) into its conversation and
/// render each message. Agent actors do snapshot, but their message list lives in
/// the snapshotted state, so folding reconstructs the full conversation.
async fn agent_session_lines(journal: &Arc<dyn Journal>, session_id: Uuid) -> Vec<String> {
    let pid = AgentActor::persistence_id_for(session_id);
    let mut state = AgentActor::initial_state();
    let mut seq = 0u64;
    if let Ok(Some((bytes, s))) = journal.latest_snapshot(&pid).await
        && let Ok(snap) = serde_json::from_slice::<AgentState>(&bytes)
    {
        state = snap;
        seq = s;
    }
    let mut stream = journal.replay(&pid, seq).await;
    while let Some(item) = stream.next().await {
        if let Ok(bytes) = item
            && let Ok(ev) = serde_json::from_slice::<AgentDomainEvent>(&bytes)
        {
            state = AgentActor::apply_event(state, ev);
        }
    }
    state.messages.iter().filter_map(render_message).collect()
}

/// The latest snapshot's sequence number, or 0 if none.
async fn snapshot_seq(journal: &Arc<dyn Journal>, pid: &PersistenceId) -> u64 {
    match journal.latest_snapshot(pid).await {
        Ok(Some((_, seq))) => seq,
        Ok(None) | Err(_) => 0,
    }
}

/// Render one conversation message to a log line, or `None` if it carries nothing
/// worth showing (e.g. an empty assistant turn).
fn render_message(m: &Message) -> Option<String> {
    let mut out = String::new();
    for part in &m.parts {
        match part {
            ContentPart::Text(t) => {
                let text = t.text.trim();
                if !text.is_empty() {
                    let prefix = if m.role == Role::User { "» " } else { "" };
                    out.push_str(&format!("{prefix}{text}\n"));
                }
            }
            ContentPart::ToolCall(tc) => {
                out.push_str(&format!("· tool {} {}\n", tc.name, compact(&tc.input)));
            }
            ContentPart::ToolResult(tr) => {
                let tag = if tr.is_error { "error" } else { "ok" };
                out.push_str(&format!("· result [{tag}] {}\n", truncate(&tr.output)));
            }
            ContentPart::Thinking(_) => {}
        }
    }
    (!out.is_empty()).then_some(out)
}

/// Compact a JSON value to a single-line, length-bounded string.
fn compact(value: &Value) -> String {
    truncate(&serde_json::to_string(value).unwrap_or_else(|_| value.to_string()))
}

fn truncate(s: &str) -> String {
    if s.chars().count() <= MAX_OUTPUT_CHARS {
        s.to_string()
    } else {
        let head: String = s.chars().take(MAX_OUTPUT_CHARS).collect();
        format!("{head}… ({} chars)", s.chars().count())
    }
}
