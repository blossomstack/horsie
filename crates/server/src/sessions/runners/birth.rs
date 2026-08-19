//! Where a runner's slice comes from: one [`RunnerArgs`], one [`RunnerState`].
//!
//! The one seam at which the session's vocabulary becomes a runner's own, and a
//! free function rather than a method on either side — `RunnerArgs` is the
//! capability's word for what it wants, `RunnerState` is the journal's word for
//! what exists, and neither should have to know the other's shape.
//!
//! Nothing here mints an id or reads a clock. The capability that asked already
//! minted the ids — that is what lets `spawn_agent` answer the model before the
//! child has been equipped — and the timestamp is a fact about the journal
//! entry, stamped by the session when it persists.

use super::RunnerState;
use super::action::{RunnerArgs, WorkflowSource};
use super::ids::RunnerId;
use super::{conversation, runtime, subagent, workflow};
use crate::agent_loop::UsageTotal;
use crate::agent_loop::capabilities::Capabilities;

/// The slice a freshly created runner starts life with.
///
/// `Err` for the one case that cannot be answered here: a workflow named rather
/// than given. Turning a name into a graph is a database read, and a database
/// read may not happen on the session mailbox, so the session resolves it on a
/// detached task and calls again with [`WorkflowSource::Graph`].
pub fn born(
    args: RunnerArgs,
    capabilities: Capabilities,
    run: RunnerId,
) -> Result<RunnerState, String> {
    Ok(match args {
        RunnerArgs::SubAgent {
            agent,
            label,
            task,
            agent_type,
            settings,
        } => RunnerState::SubAgent(subagent::State {
            agent,
            started: false,
            label,
            task,
            agent_type,
            settings: *settings,
            usage: UsageTotal::default(),
            result: None,
            reported: false,
            capabilities,
        }),
        RunnerArgs::Conversation {
            agent,
            seed,
            message,
            settings,
        } => RunnerState::Conversation(conversation::State {
            agent,
            // A fork waits for its branch to land; the session's own
            // conversation has nothing to wait for and is seeded by
            // construction. One field, so `actions()` asks one question.
            seeded: seed.is_none(),
            seed,
            started: false,
            turn: conversation::TurnStatus::default(),
            title: None,
            first_message: (!message.is_empty()).then_some(message),
            settings: *settings,
            usage: UsageTotal::default(),
            last_error: None,
            last_activity_ms: 0,
            capabilities,
        }),
        RunnerArgs::Workflow { source, input: _ } => match source {
            WorkflowSource::Named(name) => {
                return Err(format!(
                    "workflow {name} has to be resolved to a graph before its runner is created"
                ));
            }
            WorkflowSource::Graph(graph) => RunnerState::Workflow(Box::new(workflow::State {
                run,
                graph,
                steps: Vec::new(),
                status: crate::sessions::workflow::WorkflowRunStatus::default(),
                output: None,
                error: None,
                usage: UsageTotal::default(),
                step_usage: std::collections::BTreeMap::new(),
                reported: false,
                capabilities,
            })),
        },
    })
}

