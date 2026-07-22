//! A built-in `task_list` tool: an agent-visible scratchpad for tracking a
//! multi-step plan (create a list, insert tasks at a position, mark one or
//! more tasks' status).
//!
//! [`TaskListState`] is durable agent state — journaled via
//! `AgentDomainEvent::TaskListChanged` and folded into `AgentState`, exactly
//! like [`crate::timers::TimerRecord`] — so it survives an actor restart. The
//! tool executes by `ask`ing the owning `AgentActor` (see `TaskListToolbox` in
//! `agent_actor.rs`), never forwarded to the sandboxed runtime. This module
//! only holds the data model and the pure state-transition/parsing logic; the
//! actor wiring (command, event, journal fold) lives in `agent_actor.rs`.
//!
//! See `docs/superpowers/specs/2026-07-20-task-list-tool-design.md`.

use horsie_agentcore::{ToolCallError, ToolSpec};
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRecord {
    pub id: u32,
    pub content: String,
    pub status: TaskStatus,
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

    /// Apply one action, atomically: on error, `self` is left unchanged (no
    /// partial mutation), so a rejected batch never leaves the list in a
    /// confusing in-between state.
    pub fn apply(&mut self, action: TaskListAction) -> Result<(), String> {
        match action {
            TaskListAction::Create { tasks } => {
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

/// One `task_list` tool call, already validated into a typed shape. Carried
/// over the actor boundary as `AgentCommand::TaskListOp`'s payload.
#[derive(Debug, Clone)]
pub enum TaskListAction {
    Create {
        tasks: Vec<String>,
    },
    Insert {
        tasks: Vec<String>,
        position: Option<usize>,
    },
    UpdateStatus {
        ids: Vec<u32>,
        status: TaskStatus,
    },
    List,
}

impl TaskListAction {
    pub fn from_input(input: &Value) -> Result<Self, ToolCallError> {
        let action = input
            .get("action")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolCallError::InvalidInput("missing 'action'".to_string()))?;
        match action {
            "create" => Ok(Self::Create {
                tasks: task_texts(input)?,
            }),
            "insert" => Ok(Self::Insert {
                tasks: task_texts(input)?,
                position: input
                    .get("position")
                    .and_then(Value::as_u64)
                    .map(|p| p as usize),
            }),
            "update_status" => Ok(Self::UpdateStatus {
                ids: task_ids(input)?,
                status: parse_status(input)?,
            }),
            "list" => Ok(Self::List),
            other => Err(ToolCallError::InvalidInput(format!(
                "unknown action '{other}'; expected create, insert, update_status, or list"
            ))),
        }
    }
}

fn task_texts(input: &Value) -> Result<Vec<String>, ToolCallError> {
    let tasks = input
        .get("tasks")
        .and_then(Value::as_array)
        .ok_or_else(|| ToolCallError::InvalidInput("missing 'tasks'".to_string()))?;
    if tasks.is_empty() {
        return Err(ToolCallError::InvalidInput(
            "'tasks' must not be empty".to_string(),
        ));
    }
    tasks
        .iter()
        .map(|v| {
            v.as_str().map(str::to_string).ok_or_else(|| {
                ToolCallError::InvalidInput("'tasks' entries must be strings".to_string())
            })
        })
        .collect()
}

fn task_ids(input: &Value) -> Result<Vec<u32>, ToolCallError> {
    let ids = input
        .get("ids")
        .and_then(Value::as_array)
        .ok_or_else(|| ToolCallError::InvalidInput("missing 'ids'".to_string()))?;
    if ids.is_empty() {
        return Err(ToolCallError::InvalidInput(
            "'ids' must not be empty".to_string(),
        ));
    }
    ids.iter()
        .map(|v| {
            v.as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| {
                    ToolCallError::InvalidInput(
                        "'ids' entries must be non-negative integers".to_string(),
                    )
                })
        })
        .collect()
}

fn parse_status(input: &Value) -> Result<TaskStatus, ToolCallError> {
    match input.get("status").and_then(Value::as_str) {
        Some("pending") => Ok(TaskStatus::Pending),
        Some("in_progress") => Ok(TaskStatus::InProgress),
        Some("completed") => Ok(TaskStatus::Completed),
        Some(other) => Err(ToolCallError::InvalidInput(format!(
            "unknown status '{other}'; expected pending, in_progress, or completed"
        ))),
        None => Err(ToolCallError::InvalidInput("missing 'status'".to_string())),
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
            Every action returns the full current list."
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
                    "description": "Task text, in order. Required for 'create' and 'insert'."
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
    fn create_rejects_empty_tasks() {
        let err = parse(json!({"action": "create", "tasks": []})).unwrap_err();
        assert!(matches!(err, ToolCallError::InvalidInput(_)));
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
