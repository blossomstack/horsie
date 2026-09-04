//! A built-in `task_list` tool: an agent-visible scratchpad for tracking a
//! multi-step plan (create a list, insert tasks at a position, mark one or
//! more tasks' status).
//!
//! [`TaskListState`] is durable agent state — journaled via
//! `AgentDomainEvent::TaskListChanged` and folded into `AgentState`, exactly
//! like [`crate::agent_loop::timers::TimerRecord`] — so it survives an actor restart. The
//! tool executes by `ask`ing the owning `AgentActor` (see `TaskListToolbox` in
//! `agent_actor/task_list.rs`), never forwarded to the sandboxed runtime. This
//! module only holds the data model and the pure state-transition/parsing
//! logic; the actor wiring (command, event, journal fold) lives in
//! `agent_actor/task_list.rs`.
//!
//! See `docs/superpowers/specs/2026-07-20-task-list-tool-design.md`.

use horsie_agentcore::{TaskItem, ToolCallError, ToolSpec};
use horsie_models::agent::TaskStatus as WireTaskStatus;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Name of the built-in task-list tool.
pub const TASK_LIST_TOOL: &str = "task_list";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    InProgress,
    Completed,
}

impl TaskStatus {
    fn marker(self) -> &'static str {
        match self {
            TaskStatus::Pending => " ",
            TaskStatus::InProgress => ">",
            TaskStatus::Completed => "x",
        }
    }

    fn label(self) -> &'static str {
        match self {
            TaskStatus::Pending => "pending",
            TaskStatus::InProgress => "in_progress",
            TaskStatus::Completed => "completed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRecord {
    pub id: u32,
    pub content: String,
    pub status: TaskStatus,
}

/// One task, as a client reads it.
///
/// The durable record and the wire item are deliberately separate types — one
/// is journaled and the other is generated — so the crossing is written once,
/// here, beside the state it reads.
pub fn wire_task(t: &TaskRecord) -> TaskItem {
    TaskItem {
        id: t.id,
        content: t.content.clone(),
        status: match t.status {
            TaskStatus::Pending => WireTaskStatus::Pending,
            TaskStatus::InProgress => WireTaskStatus::InProgress,
            TaskStatus::Completed => WireTaskStatus::Completed,
        },
    }
}

/// Durable per-agent task list. Journaled whole (not as deltas) on every
/// mutation, mirroring how `MessageComplete`/`ToolComplete` events carry full
/// content rather than diffs — replay never needs to re-derive or re-validate
/// a past mutation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskListState {
    tasks: Vec<TaskRecord>,
    next_id: u32,
}

impl Default for TaskListState {
    fn default() -> Self {
        Self {
            tasks: Vec::new(),
            next_id: 1,
        }
    }
}

impl TaskListState {
    /// The current tasks, in list order — read-only view for callers (e.g.
    /// the session server) that project this state onto a wire event.
    pub fn tasks(&self) -> &[TaskRecord] {
        &self.tasks
    }

    /// The same list, as the wire names it — what the log entry every mutation
    /// writes carries, and what the agent document reports.
    #[must_use]
    pub fn wire_tasks(&self) -> Vec<TaskItem> {
        self.tasks.iter().map(wire_task).collect()
    }

