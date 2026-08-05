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
        ServerHookEvent::SessionStart(i) => HookInvocation::SessionStart {
            source: i.source.as_wire(),
        },
        ServerHookEvent::SubagentStart(i) => HookInvocation::SubagentStart {
            agent_id: &i.agent_id,
            agent_type: &i.agent_type,
        },
        ServerHookEvent::UserPromptSubmit(i) => {
            HookInvocation::UserPromptSubmit { prompt: &i.prompt }
        }
        ServerHookEvent::UserPromptExpansion(i) => HookInvocation::UserPromptExpansion {
            prompt: &i.prompt,
            command: &i.command,
        },
        ServerHookEvent::Stop(i) => HookInvocation::Stop {
            last_assistant_message: i.last_assistant_message.as_deref(),
            stop_hook_active: i.stop_hook_active,
        },
        ServerHookEvent::SubagentStop(i) => HookInvocation::SubagentStop {
            agent_id: &i.agent_id,
            agent_type: &i.agent_type,
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
    use horsie_models::runtime::{SessionStartInput, SessionStartSource, StopInput};
    use tempfile::TempDir;

    fn start(source: SessionStartSource) -> ServerHookEvent {
        ServerHookEvent::SessionStart(SessionStartInput { source })
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
        let records = run_hooks(&e.registry, &start(SessionStartSource::Startup)).await;
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
        assert!(
            run_hooks(&e.registry, &start(SessionStartSource::Startup))
                .await
                .is_empty()
        );
        assert_eq!(
            run_hooks(&e.registry, &start(SessionStartSource::Resume))
                .await
                .len(),
            1
        );
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
        let records = run_hooks(&e.registry, &start(SessionStartSource::Startup)).await;
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
        assert!(
            run_hooks(&registry, &start(SessionStartSource::Startup))
                .await
                .is_empty()
        );
    }

    /// A subagent's stop is its own event on the wire, carrying the agent type
    /// its matcher selects on.
    #[tokio::test]
    async fn a_subagent_stop_hook_matches_on_the_agent_type() {
        let plugins = TempDir::new().unwrap();
        plugin(
            plugins.path(),
            "reviewer-guard",
            "SubagentStop",
            "reviewer",
            "echo CHECKED",
        );
        let e = env(plugins);
        let event = |agent_type: &str| {
            ServerHookEvent::SubagentStop(horsie_models::runtime::SubagentStopInput {
                agent_id: "sub-1".to_string(),
                agent_type: agent_type.to_string(),
                last_assistant_message: None,
                stop_hook_active: false,
            })
        };
        assert!(
            run_hooks(&e.registry, &event("researcher"))
                .await
                .is_empty()
        );
        let records = run_hooks(&e.registry, &event("reviewer")).await;
        assert_eq!(records.len(), 1);
        match &records[0].action {
            HookAction::SubagentStop(r) => assert_eq!(r.agent_type, "reviewer"),
            other => panic!("{other:?}"),
        }
    }

    // --- HTTP hooks ---
    //
    // Same records, same processing, a different way of getting there. The one
    // thing the transport changes is that there is no exit code, so an HTTP
    // hook can block only through its body.

    /// A one-request HTTP server answering `status` with `body`. Hand-rolled
    /// rather than pulling in a server crate for four tests — the runtime is
    /// already a `reqwest` client and needs nothing else.
    async fn one_shot_server(status: &'static str, body: &'static str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buf = [0u8; 4096];
            let _ = socket.read(&mut buf).await;
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        });
        format!("http://{addr}/hook")
    }

    fn http_plugin(plugins: &std::path::Path, name: &str, event: &str, url: &str) {
        let dir = plugins.join(name);
        std::fs::create_dir_all(dir.join("hooks")).unwrap();
        std::fs::write(
            dir.join("hooks/hooks.json"),
            format!(
                r#"{{"hooks":{{"{event}":[{{"hooks":[
                     {{"type":"http","url":"{url}","timeout":5}}]}}]}}}}"#
            ),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn an_http_hook_records_what_its_body_said() {
        let url = one_shot_server(
            "200 OK",
            r#"{"hookSpecificOutput":{"additionalContext":"FROM THE WEB"}}"#,
        )
        .await;
        let plugins = TempDir::new().unwrap();
        http_plugin(plugins.path(), "webhook", "SessionStart", &url);
        let e = env(plugins);
        let records = run_hooks(&e.registry, &start(SessionStartSource::Startup)).await;
        assert_eq!(records.len(), 1);
        match &records[0].action {
            HookAction::SessionStart(r) => match &r.outcome {
                SessionStartOutcome::Ran(c) => {
                    assert_eq!(c.additional_context.as_deref(), Some("FROM THE WEB"));
                }
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
    }

    /// An HTTP hook blocks through its body, because there is no exit code to
    /// carry a 2. A `Stop` that blocks still continues the turn.
    #[tokio::test]
    async fn an_http_hook_blocks_through_its_body() {
        let url = one_shot_server("200 OK", r#"{"decision":"block","reason":"not yet"}"#).await;
        let plugins = TempDir::new().unwrap();
        http_plugin(plugins.path(), "webhook", "Stop", &url);
        let e = env(plugins);
        let records = run_hooks(
            &e.registry,
            &ServerHookEvent::Stop(StopInput {
                last_assistant_message: None,
                stop_hook_active: false,
            }),
        )
        .await;
        match &records[0].action {
            HookAction::Stop(r) => match &r.outcome {
                StopOutcome::Blocked(b) => assert_eq!(b.reason.as_deref(), Some("not yet")),
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
    }

    /// A non-2xx is an outage, never a refusal: the hook had its chance to
    /// refuse in the body, and a 500 means it never got that far.
    #[tokio::test]
    async fn a_failing_status_is_an_outage_not_a_decision() {
        let url = one_shot_server("500 Internal Server Error", "boom").await;
        let plugins = TempDir::new().unwrap();
        http_plugin(plugins.path(), "webhook", "SessionStart", &url);
        let e = env(plugins);
        let records = run_hooks(&e.registry, &start(SessionStartSource::Startup)).await;
        match &records[0].action {
            HookAction::SessionStart(r) => match &r.outcome {
                SessionStartOutcome::Failed(f) => assert!(f.reason.contains("500"), "{f:?}"),
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
    }

    /// A redirect is refused rather than followed. reqwest would follow up to
    /// ten by default, and a 302 turns the POST into a GET — the endpoint would
    /// receive no payload at all, and horsie would read whatever came back as
    /// the hook's reply.
    #[tokio::test]
    async fn a_redirect_is_not_followed() {
        let url = one_shot_server("302 Found", "").await;
        let plugins = TempDir::new().unwrap();
        http_plugin(plugins.path(), "webhook", "SessionStart", &url);
        let e = env(plugins);
        let records = run_hooks(&e.registry, &start(SessionStartSource::Startup)).await;
        match &records[0].action {
            HookAction::SessionStart(r) => match &r.outcome {
                SessionStartOutcome::Failed(f) => assert!(f.reason.contains("302"), "{f:?}"),
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
    }

    /// Nothing listening is the same outage shape a hook that could not be
    /// spawned produces.
    #[tokio::test]
    async fn an_unreachable_endpoint_is_recorded_as_failed() {
        let plugins = TempDir::new().unwrap();
        // Port 1 on loopback: reserved, and nothing binds it.
        http_plugin(
            plugins.path(),
            "webhook",
            "SessionStart",
            "http://127.0.0.1:1/hook",
        );
        let e = env(plugins);
        let records = run_hooks(&e.registry, &start(SessionStartSource::Startup)).await;
        match &records[0].action {
            HookAction::SessionStart(r) => {
                assert!(matches!(r.outcome, SessionStartOutcome::Failed(_)));
            }
            other => panic!("{other:?}"),
        }
    }
}
