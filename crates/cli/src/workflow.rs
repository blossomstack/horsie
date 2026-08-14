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
use horsie_models::environments::EnvironmentSpec;
use horsie_models::session::SessionStatusKind;
use horsie_models::workflow::{
    StepRunView, WorkflowInput, WorkflowRunGraph, WorkflowRunRequest, WorkflowView,
};

pub async fn list(server: &str) -> Result<(), CliError> {
    let workflows = ServerClient::new(server).await?.list_workflows().await?;
    print!("{}", render_table(&workflows));
    Ok(())
}

/// Show one workflow. `--json` prints the definition itself, which is what
/// `apply` takes back — so the pair is a round-trip and there is no second
/// format to keep in step.
pub async fn get(server: &str, name: &str, json: bool) -> Result<(), CliError> {
    let workflow = ServerClient::new(server).await?.get_workflow(name).await?;
    match json {
        true => println!("{}", to_json(&to_input(&workflow))?),
        false => print!("{}", render_detail(&workflow)),
    }
    Ok(())
}

/// Create or fully replace a definition from a JSON file.
///
/// One command rather than `create` and `replace`: from a file, "make the server
/// match this" is the only intent, and having to know whether the name is taken
/// first is friction with no purpose. The name comes from the file, because the
/// file is the definition.
pub async fn apply(server: &str, path: &str) -> Result<(), CliError> {
    let body = std::fs::read_to_string(path)
        .map_err(|e| CliError::Config(format!("cannot read {path}: {e}")))?;
    let input: WorkflowInput = serde_json::from_str(&body)
        .map_err(|e| CliError::Config(format!("{path} is not a workflow definition: {e}")))?;
    let name = input.name.clone();
    let client = ServerClient::new(server).await?;
    // Which verb this is depends on whether it exists, and only the server
    // knows. A missing one is the ordinary case for a first apply, not an error
    // worth relaying.
    let exists = client.get_workflow(&name).await.is_ok();
    let view = match exists {
        true => client.replace_workflow(&name, &input).await?,
        false => client.create_workflow(&input).await?,
    };
    let verb = match exists {
        true => "Updated",
        false => "Created",
    };
    println!("{verb} workflow {} ({} steps)", view.name, view.steps.len());
    Ok(())
}

pub async fn delete(server: &str, name: &str) -> Result<(), CliError> {
    ServerClient::new(server)
        .await?
        .delete_workflow(name)
        .await?;
    // Runs are not deleted with it: each carries its own snapshot of the graph,
    // so they stay readable. Worth saying, because deleting a routine does take
    // its runs.
    println!("Deleted workflow {name}. Its runs are sessions and are untouched.");
    Ok(())
}

/// Re-run one step execution of a run.
pub async fn retry(server: &str, session_id: &str, step_index: u32) -> Result<(), CliError> {
    ServerClient::new(server)
        .await?
        .retry_workflow_step(session_id, step_index)
        .await?;
    println!(
        "Retrying step {step_index} of {session_id}.\n  \
         The workspace is not rolled back — the new attempt runs against whatever \
         the last one left.\n\n\
         Follow it with:\n  horsie workflow status {session_id}"
    );
    Ok(())
}

/// The definition as an input document: what `get --json` prints and `apply`
/// takes. Deliberately not the view — `created_at` and `updated_at` are the
/// server's, and echoing them back would invite editing them.
fn to_input(w: &WorkflowView) -> WorkflowInput {
    WorkflowInput {
        name: w.name.clone(),
        description: (!w.description.is_empty()).then(|| w.description.clone()),
        start: w.start.clone(),
        steps: w.steps.clone(),
        max_steps: w.max_steps,
    }
}

fn to_json<T: serde::Serialize>(v: &T) -> Result<String, CliError> {
    serde_json::to_string_pretty(v).map_err(|e| CliError::Config(format!("render json: {e}")))
}

/// Start a run. The server creates the session and the first step is already
/// on its way when this returns.
pub async fn run(
    server: &str,
    name: &str,
    input: String,
    environment: EnvironmentSpec,
    session_name: Option<String>,
) -> Result<(), CliError> {
    let client = ServerClient::new(server).await?;
    let res = client
        .run_workflow(
            name,
            &WorkflowRunRequest {
                input,
                environment,
                name: session_name,
            },
        )
        .await?;
    print!("{}", render_started(client.base(), &res.session.id));
    Ok(())
}

