//! `spawn_subsession`: the tool one session hands work to another with.
//!
//! A sub session is a session in its own right — it talks to the user, names
//! itself, and never reports back — so the tool that starts one is named for
//! the thing it makes. It pairs with `spawn_agent`: same shape, one
//! self-contained brief, differing only in where the result goes.
//!
//! Nothing is copied or summarised because the agent calling it already holds
//! the context and can say what matters in a sentence, exactly as it does when
//! spawning a subagent. A copy would hand the sub session the transcript of
//! work it is not doing, and a summary would spend a turn producing one.
//!
//! Layered onto sessions only — the main agent and its sub sessions — because
//! only a session has somewhere to hand work *to*, and never onto an unattended
//! session, whose sub session would have nobody in it.
//!
//! Routes through the session's mailbox exactly as the composer's `/fork` does,
//! so both entry points meet the same guards and write the same event. Nothing
//! here knows how a sub session is seeded.

use crate::sessions::addressing::SessionRef;
use crate::sessions::run_forest::SeedMode;
use crate::sessions::session_actor::{RequestedRuntime, SessionCommand, SubSessionCommand};
use crate::sessions::spec::RuntimeEnv;
use async_trait::async_trait;
use horsie_agentcore::{ToolCallError, ToolOutcome, ToolSpec, Toolbox};
use serde_json::{Value, json};
use std::sync::Arc;
use uuid::Uuid;

/// Name of the built-in sub-session hand-off tool.
pub const SPAWN_SUBSESSION_TOOL: &str = "spawn_subsession";

fn spawn_subsession_spec(environments: &[(String, String)]) -> ToolSpec {
    ToolSpec {
        name: SPAWN_SUBSESSION_TOOL.to_string(),
        description: "Start a sub session — a second session under this one — and hand it \
            a task. The tool form of the `/fork` a person types. Use it when the work splits \
            into a direction the user will want to steer on its own, or when what comes next \
            has little to do with what this session has been about. \
            \n\nThe sub session shares this session's workspace — the same checkout, the \
            same edits — but starts with none of this session's history, so write the task \
            the way you would write one for a subagent: self-contained, including whatever \
            it needs to know. \
            \n\nIt is not a subagent, though. It talks to the user directly and never \
            reports back to you: nothing is delivered here when it finishes. Use this to \
            hand work off, spawn_agent to get an answer back. Returns the sub session's id."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "required": ["task"],
            "properties": {
                "environment": {
                    "type": "string",
                    "description": environment_help(environments)
                },
                "task": {
                    "type": "string",
                    "description": "The complete, self-contained task for the sub \
                        session — what it should do, and everything it needs to know to \
                        start. It is the first thing the sub session reads."
                }
            }
        }),
    }
}

/// What the `environment` parameter accepts, listed the way `invoke_workflow`
/// lists its saved workflows: in prose the model reads to *choose* with, not as
/// a bare enum of names that says nothing about when to pick one.
///
/// The listing is a snapshot from when this turn's toolbox was built; the call
/// re-resolves, so a name gone stale degrades to a clean refusal rather than a
/// wrong sandbox.
fn environment_help(environments: &[(String, String)]) -> String {
    let mut help = "Where the sub session runs. Omit it — the usual case — and \
        the sub session shares this session's runtime and workspace, so it \
        picks up the same checkout and the same uncommitted edits. \
        \n\nPass \"none\" to give it no sandbox at all: no shell, no files, no \
        skills. Only worth it for work that is purely reasoning or purely \
        about calling MCP tools."
        .to_string();
    if environments.is_empty() {
        return help;
    }
    let listing = environments
        .iter()
        .map(|(name, description)| match description.is_empty() {
            true => format!("- {name}"),
            false => format!("- {name}: {description}"),
        })
        .collect::<Vec<_>>()
        .join("\n");
    help.push_str(&format!(
        "\n\nOr name a saved environment to give the sub session a machine of \
         its own, built fresh. It will not see this session's workspace or \
         edits, and it takes minutes to come up — so pick one only when the \
         work genuinely needs a different machine:\n{listing}"
    ));
    help
}

