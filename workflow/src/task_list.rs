//! A built-in `task_list` tool: an agent-visible scratchpad for tracking a
//! multi-step plan (create a list, insert tasks at a position, mark one or
//! more tasks' status) without a sandbox round-trip.
//!
//! State lives on the [`TaskListTool`] instance itself, not the actor —
//! see `docs/superpowers/specs/2026-07-20-task-list-tool-design.md` for why
//! this tool doesn't need the durability the timers tools get.

use async_trait::async_trait;
use horsie_agentcore::{Tool, ToolCallError, ToolSpec};
use serde_json::{Value, json};
use std::sync::Mutex;

/// Name of the built-in task-list tool.
pub const TASK_LIST_TOOL: &str = "task_list";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskStatus {
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

#[derive(Debug, Clone)]
struct Task {
    id: u32,
    content: String,
    status: TaskStatus,
}

#[derive(Debug)]
struct TaskListState {
    tasks: Vec<Task>,
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
    fn render(&self) -> String {
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
}

/// A single, stateful `task_list` tool. One instance is constructed per agent
/// spawn (see `DefaultToolboxFactory::for_agent`), so its state lives exactly
/// as long as the agent run that owns it.
pub struct TaskListTool {
    state: Mutex<TaskListState>,
}

impl Default for TaskListTool {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskListTool {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(TaskListState::default()),
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

#[async_trait]
impl Tool for TaskListTool {
    fn spec(&self) -> ToolSpec {
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

    async fn execute(&self, input: Value) -> Result<Value, ToolCallError> {
        let action = input
            .get("action")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolCallError::InvalidInput("missing 'action'".to_string()))?;

        let mut state = self.state.lock().map_err(|_| {
            ToolCallError::ExecutionFailed("task list state lock poisoned".to_string())
        })?;

        match action {
            "create" => {
                let texts = task_texts(&input)?;
                state.tasks = texts
                    .into_iter()
                    .enumerate()
                    .map(|(i, content)| Task {
                        id: i as u32 + 1,
                        content,
                        status: TaskStatus::Pending,
                    })
                    .collect();
                state.next_id = state.tasks.len() as u32 + 1;
            }
            "insert" => {
                let texts = task_texts(&input)?;
                let len = state.tasks.len();
                let position = match input.get("position").and_then(Value::as_u64) {
                    Some(p) => {
                        let p = p as usize;
                        if p > len {
                            return Err(ToolCallError::InvalidInput(format!(
                                "position {p} is out of range; list has {len} task(s)"
                            )));
                        }
                        p
                    }
                    None => len,
                };
                let mut new_tasks = Vec::with_capacity(texts.len());
                for content in texts {
                    new_tasks.push(Task {
                        id: state.next_id,
                        content,
                        status: TaskStatus::Pending,
                    });
                    state.next_id += 1;
                }
                let tail = state.tasks.split_off(position);
                state.tasks.extend(new_tasks);
                state.tasks.extend(tail);
            }
            "update_status" => {
                let ids = task_ids(&input)?;
                let status = parse_status(&input)?;
                let missing: Vec<String> = ids
                    .iter()
                    .filter(|id| !state.tasks.iter().any(|t| &t.id == *id))
                    .map(u32::to_string)
                    .collect();
                if !missing.is_empty() {
                    return Err(ToolCallError::InvalidInput(format!(
                        "unknown task id(s): {}",
                        missing.join(", ")
                    )));
                }
                for t in state.tasks.iter_mut() {
                    if ids.contains(&t.id) {
                        t.status = status;
                    }
                }
            }
            "list" => {}
            other => {
                return Err(ToolCallError::InvalidInput(format!(
                    "unknown action '{other}'; expected create, insert, update_status, or list"
                )));
            }
        }

        Ok(Value::String(state.render()))
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

    #[tokio::test]
    async fn create_replaces_list_with_pending_tasks() {
        let tool = TaskListTool::new();
        let out = tool
            .execute(json!({"action": "create", "tasks": ["a", "b"]}))
            .await
            .unwrap();
        let text = out.as_str().unwrap();
        assert!(text.contains("Tasks (0/2 done)"));
        assert!(text.contains("[ ] 1. a"));
        assert!(text.contains("[ ] 2. b"));
    }

    #[tokio::test]
    async fn create_rejects_empty_tasks() {
        let tool = TaskListTool::new();
        let err = tool
            .execute(json!({"action": "create", "tasks": []}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolCallError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn create_resets_ids_on_each_call() {
        let tool = TaskListTool::new();
        tool.execute(json!({"action": "create", "tasks": ["a", "b", "c"]}))
            .await
            .unwrap();
        let out = tool
            .execute(json!({"action": "create", "tasks": ["x"]}))
            .await
            .unwrap();
        let text = out.as_str().unwrap();
        assert!(text.contains("[ ] 1. x"));
        assert!(!text.contains("2."));
    }

    #[tokio::test]
    async fn insert_appends_by_default() {
        let tool = TaskListTool::new();
        tool.execute(json!({"action": "create", "tasks": ["a"]}))
            .await
            .unwrap();
        let out = tool
            .execute(json!({"action": "insert", "tasks": ["b"]}))
            .await
            .unwrap();
        let text = out.as_str().unwrap();
        assert!(text.contains("[ ] 1. a"));
        assert!(text.contains("[ ] 2. b"));
    }

    #[tokio::test]
    async fn insert_at_position_shifts_existing_tasks() {
        let tool = TaskListTool::new();
        tool.execute(json!({"action": "create", "tasks": ["a", "c"]}))
            .await
            .unwrap();
        let out = tool
            .execute(json!({"action": "insert", "tasks": ["b"], "position": 1}))
            .await
            .unwrap();
        let text = out.as_str().unwrap();
        let lines: Vec<&str> = text.lines().skip(1).collect();
        assert_eq!(lines, vec!["[ ] 1. a", "[ ] 3. b", "[ ] 2. c"]);
    }

    #[tokio::test]
    async fn insert_into_empty_list_at_zero_works() {
        let tool = TaskListTool::new();
        let out = tool
            .execute(json!({"action": "insert", "tasks": ["a"], "position": 0}))
            .await
            .unwrap();
        assert!(out.as_str().unwrap().contains("[ ] 1. a"));
    }

    #[tokio::test]
    async fn insert_position_out_of_range_errors() {
        let tool = TaskListTool::new();
        tool.execute(json!({"action": "create", "tasks": ["a"]}))
            .await
            .unwrap();
        let err = tool
            .execute(json!({"action": "insert", "tasks": ["b"], "position": 5}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolCallError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn insert_continues_ids_from_current_max() {
        let tool = TaskListTool::new();
        tool.execute(json!({"action": "create", "tasks": ["a", "b"]}))
            .await
            .unwrap();
        tool.execute(json!({"action": "insert", "tasks": ["c"], "position": 0}))
            .await
            .unwrap();
        let out = tool
            .execute(json!({"action": "insert", "tasks": ["d"]}))
            .await
            .unwrap();
        assert!(out.as_str().unwrap().contains("[ ] 4. d"));
    }

    #[tokio::test]
    async fn update_status_marks_single_task_completed() {
        let tool = TaskListTool::new();
        tool.execute(json!({"action": "create", "tasks": ["a", "b"]}))
            .await
            .unwrap();
        let out = tool
            .execute(json!({"action": "update_status", "ids": [1], "status": "completed"}))
            .await
            .unwrap();
        let text = out.as_str().unwrap();
        assert!(text.contains("Tasks (1/2 done)"));
        assert!(text.contains("[x] 1. a"));
        assert!(text.contains("[ ] 2. b"));
    }

    #[tokio::test]
    async fn update_status_marks_multiple_tasks() {
        let tool = TaskListTool::new();
        tool.execute(json!({"action": "create", "tasks": ["a", "b", "c"]}))
            .await
            .unwrap();
        let out = tool
            .execute(json!({"action": "update_status", "ids": [1, 3], "status": "completed"}))
            .await
            .unwrap();
        let text = out.as_str().unwrap();
        assert!(text.contains("Tasks (2/3 done)"));
        assert!(text.contains("[x] 1. a"));
        assert!(text.contains("[ ] 2. b"));
        assert!(text.contains("[x] 3. c"));
    }

    #[tokio::test]
    async fn update_status_supports_in_progress_and_reopen() {
        let tool = TaskListTool::new();
        tool.execute(json!({"action": "create", "tasks": ["a"]}))
            .await
            .unwrap();
        let out = tool
            .execute(json!({"action": "update_status", "ids": [1], "status": "in_progress"}))
            .await
            .unwrap();
        assert!(out.as_str().unwrap().contains("[>] 1. a"));
        let out = tool
            .execute(json!({"action": "update_status", "ids": [1], "status": "pending"}))
            .await
            .unwrap();
        assert!(out.as_str().unwrap().contains("[ ] 1. a"));
    }

    #[tokio::test]
    async fn update_status_unknown_id_errors_without_partial_apply() {
        let tool = TaskListTool::new();
        tool.execute(json!({"action": "create", "tasks": ["a", "b"]}))
            .await
            .unwrap();
        let err = tool
            .execute(json!({"action": "update_status", "ids": [1, 99], "status": "completed"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolCallError::InvalidInput(_)));
        // Task 1 must remain untouched -- the whole batch was rejected.
        let out = tool.execute(json!({"action": "list"})).await.unwrap();
        assert!(out.as_str().unwrap().contains("[ ] 1. a"));
    }

    #[tokio::test]
    async fn update_status_rejects_missing_status() {
        let tool = TaskListTool::new();
        tool.execute(json!({"action": "create", "tasks": ["a"]}))
            .await
            .unwrap();
        let err = tool
            .execute(json!({"action": "update_status", "ids": [1]}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolCallError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn list_on_empty_state_says_no_tasks() {
        let tool = TaskListTool::new();
        let out = tool.execute(json!({"action": "list"})).await.unwrap();
        assert_eq!(out.as_str().unwrap(), "No tasks.");
    }

    #[tokio::test]
    async fn list_does_not_mutate() {
        let tool = TaskListTool::new();
        tool.execute(json!({"action": "create", "tasks": ["a"]}))
            .await
            .unwrap();
        tool.execute(json!({"action": "list"})).await.unwrap();
        let out = tool.execute(json!({"action": "list"})).await.unwrap();
        assert!(out.as_str().unwrap().contains("[ ] 1. a"));
    }

    #[tokio::test]
    async fn unknown_action_errors() {
        let tool = TaskListTool::new();
        let err = tool
            .execute(json!({"action": "delete_everything"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolCallError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn missing_action_errors() {
        let tool = TaskListTool::new();
        let err = tool.execute(json!({})).await.unwrap_err();
        assert!(matches!(err, ToolCallError::InvalidInput(_)));
    }

    #[test]
    fn spec_has_expected_shape() {
        let spec = TaskListTool::new().spec();
        assert_eq!(spec.name, TASK_LIST_TOOL);
        assert_eq!(spec.input_schema["required"][0], "action");
    }
}
