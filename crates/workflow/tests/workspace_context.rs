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

use horsie_models::runtime::{ScannedFile, WorkspaceScan};
use horsie_runtime_host::{MockTransport, RuntimeClient};
use horsie_workflow::{
    AgentRunDef, DefaultToolboxFactory, ToolboxFactory, compose_system_prompt, scan_workspace,
};

fn agent_def() -> AgentRunDef {
    AgentRunDef {
        system_prompt: Some("You are a coder.".into()),
        output_schema: None,
        allow_ask_user: false,
        allow_timers: None,
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
        // Absolute, as the runtime's glob produces it.
        skills: vec![ScannedFile {
            path: "/ws/october/.claude/skills/git-bisect/SKILL.md".into(),
            content:
                "---\nname: git-bisect\ndescription: Find the bad commit\n---\nRun git bisect."
                    .into(),
        }],
        platform: None,
    }
}

#[tokio::test]
async fn scan_composes_prompt_and_exposes_skill_tool() {
    let client = RuntimeClient::new(
        MockTransport::ok("").with_scan(vec![scan_payload()]),
        "test-agent",
    );
    let (ws, _shared) = scan_workspace(&client, None, false).await;

    // Prompt: role first, then a `# Workspaces` block per root, then its skill listing.
    let prompt = compose_system_prompt(agent_def().system_prompt.as_deref(), &ws, None).unwrap();
    assert!(prompt.contains("You are a coder."));
    assert!(prompt.contains("# Workspaces"));
    assert!(prompt.contains("## october — /ws/october (git)"));
    assert!(prompt.contains("Project rules."));
    // The intro names the directory the agent starts in; how that directory
    // behaves — sticky, moved with set_working_dir, never with `cd` — is the
    // `# Tool session state` section's job. Neither advertises a `workspace`
    // argument the tools no longer take.
    assert!(
        prompt.contains("Your working directory starts at /ws/october;"),
        "{prompt}"
    );
    assert!(prompt.contains("# Tool session state"), "{prompt}");
    assert!(
        prompt.contains("relative paths resolve against it"),
        "{prompt}"
    );
    assert!(!prompt.contains("`workspace` argument"), "{prompt}");
    // Each skill carries its directory, relative to the root in its header.
    assert!(
        prompt.contains("- git-bisect — .claude/skills/git-bisect/: Find the bad commit"),
        "{prompt}"
    );

    // Toolbox fetches skills live: skill + inspect_workspace present (even with
    // allowed_tools=["bash"]); skill(name) serves the body from a fresh scan, and with
    // a single workspace the `workspace` arg can be omitted.
    let tb = DefaultToolboxFactory.for_agent(&agent_def(), client, ws.names(), false, Vec::new());
    let names: Vec<String> = tb.specs().into_iter().map(|s| s.name).collect();
    assert!(names.contains(&"bash".to_string()));
    assert!(names.contains(&"skill".to_string()));
    assert!(names.contains(&"inspect_workspace".to_string()));
    let body = tb
        .execute("skill", serde_json::json!({ "name": "git-bisect" }), "tc1")
        .await
        .unwrap();
    assert_eq!(
        body,
        serde_json::json!(
            "Run git bisect.\n\n[resources] This skill's files are in \
             /ws/october/.claude/skills/git-bisect/. Read one with \
             read_file(path=\"/ws/october/.claude/skills/git-bisect/<file>\")."
        )
    );
}

#[tokio::test]
async fn empty_workspace_yields_plain_prompt_but_tools_present() {
    let client = RuntimeClient::new(MockTransport::ok(""), "test-agent"); // default empty scan
    let (ws, _shared) = scan_workspace(&client, None, false).await;
    let prompt = compose_system_prompt(agent_def().system_prompt.as_deref(), &ws, None);
    assert_eq!(prompt.as_deref(), Some("You are a coder."));
    let tb = DefaultToolboxFactory.for_agent(&agent_def(), client, ws.names(), false, Vec::new());
    let names: Vec<String> = tb.specs().into_iter().map(|s| s.name).collect();
    assert!(names.contains(&"skill".to_string()));
    assert!(names.contains(&"inspect_workspace".to_string()));
}