/// Wraps a session's toolbox, adding `spawn_subsession`.
pub struct SubSessionToolbox {
    inner: Arc<dyn Toolbox>,
    session: SessionRef,
    /// The session that branches — the main agent's id is the session's.
    caller: Uuid,
    /// Saved environments as they stood when this toolbox was built: name and
    /// description, for the tool's own prose.
    environments: Vec<(String, String)>,
    /// Resolves a name into something a runtime can be built from. Held rather
    /// than reached through the session, because resolving is a database read
    /// and the session's mailbox must not wait on one.
    resolver: Option<Arc<dyn EnvironmentResolver>>,
}

/// Turns the name a model wrote into the environment a runtime is built from.
///
/// A trait rather than the concrete service so this file — which a model's
/// words reach directly — has no opinion about where environments are stored,
/// and so a test can answer without a database.
#[async_trait]
pub trait EnvironmentResolver: Send + Sync + 'static {
    async fn resolve(&self, name: &str) -> Result<RuntimeEnv, String>;
}

impl SubSessionToolbox {
    pub fn new(
        inner: Arc<dyn Toolbox>,
        session: SessionRef,
        caller: Uuid,
        environments: Vec<(String, String)>,
        resolver: Option<Arc<dyn EnvironmentResolver>>,
    ) -> Self {
        Self {
            inner,
            session,
            caller,
            environments,
            resolver,
        }
    }
}

