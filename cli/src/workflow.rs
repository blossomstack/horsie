//! `horsie workflow …`: list and inspect workflow definitions, start a run,
//! and read where a run got to.
//!
//! A run is a session, so everything after starting one is the session
//! commands: `horsie session status <id>` for its lifecycle, and
//! `horsie session tail <id> --agent <step-agent-id>` for one step's
//! transcript. Only the graph needs a command of its own.

use crate::agent::truncate;
use crate::error::CliError;
use crate::server_client::ServerClient;
use horsie_models::session_api::RepoConfig;
use horsie_models::workflow::{StepRunView, WorkflowRunGraph, WorkflowRunRequest, WorkflowView};

pub async fn list(server: &str) -> Result<(), CliError> {
    let workflows = ServerClient::new(server).await?.list_workflows().await?;
    print!("{}", render_table(&workflows));
    Ok(())
}

pub async fn get(server: &str, name: &str) -> Result<(), CliError> {
    let workflow = ServerClient::new(server).await?.get_workflow(name).await?;
    print!("{}", render_detail(&workflow));
    Ok(())
}

/// Start a run. The server creates the session and the first step is already
/// on its way when this returns.
pub async fn run(
    server: &str,
    name: &str,
    input: String,
    vendor: Option<String>,
    repos: Vec<String>,
    session_name: Option<String>,
) -> Result<(), CliError> {
    let client = ServerClient::new(server).await?;
    let res = client
        .run_workflow(
            name,
            &WorkflowRunRequest {
                input,
                vendor,
                repos: if repos.is_empty() {
                    None
                } else {
                    Some(
                        repos
                            .into_iter()
                            .map(|url| RepoConfig {
                                url,
                                git_ref: None,
                                dir: None,
                            })
                            .collect(),
                    )
                },
                name: session_name,
            },
        )
        .await?;
    print!("{}", render_started(client.base(), &res.session.id));
    Ok(())
}

/// Where a run got to, as a table of its executions.
pub async fn status(server: &str, session_id: &str) -> Result<(), CliError> {
    let graph = ServerClient::new(server)
        .await?
        .workflow_run(session_id)
        .await?;
    print!("{}", render_run(&graph));
    Ok(())
}

fn render_table(workflows: &[WorkflowView]) -> String {
    if workflows.is_empty() {
        return "No workflows.\n".to_string();
    }
    let mut out = format!(
        "{:<24} {:<7} {:<12} {}\n",
        "NAME", "STEPS", "START", "DESCRIPTION"
    );
    for w in workflows {
        out.push_str(&format!(
            "{:<24} {:<7} {:<12} {}\n",
            truncate(&w.name, 24),
            w.steps.len(),
            truncate(&w.start, 12),
            truncate(&w.description, 40),
        ));
    }
    out
}

fn render_detail(w: &WorkflowView) -> String {
    let mut out = format!("{}\n", w.name);
    if !w.description.is_empty() {
        out.push_str(&format!("  {}\n", w.description));
    }
    out.push_str(&format!("  starts at: {}\n\n", w.start));
    for step in &w.steps {
        out.push_str(&format!("  {} (agent {})\n", step.name, step.agent));
        let outputs = step
            .output_schema
            .as_ref()
            .and_then(|s| s.get("properties"))
            .and_then(|p| p.as_object())
            .map(|p| p.keys().cloned().collect::<Vec<_>>().join(", "))
            .unwrap_or_default();
        if !outputs.is_empty() {
            out.push_str(&format!("    outputs: {outputs}\n"));
        }
        for t in step.transitions.iter().flatten() {
            match &t.condition {
                Some(c) => out.push_str(&format!("    → {} when {}\n", t.to, c)),
                None => out.push_str(&format!("    → {} otherwise\n", t.to)),
            }
        }
        if step.transitions.as_ref().is_none_or(|t| t.is_empty()) {
            out.push_str("    → ends the run\n");
        }
    }
    out
}

fn render_started(base: &str, session_id: &str) -> String {
    format!(
        "Started run {session_id}\n  {base}/sessions/{session_id}\n\n\
         Follow it with:\n  horsie workflow status {session_id}\n"
    )
}

fn render_run(graph: &WorkflowRunGraph) -> String {
    let mut out = format!(
        "{}  {}  {} tokens\n\n",
        graph.workflow,
        status_word(graph),
        graph.input_tokens + graph.output_tokens,
    );
    let runs: Vec<&StepRunView> = graph.nodes.iter().flat_map(|n| n.runs.iter()).collect();
    if runs.is_empty() {
        out.push_str("No step has run yet.\n");
    } else {
        out.push_str(&format!(
            "{:<4} {:<20} {:<4} {:<11} {}\n",
            "#", "STEP", "TRY", "STATUS", "AGENT"
        ));
        let mut ordered = runs;
        ordered.sort_by_key(|r| r.index);
        for r in ordered {
            out.push_str(&format!(
                "{:<4} {:<20} {:<4} {:<11} {}\n",
                r.index,
                truncate(&r.step, 20),
                r.attempt,
                step_word(r),
                r.agent_id,
            ));
        }
    }
    // The steps a run never reached, so the reader can tell "not taken" from
    // "not yet".
    let untouched: Vec<&str> = graph
        .nodes
        .iter()
        .filter(|n| n.runs.is_empty())
        .map(|n| n.step.as_str())
        .collect();
    if !untouched.is_empty() {
        out.push_str(&format!("\nNot reached: {}\n", untouched.join(", ")));
    }
    if let Some(error) = &graph.error {
        out.push_str(&format!("\nError: {error}\n"));
    }
    out
}