    pub fn render(&self) -> String {
        if self.tasks.is_empty() {
            return "No tasks.".to_string();
        }
        let done = self
            .tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Completed)
            .count();
        let mut out = format!("Tasks ({done}/{} done):\n", self.tasks.len());
        for t in &self.tasks {
            out.push_str(&format!(
                "[{}] {}. {}\n",
                t.status.marker(),
                t.id,
                t.content
            ));
        }
        out.pop(); // drop trailing newline
        out
    }

    /// Render the model-facing result without repeating the full list after
    /// every small mutation. The durable lifecycle event still carries the
    /// complete snapshot for clients.
    pub fn render_result(&self, action: &TaskListAction) -> String {
        match action {
            TaskListAction::Create { .. } | TaskListAction::List => self.render(),
            TaskListAction::Insert { tasks, .. } => {
                let first = self.next_id.saturating_sub(tasks.len() as u32);
                let ids = (first..self.next_id)
                    .map(|id| id.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("Inserted task(s) {ids}. {}", self.progress())
            }
            TaskListAction::UpdateStatus { ids, status } => {
                let ids = ids
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("Task(s) {ids} → {}. {}", status.label(), self.progress())
            }
        }
    }

    fn progress(&self) -> String {
        let done = self
            .tasks
            .iter()
            .filter(|task| task.status == TaskStatus::Completed)
            .count();
        format!("Progress: {done}/{} complete.", self.tasks.len())
    }

    /// Apply one action, atomically: on error, `self` is left unchanged (no
    /// partial mutation), so a rejected batch never leaves the list in a
    /// confusing in-between state.
    pub fn apply(&mut self, action: TaskListAction) -> Result<(), String> {
        match action {
            TaskListAction::Create { tasks } => {
                if tasks.is_empty() {
                    return Err("'tasks' must not be empty".to_string());
                }
                self.tasks = tasks
                    .into_iter()
                    .enumerate()
                    .map(|(i, content)| TaskRecord {
                        id: i as u32 + 1,
                        content,
                        status: TaskStatus::Pending,
                    })
                    .collect();
                self.next_id = self.tasks.len() as u32 + 1;
                Ok(())
            }
            TaskListAction::Insert { tasks, position } => {
                if tasks.is_empty() {
                    return Err("'tasks' must not be empty".to_string());
                }
                let len = self.tasks.len();
                let position = position.unwrap_or(len);
                if position > len {
                    return Err(format!(
                        "position {position} is out of range; list has {len} task(s)"
                    ));
                }
                let mut new_tasks = Vec::with_capacity(tasks.len());
                for content in tasks {
                    new_tasks.push(TaskRecord {
                        id: self.next_id,
                        content,
                        status: TaskStatus::Pending,
                    });
                    self.next_id += 1;
                }
                let tail = self.tasks.split_off(position);
                self.tasks.extend(new_tasks);
                self.tasks.extend(tail);
                Ok(())
            }
            TaskListAction::UpdateStatus { ids, status } => {
                if ids.is_empty() {
                    return Err("'ids' must not be empty".to_string());
                }
                let missing: Vec<String> = ids
                    .iter()
                    .filter(|id| !self.tasks.iter().any(|t| &t.id == *id))
                    .map(u32::to_string)
                    .collect();
                if !missing.is_empty() {
                    return Err(format!("unknown task id(s): {}", missing.join(", ")));
                }
                for t in self.tasks.iter_mut() {
                    if ids.contains(&t.id) {
                        t.status = status;
                    }
                }
                Ok(())
            }
            TaskListAction::List => Ok(()),
        }
    }
}

/// One `task_list` tool call, deserialized straight from the tool input by
/// serde — the `action` field selects the variant. Carried over the actor
/// boundary as `AgentCommand::TaskListOp`'s payload.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum TaskListAction {
    Create {
        tasks: Vec<String>,
    },
    Insert {
        tasks: Vec<String>,
        #[serde(default)]
        position: Option<usize>,
    },
    UpdateStatus {
        ids: Vec<u32>,
        status: TaskStatus,
    },
    List,
}

impl TaskListAction {
    /// Deserialize a tool call from its raw input. serde drives the whole parse
    /// off the `action` tag; a shape mismatch (wrong type, unknown action,
    /// missing field) surfaces as `InvalidInput` with serde's own message,
    /// which goes back to the model so it can correct the call.
    pub fn from_input(input: &Value) -> Result<Self, ToolCallError> {
        serde_json::from_value(input.clone())
            .map_err(|e| ToolCallError::InvalidInput(e.to_string()))
    }
}