#[async_trait]
impl Toolbox for SubSessionToolbox {
    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = self.inner.specs();
        specs.push(spawn_subsession_spec(&self.environments));
        specs
    }

    async fn execute(
        &self,
        name: &str,
        input: Value,
        tool_call_id: &str,
    ) -> Result<ToolOutcome, ToolCallError> {
        if name != SPAWN_SUBSESSION_TOOL {
            return self.inner.execute(name, input, tool_call_id).await;
        }
        let task = input
            .get("task")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolCallError::InvalidInput("missing 'task'".to_string()))?;
        // Resolved here, on this agent's own task: a name reaches a database,
        // and the session's mailbox must not wait on one. A name that does not
        // resolve is a refusal the model reads and can correct, not a sub
        // session built somewhere unintended.
        // Three answers, one variant each. Omitting the parameter must not
        // mean "no sandbox": a model that just wants to hand work off writes no
        // `environment`, and silently taking its filesystem away is the
        // opposite of what it asked for.
        let env = match input.get("environment").and_then(Value::as_str) {
            None => RequestedRuntime::Inherit,
            // The one name that resolves to nothing rather than failing: an
            // explicit "no sandbox" is an answer, not a missing environment.
            Some("none") => RequestedRuntime::Without,
            Some(name) => {
                let resolver = self.resolver.as_ref().ok_or_else(|| {
                    ToolCallError::ExecutionFailed(
                        "this server has no saved environments".to_string(),
                    )
                })?;
                RequestedRuntime::Own(Box::new(resolver.resolve(name).await.map_err(|e| {
                    ToolCallError::InvalidInput(format!("no environment named '{name}': {e}"))
                })?))
            }
        };
        let id = self
            .session
            .ask(|reply| {
                SessionCommand::SubSession(SubSessionCommand::Create {
                    parent: self.caller,
                    seed: SeedMode::Fresh,
                    message: task.to_string(),
                    env: env.clone(),
                    reply,
                })
            })
            .await
            .map_err(|e| ToolCallError::ExecutionFailed(e.to_string()))?
            .map_err(ToolCallError::ExecutionFailed)?;
        // What it got, said plainly: a model that asked for a machine of its
        // own and silently received a shared one would go on to give the sub
        // session instructions that assume a fresh checkout.
        let where_it_runs = match env {
            RequestedRuntime::Inherit => "It shares this session's workspace.".to_string(),
            RequestedRuntime::Without => "It has no sandbox: no shell, no files.".to_string(),
            RequestedRuntime::Own(env) => format!(
                "It is building a machine of its own on '{}'; its first turn waits for that.",
                env.vendor
            ),
        };
        Ok(ToolOutcome::Result(Value::String(format!(
            "Started a sub session: {id}. {where_it_runs} It carries on with the user from \
             here; you will hear nothing back from it."
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
    use horsie_actor::{
        ActorContext, CommandEffect, EventSourcedActor, InMemoryJournal, PersistenceId,
    };
    use horsie_agentcore::EmptyToolbox;
    use serde::{Deserialize, Serialize};
    use std::sync::Mutex;

    #[derive(Serialize, Deserialize, Default)]
    struct Empty;

    /// What the stub records of each `Create` it answered: who asked, how the
    /// sub session is seeded, the message, and where it was told to run.
    type Created = Arc<Mutex<Vec<(Uuid, SeedMode, String, String)>>>;

    /// Answers `Create` the way the session will, and records what it was
    /// asked for — the tool's whole job is to turn a call into that command.
    struct StubSession {
        result: Result<Uuid, String>,
        seen: Created,
    }

    #[async_trait::async_trait]
    impl EventSourcedActor for StubSession {
        type Command = SessionInbox;
        type Event = ();
        type State = Empty;

        fn persistence_id(&self) -> PersistenceId {
            PersistenceId::new("sub_session-tool-test", "stub")
        }
        fn initial_state() -> Empty {
            Empty
        }
        fn apply_event(state: Empty, (): ()) -> Empty {
            state
        }
        async fn handle_command(
            &mut self,
            _state: &Empty,
            cmd: SessionInbox,
            _ctx: &mut ActorContext<SessionInbox>,
        ) -> CommandEffect<()> {
            if let SessionCommand::SubSession(SubSessionCommand::Create {
                parent,
                seed,
                message,
                env,
                reply,
            }) = cmd.cmd
            {
                self.seen.lock().unwrap().push((
                    parent,
                    seed,
                    message,
                    // The variant itself, not a flattened vendor: telling
                    // `Inherit` from `Without` is the whole point of the
                    // type, so a test that collapsed them could not catch
                    // the flaw it exists to prevent.
                    match env {
                        RequestedRuntime::Inherit => "inherit".to_string(),
                        RequestedRuntime::Without => "without".to_string(),
                        RequestedRuntime::Own(e) => format!("own:{}", e.vendor),
                    },
                ));
                let _ = reply.send(self.result.clone());
            }
            CommandEffect::none()
        }
    }

    struct Harness {
        toolbox: SubSessionToolbox,
        caller: Uuid,
        /// What the session was asked for: who branched, how it is seeded, the
        /// brief, and which of the three runtime answers it carried.
        seen: Created,
    }

    /// Answers any name with a runtime on a vendor called after it, so a test
    /// can tell which environment a call resolved without a database.
    struct StubEnvironments;

    #[async_trait]
    impl EnvironmentResolver for StubEnvironments {
        async fn resolve(&self, name: &str) -> Result<RuntimeEnv, String> {
            if name == "missing" {
                return Err("unknown environment 'missing'".to_string());
            }
            Ok(RuntimeEnv {
                vendor: format!("vendor-for-{name}"),
                workspaces: Vec::new(),
                provision: Vec::new(),
                env_vars: Vec::new(),
                environment: Some(name.to_string()),
            })
        }
    }

    fn harness(result: Result<Uuid, String>) -> Harness {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let session = SessionRef::new(
            crate::testing::spawn_detached(
                &horsie_actor::ActorSystem::new(Arc::new(InMemoryJournal::new())),
                StubSession {
                    result,
                    seen: Arc::clone(&seen),
                },
            ),
            crate::projects::ProjectId::generate(),
            Uuid::new_v4(),
            None,
        );
        let caller = Uuid::new_v4();
        Harness {
            toolbox: SubSessionToolbox::new(
                Arc::new(EmptyToolbox),
                session,
                caller,
                vec![("staging".to_string(), "a throwaway copy".to_string())],
                Some(Arc::new(StubEnvironments) as Arc<dyn EnvironmentResolver>),
            ),
            caller,
            seen,
        }
    }

    #[tokio::test]
    async fn specs_advertise_the_tool() {
        let names: Vec<String> = harness(Ok(Uuid::new_v4()))
            .toolbox
            .specs()
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(names, vec![SPAWN_SUBSESSION_TOOL.to_string()]);
    }

    /// Two things a model must not take from this tool: that a sub session
    /// reports back the way a subagent does, and that it can see what was said
    /// here.
    #[test]
    fn the_description_says_nothing_comes_back_and_nothing_goes_with_it() {
        let spec = spawn_subsession_spec(&[]);
        assert!(spec.description.contains("never reports back"), "{spec:?}");
        assert!(spec.description.contains("spawn_agent"), "{spec:?}");
        assert!(
            spec.description.contains("none of this session's history"),
            "{spec:?}"
        );
        assert!(spec.description.contains("self-contained"), "{spec:?}");
        // The name says "subsession", not "fork", so the description is the
        // only place a model learns this is what `/fork` asks for.
        assert!(spec.description.contains("`/fork`"), "{spec:?}");
    }

    /// A sub session is attributed to the session that asked for it, not to the
    /// root — which is what makes sub sessions nest. And it is always `Fresh`:
    /// the tool has no way to ask for a copy, because the brief it carries is
    /// the point.
    #[tokio::test]
    async fn a_call_hands_a_brief_to_a_sub_session_of_the_caller() {
        let h = harness(Ok(Uuid::new_v4()));
        let out = h
            .toolbox
            .execute(
                SPAWN_SUBSESSION_TOOL,
                json!({"task": "try the other migration"}),
                "tc1",
            )
            .await
            .unwrap();
        assert_eq!(
            *h.seen.lock().unwrap(),
            vec![(
                h.caller,
                SeedMode::Fresh,
                "try the other migration".to_string(),
                // Omitted, so it inherits.
                "inherit".to_string()
            )]
        );
        match out {
            ToolOutcome::Result(Value::String(text)) => {
                assert!(text.contains("hear nothing back"), "{text}");
            }
            other => panic!("{other:?}"),
        }
    }

    /// The three answers the `environment` parameter can give, and the fact
    /// that they are three rather than two.
    ///
    /// Omitting it must not mean "no sandbox": a model that just wants to hand
    /// work off writes no `environment`, and silently taking its filesystem
    /// away would break the sub session in a way neither party asked for.
    #[tokio::test]
    async fn omitting_the_environment_inherits_and_naming_none_does_not() {
        let h = harness(Ok(Uuid::new_v4()));
        h.toolbox
            .execute(SPAWN_SUBSESSION_TOOL, json!({"task": "go"}), "tc1")
            .await
            .unwrap();
        h.toolbox
            .execute(
                SPAWN_SUBSESSION_TOOL,
                json!({"task": "think", "environment": "none"}),
                "tc2",
            )
            .await
            .unwrap();
        h.toolbox
            .execute(
                SPAWN_SUBSESSION_TOOL,
                json!({"task": "build", "environment": "staging"}),
                "tc3",
            )
            .await
            .unwrap();

        let seen = h.seen.lock().unwrap();
        // Both of the first two carry no environment to the session, and they
        // are still different asks: the session tells them apart by whether the
        // sub session was pointed at a runtime of its own.
        // Three distinct answers reach the session. Omitting the parameter and
        // asking for "none" must not arrive as the same thing: the first shares
        // the parent's sandbox, the second has none, and a model that omitted
        // it did not ask to lose its filesystem.
        assert_eq!(seen[0].3, "inherit", "omitted inherits");
        assert_eq!(seen[1].3, "without", "\"none\" asks for no sandbox");
        assert_eq!(
            seen[2].3, "own:vendor-for-staging",
            "a named environment is resolved before the session is asked"
        );
    }

    /// And the model is told which of the three it got. A model that asked for
    /// a machine of its own and silently received a shared one would go on to
    /// brief the sub session as though it had a fresh checkout.
    #[tokio::test]
    async fn the_result_says_where_the_sub_session_runs() {
        let h = harness(Ok(Uuid::new_v4()));
        let text = |out: ToolOutcome| match out {
            ToolOutcome::Result(Value::String(t)) => t,
            other => panic!("{other:?}"),
        };
        let inherited = text(
            h.toolbox
                .execute(SPAWN_SUBSESSION_TOOL, json!({"task": "go"}), "tc1")
                .await
                .unwrap(),
        );
        assert!(
            inherited.contains("shares this session's workspace"),
            "{inherited}"
        );

        let none = text(
            h.toolbox
                .execute(
                    SPAWN_SUBSESSION_TOOL,
                    json!({"task": "think", "environment": "none"}),
                    "tc2",
                )
                .await
                .unwrap(),
        );
        assert!(none.contains("no sandbox"), "{none}");

        let own = text(
            h.toolbox
                .execute(
                    SPAWN_SUBSESSION_TOOL,
                    json!({"task": "build", "environment": "staging"}),
                    "tc3",
                )
                .await
                .unwrap(),
        );
        assert!(own.contains("vendor-for-staging"), "{own}");
        assert!(own.contains("waits"), "the wait has to be said: {own}");
    }

    /// A name that does not resolve is a refusal the model reads and can
    /// correct — never a sub session quietly built somewhere else.
    #[tokio::test]
    async fn an_unknown_environment_refuses_before_anything_is_created() {
        let h = harness(Ok(Uuid::new_v4()));
        let err = h
            .toolbox
            .execute(
                SPAWN_SUBSESSION_TOOL,
                json!({"task": "go", "environment": "missing"}),
                "tc1",
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolCallError::InvalidInput(ref m) if m.contains("missing")),
            "{err:?}"
        );
        assert!(
            h.seen.lock().unwrap().is_empty(),
            "nothing may be created when the environment did not resolve"
        );
    }

    /// The saved environments ride in the description, the way
    /// `invoke_workflow` carries its catalogue — a bare name parameter says
    /// nothing about when to pick one.
    #[test]
    fn the_description_lists_the_environments_and_says_omitting_is_normal() {
        let listed = spawn_subsession_spec(&[("staging".into(), "a throwaway copy".into())]);
        let help = listed.input_schema["properties"]["environment"]["description"]
            .as_str()
            .expect("the environment parameter documents itself");
        assert!(help.contains("staging: a throwaway copy"), "{help}");
        assert!(help.contains("Omit it"), "{help}");
        assert!(help.contains("\"none\""), "{help}");

        // With none saved, the listing is absent rather than an empty heading.
        let bare = spawn_subsession_spec(&[]);
        let help = bare.input_schema["properties"]["environment"]["description"]
            .as_str()
            .unwrap();
        assert!(!help.contains("saved environment"), "{help}");
    }

    #[tokio::test]
    async fn a_missing_task_is_rejected() {
        let h = harness(Ok(Uuid::new_v4()));
        let err = h
            .toolbox
            .execute(SPAWN_SUBSESSION_TOOL, json!({}), "tc1")
            .await
            .unwrap_err();
        assert!(matches!(err, ToolCallError::InvalidInput(_)));
        assert!(h.seen.lock().unwrap().is_empty());
    }

    /// The session's refusal — a workflow run, a subagent caller — reaches the
    /// model verbatim rather than as a generic failure.
    #[tokio::test]
    async fn the_sessions_refusal_is_what_the_model_reads() {
        let h = harness(Err("a workflow run cannot be branched".to_string()));
        let err = h
            .toolbox
            .execute(SPAWN_SUBSESSION_TOOL, json!({"task": "go"}), "tc1")
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolCallError::ExecutionFailed(ref m) if m == "a workflow run cannot be branched")
        );
    }

    #[tokio::test]
    async fn other_tools_pass_through() {
        let h = harness(Ok(Uuid::new_v4()));
        let err = h
            .toolbox
            .execute("read_file", json!({}), "tc1")
            .await
            .unwrap_err();
        assert!(!matches!(err, ToolCallError::InvalidInput(ref m) if m.contains("task")));
        assert!(h.seen.lock().unwrap().is_empty());
    }
}
