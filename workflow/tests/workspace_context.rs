//! Integration test for workspace context: scan over a `RuntimeClient` (backed by a
//! `MockTransport` returning a `WorkspaceScan`), then prompt composition and the
//! `DefaultToolboxFactory` skill tool — the real seam used by `spawn_agent`, without
//! standing up the full actor/journal.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]

use models::runtime::{ScannedFile, WorkspaceScan};
use models::workflow::WorkflowAgentDef;
use runtime_client::{MockTransport, RuntimeClient};
use workflow::{DefaultToolboxFactory, ToolboxFactory, compose_system_prompt, scan_workspace};

fn agent_def() -> WorkflowAgentDef {
    WorkflowAgentDef {
        use_plugins: None,
        name: "coder".into(),
        system_prompt: Some("You are a coder.".into()),
        model: "m".into(),
        output_schema: None,
        allow_ask_user: false,
        allow_timers: None,
        transitions: None,
        max_iterations: None,
        max_retries: None,
        allowed_tools: Some(vec!["bash".into()]),
    }
}

fn scan_payload() -> WorkspaceScan {
    WorkspaceScan {
        name: "october".into(),
        path: "/ws/october".into(),
        is_git_repo: true,
        instructions: Some(ScannedFile {
            path: "AGENTS.md".into(),
            content: "Project rules.".into(),
        }),
        skills: vec![ScannedFile {
            path: ".claude/skills/git-bisect/SKILL.md".into(),
            content:
                "---\nname: git-bisect\ndescription: Find the bad commit\n---\nRun git bisect."
                    .into(),
        }],
    }
}

#[tokio::test]
async fn scan_composes_prompt_and_exposes_skill_tool() {
    let client = RuntimeClient::new(MockTransport::ok("").with_scan(vec![scan_payload()]));
    let (ws, _shared) = scan_workspace(&client, None, false).await;

    // Prompt: role first, then a `# Workspaces` block per root, then its skill listing.
    let prompt = compose_system_prompt(agent_def().system_prompt.as_deref(), &ws, None).unwrap();
    assert!(prompt.contains("You are a coder."));
    assert!(prompt.contains("# Workspaces"));
    assert!(prompt.contains("## october — /ws/october (git)"));
    assert!(prompt.contains("Project rules."));
    assert!(prompt.contains("- git-bisect: Find the bad commit"));

    // Toolbox fetches skills live: skill + inspect_workspace present (even with
    // allowed_tools=["bash"]); skill(name) serves the body from a fresh scan, and with
    // a single workspace the `workspace` arg can be omitted.
    let tb = DefaultToolboxFactory.for_agent(&agent_def(), client, ws.names(), false);
    let names: Vec<String> = tb.specs().into_iter().map(|s| s.name).collect();
    assert!(names.contains(&"bash".to_string()));
    assert!(names.contains(&"skill".to_string()));
    assert!(names.contains(&"inspect_workspace".to_string()));
    let body = tb
        .execute("skill", serde_json::json!({ "name": "git-bisect" }))
        .await
        .unwrap();
    assert_eq!(body, serde_json::json!("Run git bisect."));
}

#[tokio::test]
async fn empty_workspace_yields_plain_prompt_but_tools_present() {
    let client = RuntimeClient::new(MockTransport::ok("")); // default empty scan
    let (ws, _shared) = scan_workspace(&client, None, false).await;
    let prompt = compose_system_prompt(agent_def().system_prompt.as_deref(), &ws, None);
    assert_eq!(prompt.as_deref(), Some("You are a coder."));
    let tb = DefaultToolboxFactory.for_agent(&agent_def(), client, ws.names(), false);
    let names: Vec<String> = tb.specs().into_iter().map(|s| s.name).collect();
    assert!(names.contains(&"skill".to_string()));
    assert!(names.contains(&"inspect_workspace".to_string()));
}
