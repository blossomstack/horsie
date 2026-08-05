//! Hooks the server initiates, which the runtime runs because the runtime is
//! where the plugin files are.
//!
//! One function for every such event: the invocation carries what the event
//! needs, [`super::matching`] finds the declarations whose matcher selects it,
//! and [`super::run_one`] runs each. No per-event branching beyond building the
//! invocation — which is the whole reason `SessionStart`'s bespoke RPC was
//! worth replacing rather than duplicating.

use horsie_models::hooks::HookRecord;
use horsie_models::runtime::ServerHookEvent;
use horsie_support::plugin::hooks::HookInvocation;

use crate::workspace::WorkspaceRegistry;

/// Run every hook matching `event`, in stable plugin order.
///
/// Empty when there is no plugin library or nothing declares the event —
/// including, deliberately, when a hook failed: a failure is a record, not an
/// omission.
pub async fn run_hooks(registry: &WorkspaceRegistry, event: &ServerHookEvent) -> Vec<HookRecord> {
    let Some(plugins_dir) = registry.plugins_dir() else {
        return Vec::new();
    };
    let invocation = match event {
        ServerHookEvent::SessionStart(i) => HookInvocation::SessionStart { source: &i.source },
        ServerHookEvent::SubagentStart(i) => HookInvocation::SubagentStart {
            agent_type: &i.agent_type,
        },
        ServerHookEvent::UserPromptSubmit(i) => {
            HookInvocation::UserPromptSubmit { prompt: &i.prompt }
        }
        ServerHookEvent::Stop(i) => HookInvocation::Stop {
            last_assistant_message: i.last_assistant_message.as_deref(),
            stop_hook_active: i.stop_hook_active,
        },
    };
    let hook_path = registry.hook_path();
    let subjects = invocation.matcher_subjects();
    let mut records = Vec::new();
    for (root, plugin, decl) in super::matching(plugins_dir, invocation.event(), &subjects) {
        let (_, record) = super::run_one(&root, &plugin, &decl, hook_path, invocation).await;
        records.push(record);
    }
    records
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
    use crate::hooks::tool::tests::{env, plugin};
    use horsie_models::Workspace;
    use horsie_models::hooks::{HookAction, SessionStartOutcome, StopOutcome};
    use horsie_models::runtime::{SessionStartInput, StopInput};
    use tempfile::TempDir;

    fn start(source: &str) -> ServerHookEvent {
        ServerHookEvent::SessionStart(SessionStartInput {
            source: source.to_string(),
        })
    }

    /// The asymmetry this closes: `SessionStart` used to return a bare string
    /// and produce no record at all, so "every hook that runs is recorded" was
    /// already untrue for it.
    #[tokio::test]
    async fn a_session_start_hook_produces_a_record_and_its_context() {
        let plugins = TempDir::new().unwrap();
        plugin(
            plugins.path(),
            "boot",
            "SessionStart",
            "",
            "echo CONVENTIONS",
        );
        let e = env(plugins);
        let records = run_hooks(&e.registry, &start("startup")).await;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].plugin, "boot");
        match &records[0].action {
            HookAction::SessionStart(r) => {
                assert_eq!(r.source, "startup");
                match &r.outcome {
                    SessionStartOutcome::Ran(c) => {
                        assert_eq!(c.additional_context.as_deref(), Some("CONVENTIONS"));
                    }
                    other => panic!("{other:?}"),
                }
            }
            other => panic!("{other:?}"),
        }
    }

    /// A `SessionStart` matcher selects on `source`, not on a tool name. This is
    /// the generalisation `matcher_applies` could not express.
    #[tokio::test]
    async fn a_source_matcher_selects_the_right_start() {
        let plugins = TempDir::new().unwrap();
        plugin(
            plugins.path(),
            "boot",
            "SessionStart",
            "resume",
            "echo ONLY_ON_RESUME",
        );
        let e = env(plugins);
        assert!(run_hooks(&e.registry, &start("startup")).await.is_empty());
        assert_eq!(run_hooks(&e.registry, &start("resume")).await.len(), 1);
    }

    /// A failing hook is recorded rather than dropped — the old path logged it
    /// and returned `None`, so nothing downstream could ever see it.
    #[tokio::test]
    async fn a_failing_session_start_hook_is_recorded_as_failed() {
        let plugins = TempDir::new().unwrap();
        plugin(
            plugins.path(),
            "boot",
            "SessionStart",
            "",
            "echo nope 1>&2; exit 1",
        );
        let e = env(plugins);
        let records = run_hooks(&e.registry, &start("startup")).await;
        match &records[0].action {
            HookAction::SessionStart(r) => {
                assert!(matches!(r.outcome, SessionStartOutcome::Failed(_)));
            }
            other => panic!("{other:?}"),
        }
    }

    /// `Stop` blocking is *blocked from stopping*, so the runtime records a
    /// block. What that then does to the turn is the server's decision.
    #[tokio::test]
    async fn a_blocking_stop_hook_records_a_block() {
        let plugins = TempDir::new().unwrap();
        plugin(
            plugins.path(),
            "stopper",
            "Stop",
            "",
            "echo 'tests still failing' 1>&2; exit 2",
        );
        let e = env(plugins);
        let records = run_hooks(
            &e.registry,
            &ServerHookEvent::Stop(StopInput {
                last_assistant_message: Some("done".into()),
                stop_hook_active: false,
            }),
        )
        .await;
        match &records[0].action {
            HookAction::Stop(r) => match &r.outcome {
                StopOutcome::Blocked(b) => {
                    assert_eq!(b.reason.as_deref(), Some("tests still failing"));
                }
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn with_no_plugin_library_nothing_runs_and_nothing_is_recorded() {
        let work = TempDir::new().unwrap();
        let registry = WorkspaceRegistry::new(vec![Workspace {
            name: "main".into(),
            path: work.path().to_path_buf(),
        }]);
        assert!(run_hooks(&registry, &start("startup")).await.is_empty());
    }
}