/// Where a run got to, as a table of its executions.
///
/// Two reads: the graph says where the run got to, the session says what state
/// it is in. A run's status is its session's — one vocabulary for every session
/// — so there is nothing on the graph to print here.
pub async fn status(server: &str, session_id: &str) -> Result<(), CliError> {
    let client = ServerClient::new(server).await?;
    let graph = client.workflow_run(session_id).await?;
    let session = client.get_session(session_id).await?;
    print!("{}", render_run(&graph, session.status));
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
        let outcomes = step
            .outcomes
            .as_ref()
            .map(|o| {
                o.iter()
                    .map(|v| v.value.clone())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_else(|| "success, failure".to_string());
        out.push_str(&format!("    outcomes: {outcomes}\n"));
        let fields = step
            .fields
            .as_ref()
            .map(|f| {
                f.iter()
                    .map(|v| v.name.clone())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        if !fields.is_empty() {
            out.push_str(&format!("    fields: {fields}\n"));
        }
        for t in step.transitions.iter().flatten() {
            match &t.when {
                Some(f) => out.push_str(&format!("    → {} when {}\n", t.to, f.render())),
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

fn render_run(graph: &WorkflowRunGraph, status: SessionStatusKind) -> String {
    let mut out = format!(
        "{}  {}  {} tokens\n\n",
        graph.workflow,
        status_word(status),
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
    // What the run produced. It was on the wire from the start and printed
    // nowhere, so the one thing a finished run is *for* could only be read by
    // tailing its last step.
    if let Some(output) = &graph.output {
        out.push_str(&format!("\nOutput:\n{}\n", indent(&render_output(output))));
    }
    if let Some(error) = &graph.error {
        out.push_str(&format!("\nError: {error}\n"));
    }
    out
}

/// A run's output as text: a string is its own answer, anything else is pretty
/// JSON. The same rule that hands one step's output to the next, so what is
/// printed is what a following step would have been given.
fn render_output(output: &serde_json::Value) -> String {
    output.as_str().map(str::to_string).unwrap_or_else(|| {
        serde_json::to_string_pretty(output).unwrap_or_else(|_| output.to_string())
    })
}

fn indent(body: &str) -> String {
    body.lines()
        .map(|l| format!("  {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn status_word(status: SessionStatusKind) -> &'static str {
    match status {
        SessionStatusKind::Provisioning => "provisioning",
        // A run that is merely idle is one that stopped part-way: a step was
        // interrupted and nothing moves until someone retries it.
        SessionStatusKind::Idle => "suspended",
        SessionStatusKind::Running => "running",
        SessionStatusKind::AwaitingInput => "awaiting input",
        SessionStatusKind::Finished => "finished",
        SessionStatusKind::Failed => "failed",
        SessionStatusKind::Unrecoverable => "unrecoverable",
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
        OutcomeFilter, OutcomeIn, RunNode, StepConcluded, StepRunStatus, WorkflowStepDef,
        WorkflowTransition,
    };

    fn step(name: &str, to: Option<(&str, Option<&[&str]>)>) -> WorkflowStepDef {
        WorkflowStepDef {
            name: name.into(),
            agent: "coder".into(),
            prompt: "do it".into(),
            outcomes: Some(vec![
                horsie_models::workflow::StepOutcome {
                    value: "p0".into(),
                    description: "drop everything".into(),
                },
                horsie_models::workflow::StepOutcome {
                    value: "p2".into(),
                    description: "file it".into(),
                },
            ]),
            fields: None,
            interactive: None,
            transitions: to.map(|(target, values)| {
                vec![WorkflowTransition {
                    to: target.into(),
                    when: values.map(|v: &[&str]| {
                        OutcomeFilter::In(OutcomeIn {
                            values: v.iter().map(|x| (*x).to_string()).collect(),
                        })
                    }),
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
                step("triage", Some(("fix", Some(&["p0"])))),
                step("fix", None),
            ],
            max_steps: None,
            created_at: "1".into(),
            updated_at: "1".into(),
        };
        let out = render_detail(&w);
        assert!(out.contains("starts at: triage"), "{out}");
        assert!(out.contains("→ fix when outcome in [p0]"), "{out}");
        assert!(out.contains("outcomes: p0, p2"), "{out}");
        // A step with nowhere to go is where the run ends — worth saying, since
        // an empty line reads like missing information.
        assert!(out.contains("→ ends the run"), "{out}");
    }

    /// A finished two-step run that never reached `file`.
    fn finished_graph() -> WorkflowRunGraph {
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
        WorkflowRunGraph {
            workflow: "fix-bug".into(),
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
        }
    }

    #[test]
    fn a_run_lists_its_executions_in_order_and_names_what_it_missed() {
        let graph = finished_graph();
        let out = render_run(&graph, SessionStatusKind::Finished);
        assert!(out.contains("finished"), "{out}");
        assert!(out.contains("150 tokens"), "{out}");
        assert!(out.contains("agent-0"), "{out}");
        // "Not reached" distinguishes a branch not taken from a step still to
        // come.
        assert!(out.contains("Not reached: file"), "{out}");
    }

    /// A finished run's whole point is its output, and it used to be printed
    /// nowhere — readable only by tailing the last step's transcript.
    #[test]
    fn a_finished_run_prints_what_it_produced() {
        let mut graph = finished_graph();
        graph.output = Some(serde_json::json!({"filed": 12}));
        let out = render_run(&graph, SessionStatusKind::Finished);
        assert!(out.contains("Output:"), "{out}");
        assert!(out.contains("\"filed\": 12"), "{out}");
    }

    /// A string output is its own answer; quoting and escaping it would be
    /// noise, and it is what a following step would have been handed.
    #[test]
    fn a_string_output_is_printed_unquoted() {
        let mut graph = finished_graph();
        graph.output = Some(serde_json::json!("all clear"));
        let out = render_run(&graph, SessionStatusKind::Finished);
        assert!(out.contains("all clear"), "{out}");
        assert!(!out.contains("\"all clear\""), "{out}");
    }

    /// `get --json` prints what `apply` takes: the definition, and nothing the
    /// server owns. If these two drift, a round-trip silently loses part of a
    /// graph — and a save is a full replace.
    #[test]
    fn the_json_form_round_trips_through_a_definition_document() {
        let w = WorkflowView {
            name: "fix-bug".into(),
            description: "triage then fix".into(),
            start: "triage".into(),
            steps: vec![
                step("triage", Some(("fix", Some(&["p0"])))),
                step("fix", None),
            ],
            max_steps: Some(40),
            created_at: "1".into(),
            updated_at: "2".into(),
        };
        let json = to_json(&to_input(&w)).unwrap();
        let back: WorkflowInput = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, "fix-bug");
        assert_eq!(back.description.as_deref(), Some("triage then fix"));
        assert_eq!(back.start, "triage");
        assert_eq!(back.steps, w.steps, "every step survives the round-trip");
        assert_eq!(back.max_steps, Some(40), "the budget is part of the graph");
        // The server's own stamps are deliberately absent: echoing them back
        // would invite editing them.
        assert!(!json.contains("createdAt"), "{json}");
        assert!(!json.contains("updatedAt"), "{json}");
        // Keys are camelCase, because that is what the API reads. A snake_case
        // key in a body is not an error — it is silently ignored — so a printed
        // document that a user edits and applies has to be in the wire's casing
        // or the edit vanishes.
        assert!(json.contains("\"maxSteps\""), "{json}");
        assert!(json.contains("\"outcomes\""), "{json}");
        assert!(!json.contains("max_steps"), "{json}");
    }

    /// An empty description is absent rather than `""`, so a round-tripped
    /// document does not gain a field the user never set.
    #[test]
    fn an_empty_description_round_trips_as_absent() {
        let w = WorkflowView {
            name: "w".into(),
            description: String::new(),
            start: "a".into(),
            steps: vec![step("a", None)],
            max_steps: None,
            created_at: "1".into(),
            updated_at: "1".into(),
        };
        assert_eq!(to_input(&w).description, None);
    }

    #[test]
    fn a_run_with_no_steps_yet_says_so() {
        let graph = WorkflowRunGraph {
            workflow: "w".into(),
            current: None,
            start: "a".into(),
            nodes: vec![],
            edges: vec![],
            output: None,
            error: None,
            input_tokens: 0,
            output_tokens: 0,
        };
        assert!(render_run(&graph, SessionStatusKind::Running).contains("No step has run yet."));
    }
}