/// The sandbox's slice. Not born from args, because nobody's agent asks for it:
/// it is created with the session, from the session's own spec.
#[must_use]
pub fn runtime_born() -> RunnerState {
    RunnerState::Runtime(runtime::State::default())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::super::action::{Branch, ForkMode};
    use super::super::ids::AgentId;
    use super::*;
    use std::sync::Arc;

    fn settings() -> crate::sessions::spec::AgentSettings {
        crate::sessions::runners::empty_settings()
    }

    /// A worker's task is also its first input, so it is stored verbatim: a
    /// restart before the agent existed re-sends exactly what was asked for.
    #[test]
    fn a_subagent_keeps_its_task_verbatim() {
        let agent = AgentId::new_v4();
        let args = RunnerArgs::SubAgent {
            agent,
            label: "read the flake".into(),
            task: "  find why it fails  ".into(),
            agent_type: None,
            settings: Box::new(settings()),
        };
        let state = born(args, Capabilities::default(), RunnerId::new_v4()).unwrap();
        let RunnerState::SubAgent(s) = state else {
            panic!("expected a subagent")
        };
        assert_eq!(s.agent, agent);
        assert_eq!(s.task, "  find why it fails  ");
        assert_eq!(s.label, "read the flake");
        assert!(!s.started, "creation starts nothing; actions() does");
        assert!(s.result.is_none());
    }

    /// A fork is a conversation with a branch point, and the message it was
    /// created with is its first input rather than a title.
    #[test]
    fn a_fork_is_a_conversation_carrying_its_branch() {
        let agent = AgentId::new_v4();
        let branch = Branch {
            source: AgentId::new_v4(),
            source_seq: 42,
            mode: ForkMode::Copy,
        };
        let args = RunnerArgs::Conversation {
            agent,
            seed: Some(branch.clone()),
            message: "carry on from here".into(),
            settings: Box::new(settings()),
        };
        let state = born(args, Capabilities::default(), RunnerId::new_v4()).unwrap();
        let RunnerState::Conversation(s) = state else {
            panic!("expected a conversation")
        };
        assert_eq!(s.seed, Some(branch));
        assert!(!s.seeded, "a fork is not seeded until its seed lands");
        assert_eq!(s.first_message.as_deref(), Some("carry on from here"));
        assert!(
            s.title.is_none(),
            "a fork names itself later, or not at all"
        );
    }

    /// The session's own conversation: no branch, nothing waiting to be seeded,
    /// so `actions()` may start it as soon as the runtime is ready.
    #[test]
    fn the_root_conversation_has_no_branch_and_is_already_seeded() {
        let args = RunnerArgs::Conversation {
            agent: AgentId::new_v4(),
            seed: None,
            message: String::new(),
            settings: Box::new(settings()),
        };
        let state = born(args, Capabilities::default(), RunnerId::new_v4()).unwrap();
        let RunnerState::Conversation(s) = state else {
            panic!("expected a conversation")
        };
        assert!(s.seed.is_none());
        assert!(
            s.seeded,
            "nothing has to land before the session's own agent runs"
        );
        assert_eq!(
            s.first_message, None,
            "an empty create message is no message"
        );
    }

    /// The graph is journal data on the runner, which is what makes an ad-hoc
    /// run — a graph with no definition row and no name — expressible.
    #[test]
    fn a_workflow_snapshots_the_graph_it_was_given() {
        let run = RunnerId::new_v4();
        let graph = Arc::new(crate::sessions::workflow::WorkflowRunSpec {
            workflow: "nightly".into(),
            start: "first".into(),
            steps: Vec::new(),
            input: "go".into(),
            max_steps: 8,
        });
        let args = RunnerArgs::Workflow {
            source: WorkflowSource::Graph(Arc::clone(&graph)),
            input: "go".into(),
        };
        let state = born(args, Capabilities::default(), run).unwrap();
        let RunnerState::Workflow(s) = state else {
            panic!("expected a workflow")
        };
        assert_eq!(s.run, run, "a run's slice names the runner it is");
        assert!(Arc::ptr_eq(&s.graph, &graph));
        assert!(s.steps.is_empty(), "no step has run yet");
    }

    /// A name is not a graph. Resolving one is a database read, and a database
    /// read may not happen on the session mailbox.
    #[test]
    fn a_named_workflow_is_refused_because_resolving_is_not_this_function() {
        let args = RunnerArgs::Workflow {
            source: WorkflowSource::Named("nightly".into()),
            input: "go".into(),
        };
        let err = born(args, Capabilities::default(), RunnerId::new_v4()).unwrap_err();
        assert!(
            err.contains("nightly"),
            "the error names what was unresolved: {err}"
        );
    }
}