pub fn task_list_tool_spec() -> ToolSpec {
    ToolSpec {
        name: TASK_LIST_TOOL.to_string(),
        description: "Track a multi-step plan as a visible list of tasks. \
            'create' replaces the whole list (use to start or fully re-plan). \
            'insert' adds one or more new tasks at a position (default: end). \
            'update_status' marks one or more tasks by id as pending, \
            in_progress, or completed. 'list' returns the current state. \
            'create' and 'list' return the full state; mutations return a compact summary. New tasks always start \
            as pending; move them with 'update_status'. \
            Example — create: {\"action\": \"create\", \"tasks\": [\"write tests\", \"ship it\"]}. \
            Example — mark done: {\"action\": \"update_status\", \"ids\": [1], \"status\": \"completed\"}."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "required": ["action"],
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["create", "insert", "update_status", "list"],
                    "description": "Which operation to perform."
                },
                "tasks": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Task texts, in order — one plain string per task, e.g. [\"write tests\", \"ship it\"]. Not objects. Required for 'create' and 'insert'."
                },
                "position": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "0-based index to insert at. 'insert' only; omitted appends to the end."
                },
                "ids": {
                    "type": "array",
                    "items": { "type": "integer" },
                    "description": "Task ids to update. Required for 'update_status'."
                },
                "status": {
                    "type": "string",
                    "enum": ["pending", "in_progress", "completed"],
                    "description": "New status for the given ids. Required for 'update_status'."
                }
            }
        }),
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

    fn create(state: &mut TaskListState, tasks: &[&str]) {
        state
            .apply(TaskListAction::Create {
                tasks: tasks.iter().map(|s| s.to_string()).collect(),
            })
            .unwrap();
    }

    fn parse(json: Value) -> Result<TaskListAction, ToolCallError> {
        TaskListAction::from_input(&json)
    }

    fn parse_create(json: Value) -> TaskListAction {
        let action = parse(json).unwrap();
        assert!(matches!(action, TaskListAction::Create { .. }));
        action
    }

    fn create_tasks(json: Value) -> Vec<String> {
        match parse_create(json) {
            TaskListAction::Create { tasks } => tasks,
            other => panic!("expected create action, got {other:?}"),
        }
    }

    #[test]
    fn create_replaces_list_with_pending_tasks() {
        let mut state = TaskListState::default();
        create(&mut state, &["a", "b"]);
        let text = state.render();
        assert!(text.contains("Tasks (0/2 done)"));
        assert!(text.contains("[ ] 1. a"));
        assert!(text.contains("[ ] 2. b"));
    }

    #[test]
    fn create_parses_plain_string_tasks() {
        let tasks = create_tasks(json!({"action": "create", "tasks": ["a", "b"]}));
        assert_eq!(tasks, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn create_rejects_empty_tasks() {
        // serde accepts an empty array; the empty batch is rejected in `apply`.
        let mut state = TaskListState::default();
        let err = state
            .apply(TaskListAction::Create { tasks: vec![] })
            .unwrap_err();
        assert!(err.contains("must not be empty"));
    }

    #[test]
    fn create_rejects_object_form() {
        // Tasks are plain strings only. The object form a model sometimes
        // reaches for (`{"text": ...}`) is now a hard InvalidInput rather than
        // silently accepted -- the schema and description steer it to strings.
        let err = parse(json!({"action": "create", "tasks": [{"text": "a"}]})).unwrap_err();
        assert!(matches!(err, ToolCallError::InvalidInput(_)));
    }

    #[test]
    fn create_rejects_non_string_entry() {
        let err = parse(json!({"action": "create", "tasks": [42]})).unwrap_err();
        assert!(matches!(err, ToolCallError::InvalidInput(_)));
    }

    #[test]
    fn insert_parses_plain_string_tasks() {
        match parse(json!({"action": "insert", "tasks": ["a"]})).unwrap() {
            TaskListAction::Insert { tasks, .. } => assert_eq!(tasks, vec!["a".to_string()]),
            other => panic!("expected insert action, got {other:?}"),
        }
    }

    #[test]
    fn insert_rejects_empty_tasks() {
        let mut state = TaskListState::default();
        let err = state
            .apply(TaskListAction::Insert {
                tasks: vec![],
                position: None,
            })
            .unwrap_err();
        assert!(err.contains("must not be empty"));
    }

    #[test]
    fn update_status_rejects_empty_ids() {
        let mut state = TaskListState::default();
        let err = state
            .apply(TaskListAction::UpdateStatus {
                ids: vec![],
                status: TaskStatus::Completed,
            })
            .unwrap_err();
        assert!(err.contains("must not be empty"));
    }

    #[test]
    fn create_resets_ids_on_each_call() {
        let mut state = TaskListState::default();
        create(&mut state, &["a", "b", "c"]);
        create(&mut state, &["x"]);
        let text = state.render();
        assert!(text.contains("[ ] 1. x"));
        assert!(!text.contains("2."));
    }

    #[test]
    fn insert_appends_by_default() {
        let mut state = TaskListState::default();
        create(&mut state, &["a"]);
        state
            .apply(TaskListAction::Insert {
                tasks: vec!["b".to_string()],
                position: None,
            })
            .unwrap();
        let text = state.render();
        assert!(text.contains("[ ] 1. a"));
        assert!(text.contains("[ ] 2. b"));
    }

    #[test]
    fn insert_at_position_shifts_existing_tasks() {
        let mut state = TaskListState::default();
        create(&mut state, &["a", "c"]);
        state
            .apply(TaskListAction::Insert {
                tasks: vec!["b".to_string()],
                position: Some(1),
            })
            .unwrap();
        let text = state.render();
        let lines: Vec<&str> = text.lines().skip(1).collect();
        assert_eq!(lines, vec!["[ ] 1. a", "[ ] 3. b", "[ ] 2. c"]);
    }

    #[test]
    fn insert_into_empty_list_at_zero_works() {
        let mut state = TaskListState::default();
        state
            .apply(TaskListAction::Insert {
                tasks: vec!["a".to_string()],
                position: Some(0),
            })
            .unwrap();
        assert!(state.render().contains("[ ] 1. a"));
    }

    #[test]
    fn insert_position_out_of_range_errors() {
        let mut state = TaskListState::default();
        create(&mut state, &["a"]);
        let err = state
            .apply(TaskListAction::Insert {
                tasks: vec!["b".to_string()],
                position: Some(5),
            })
            .unwrap_err();
        assert!(err.contains("out of range"));
    }

    #[test]
    fn insert_continues_ids_from_current_max() {
        let mut state = TaskListState::default();
        create(&mut state, &["a", "b"]);
        state
            .apply(TaskListAction::Insert {
                tasks: vec!["c".to_string()],
                position: Some(0),
            })
            .unwrap();
        state
            .apply(TaskListAction::Insert {
                tasks: vec!["d".to_string()],
                position: None,
            })
            .unwrap();
        assert!(state.render().contains("[ ] 4. d"));
    }

    #[test]
    fn update_status_marks_single_task_completed() {
        let mut state = TaskListState::default();
        create(&mut state, &["a", "b"]);
        state
            .apply(TaskListAction::UpdateStatus {
                ids: vec![1],
                status: TaskStatus::Completed,
            })
            .unwrap();
        let text = state.render();
        assert!(text.contains("Tasks (1/2 done)"));
        assert!(text.contains("[x] 1. a"));
        assert!(text.contains("[ ] 2. b"));
    }

    #[test]
    fn update_status_marks_multiple_tasks() {
        let mut state = TaskListState::default();
        create(&mut state, &["a", "b", "c"]);
        state
            .apply(TaskListAction::UpdateStatus {
                ids: vec![1, 3],
                status: TaskStatus::Completed,
            })
            .unwrap();
        let text = state.render();
        assert!(text.contains("Tasks (2/3 done)"));
        assert!(text.contains("[x] 1. a"));
        assert!(text.contains("[ ] 2. b"));
        assert!(text.contains("[x] 3. c"));
    }

    #[test]
    fn update_status_supports_in_progress_and_reopen() {
        let mut state = TaskListState::default();
        create(&mut state, &["a"]);
        state
            .apply(TaskListAction::UpdateStatus {
                ids: vec![1],
                status: TaskStatus::InProgress,
            })
            .unwrap();
        assert!(state.render().contains("[>] 1. a"));
        state
            .apply(TaskListAction::UpdateStatus {
                ids: vec![1],
                status: TaskStatus::Pending,
            })
            .unwrap();
        assert!(state.render().contains("[ ] 1. a"));
    }

    #[test]
    fn update_status_unknown_id_errors_without_partial_apply() {
        let mut state = TaskListState::default();
        create(&mut state, &["a", "b"]);
        let err = state
            .apply(TaskListAction::UpdateStatus {
                ids: vec![1, 99],
                status: TaskStatus::Completed,
            })
            .unwrap_err();
        assert!(err.contains("99"));
        // Task 1 must remain untouched -- the whole batch was rejected.
        assert!(state.render().contains("[ ] 1. a"));
    }

    #[test]
    fn update_status_rejects_missing_status() {
        let err = parse(json!({"action": "update_status", "ids": [1]})).unwrap_err();
        assert!(matches!(err, ToolCallError::InvalidInput(_)));
    }

    #[test]
    fn mutation_results_are_compact_but_list_results_are_complete() {
        let mut state = TaskListState::default();
        create(&mut state, &["a", "b"]);
        let action = TaskListAction::UpdateStatus {
            ids: vec![1],
            status: TaskStatus::Completed,
        };
        state.apply(action.clone()).unwrap();
        let result = state.render_result(&action);
        assert_eq!(result, "Task(s) 1 → completed. Progress: 1/2 complete.");
        assert!(!result.contains("2. b"));
        assert!(state.render_result(&TaskListAction::List).contains("2. b"));
    }

    #[test]
    fn list_on_empty_state_says_no_tasks() {
        assert_eq!(TaskListState::default().render(), "No tasks.");
    }

    #[test]
    fn list_does_not_mutate() {
        let mut state = TaskListState::default();
        create(&mut state, &["a"]);
        state.apply(TaskListAction::List).unwrap();
        assert!(state.render().contains("[ ] 1. a"));
    }

    #[test]
    fn unknown_action_errors() {
        let err = parse(json!({"action": "delete_everything"})).unwrap_err();
        assert!(matches!(err, ToolCallError::InvalidInput(_)));
    }

    #[test]
    fn missing_action_errors() {
        let err = parse(json!({})).unwrap_err();
        assert!(matches!(err, ToolCallError::InvalidInput(_)));
    }

    #[test]
    fn spec_has_expected_shape() {
        let spec = task_list_tool_spec();
        assert_eq!(spec.name, TASK_LIST_TOOL);
        assert_eq!(spec.input_schema["required"][0], "action");
    }
}