fn status_word(graph: &WorkflowRunGraph) -> &'static str {
    use horsie_models::workflow::WorkflowStatus;
    match graph.status {
        WorkflowStatus::Pending(_) => "pending",
        WorkflowStatus::Running(_) => "running",
        WorkflowStatus::Suspended(_) => "suspended",
        WorkflowStatus::AwaitingInput(_) => "awaiting input",
        WorkflowStatus::Finished(_) => "finished",
        WorkflowStatus::Failed(_) => "failed",
    }
}

fn step_word(r: &StepRunView) -> &'static str {
    use horsie_models::workflow::StepRunStatus;
    match r.status {
        StepRunStatus::Running(_) => "running",
        StepRunStatus::Concluded(_) => "concluded",
        StepRunStatus::Failed(_) => "failed",
        StepRunStatus::Cancelled(_) => "cancelled",
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use horsie_models::workflow::{
        FinishedStatus, RunNode, StepConcluded, StepRunStatus, WorkflowStatus, WorkflowStepDef,
        WorkflowTransition,
    };

    fn step(name: &str, to: Option<(&str, Option<&str>)>) -> WorkflowStepDef {
        WorkflowStepDef {
            name: name.into(),
            agent: "coder".into(),
            prompt: "do it".into(),
            output_schema: Some(serde_json::json!({
                "type": "object",
                "properties": {"severity": {"type": "string"}}
            })),
            transitions: to.map(|(target, cond)| {
                vec![WorkflowTransition {
                    to: target.into(),
                    condition: cond.map(str::to_string),
                }]
            }),
            max_iterations: None,
            max_retries: None,
        }
    }

    #[test]
    fn an_empty_list_says_so_rather_than_printing_a_bare_header() {
        assert_eq!(render_table(&[]), "No workflows.\n");
    }

    #[test]
    fn the_detail_shows_each_transition_and_its_condition() {
        let w = WorkflowView {
            name: "fix-bug".into(),
            description: "triage then fix".into(),
            start: "triage".into(),
            steps: vec![
                step("triage", Some(("fix", Some("output.severity == \"p0\"")))),
                step("fix", None),
            ],
            created_at: "1".into(),
            updated_at: "1".into(),
        };
        let out = render_detail(&w);
        assert!(out.contains("starts at: triage"), "{out}");
        assert!(
            out.contains("→ fix when output.severity == \"p0\""),
            "{out}"
        );
        assert!(out.contains("outputs: severity"), "{out}");
        // A step with nowhere to go is where the run ends — worth saying, since
        // an empty line reads like missing information.
        assert!(out.contains("→ ends the run"), "{out}");
    }

    #[test]
    fn a_run_lists_its_executions_in_order_and_names_what_it_missed() {
        let concluded = |index: u32, step: &str| StepRunView {
            index,
            step: step.into(),
            agent_id: format!("agent-{index}"),
            attempt: 1,
            status: StepRunStatus::Concluded(StepConcluded {}),
            output: None,
            error: None,
            started_at_ms: 0,
            ended_at_ms: Some(1),
            input_tokens: 0,
            output_tokens: 0,
        };
        let graph = WorkflowRunGraph {
            workflow: "fix-bug".into(),
            status: WorkflowStatus::Finished(FinishedStatus {}),
            current: None,
            start: "triage".into(),
            nodes: vec![
                RunNode {
                    step: "triage".into(),
                    runs: vec![concluded(0, "triage")],
                },
                RunNode {
                    step: "fix".into(),
                    runs: vec![concluded(1, "fix")],
                },
                RunNode {
                    step: "file".into(),
                    runs: vec![],
                },
            ],
            edges: vec![],
            output: None,
            error: None,
            input_tokens: 120,
            output_tokens: 30,
        };
        let out = render_run(&graph);
        assert!(out.contains("finished"), "{out}");
        assert!(out.contains("150 tokens"), "{out}");
        assert!(out.contains("agent-0"), "{out}");
        // "Not reached" distinguishes a branch not taken from a step still to
        // come.
        assert!(out.contains("Not reached: file"), "{out}");
    }

    #[test]
    fn a_run_with_no_steps_yet_says_so() {
        let graph = WorkflowRunGraph {
            workflow: "w".into(),
            status: WorkflowStatus::Finished(FinishedStatus {}),
            current: None,
            start: "a".into(),
            nodes: vec![],
            edges: vec![],
            output: None,
            error: None,
            input_tokens: 0,
            output_tokens: 0,
        };
        assert!(render_run(&graph).contains("No step has run yet."));
    }
}
