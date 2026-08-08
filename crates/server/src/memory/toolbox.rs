//! The agent-facing memory tools. Executes in the server process against
//! SQLite -- the sandboxed runtime is never involved, like `McpToolbox`.
//!
//! Wraps an inner toolbox rather than composing into one, so memory tools sit
//! outside `FilteredToolbox` and a session that sets `allowed_tools` does not
//! silently lose them. The session's selected spaces are the only gate.
//!
//! Specs are static: `CompositeToolbox::execute` calls `specs()` on every box
//! for every tool call, so nothing here may touch the database.

use crate::memory::{MAX_DESCRIPTION_CHARS, MemoryRow, MemoryService};
use async_trait::async_trait;
use horsie_agentcore::{ToolCallError, ToolSpec, Toolbox};
use horsie_models::memory::{MemoryCreateInput, MemoryUpdateInput};
use serde_json::{Value, json};
use std::sync::Arc;

const LOAD: &str = "memory_load";
const CREATE: &str = "memory_create";
const UPDATE: &str = "memory_update";
const DELETE: &str = "memory_delete";
const LIST: &str = "memory_list";

pub struct MemoryToolbox {
    inner: Arc<dyn Toolbox>,
    service: Arc<MemoryService>,
    /// The session's selected spaces. Every read and write is confined to
    /// these, so a session cannot reach outside its declared scope.
    spaces: Vec<String>,
}

impl MemoryToolbox {
    pub fn new(inner: Arc<dyn Toolbox>, service: Arc<MemoryService>, spaces: Vec<String>) -> Self {
        Self {
            inner,
            service,
            spaces,
        }
    }

    fn specs_for_memory(&self) -> Vec<ToolSpec> {
        let spaces = self.spaces.join(", ");
        vec![
            ToolSpec {
                name: LOAD.to_string(),
                description: "Read the full text of saved memories. Addresses come from \
                     the memory index in your system prompt, in the form <space>/<name>. \
                     Batch every memory you want in one call."
                    .to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "refs": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "Memory addresses, e.g. [\"default/deploy-order\"]."
                        }
                    },
                    "required": ["refs"]
                }),
            },
            ToolSpec {
                name: CREATE.to_string(),
                description: format!(
                    "Save a new memory. Use this only for something durable and \
                     non-obvious that will matter in a later session -- not for facts the \
                     repository already records. Prefer {UPDATE} when a related memory \
                     already exists. Available spaces: {spaces}."
                ),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "space": {
                            "type": "string",
                            "description": format!(
                                "Which space to save into. Optional when only one space is \
                                 available. Available: {spaces}."
                            )
                        },
                        "name": {
                            "type": "string",
                            "description": "Short slug identifying the memory: lowercase \
                                 letters, digits, '.', '_' and '-'."
                        },
                        "description": {
                            "type": "string",
                            "description": format!(
                                "One line summarising the memory, at most \
                                 {MAX_DESCRIPTION_CHARS} characters. This is all you will \
                                 see in the index later, so make it specific enough to \
                                 decide whether to load the body."
                            )
                        },
                        "content": {
                            "type": "string",
                            "description": "The memory itself, in markdown. Reference \
                                 another memory as [[space/name]]."
                        }
                    },
                    "required": ["name", "description", "content"]
                }),
            },
            ToolSpec {
                name: UPDATE.to_string(),
                description: "Rewrite an existing memory. Supplied fields replace the old \
                     values wholesale; omitted fields are left alone."
                    .to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "ref": {"type": "string", "description": "Address, <space>/<name>."},
                        "description": {"type": "string"},
                        "content": {"type": "string"}
                    },
                    "required": ["ref"]
                }),
            },
            ToolSpec {
                name: DELETE.to_string(),
                description: "Delete a memory that is wrong or no longer useful.".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "ref": {"type": "string", "description": "Address, <space>/<name>."}
                    },
                    "required": ["ref"]
                }),
            },
            ToolSpec {
                name: LIST.to_string(),
                description: "Re-read the memory index. The index in your system prompt is \
                     a snapshot from the start of the turn, so use this after saving or \
                     deleting something in this same turn."
                    .to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "space": {
                            "type": "string",
                            "description": format!("Limit to one space. Available: {spaces}.")
                        }
                    }
                }),
            },
        ]
    }
}

#[async_trait]
impl Toolbox for MemoryToolbox {
    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = self.inner.specs();
        specs.extend(self.specs_for_memory());
        specs
    }

    async fn execute(
        &self,
        name: &str,
        input: Value,
        tool_call_id: &str,
    ) -> Result<Value, ToolCallError> {
        match name {
            LOAD => self.load(input).await,
            CREATE => self.create(input).await,
            UPDATE => self.update(input).await,
            DELETE => self.delete(input).await,
            LIST => self.list(input).await,
            _ => self.inner.execute(name, input, tool_call_id).await,
        }
    }
}

impl MemoryToolbox {
    async fn load(&self, input: Value) -> Result<Value, ToolCallError> {
        let refs = input
            .get("refs")
            .and_then(Value::as_array)
            .ok_or_else(|| bad("'refs' must be an array of memory addresses"))?;
        let mut found = Vec::new();
        let mut missing = Vec::new();
        for r in refs {
            let raw = r
                .as_str()
                .ok_or_else(|| bad("every entry in 'refs' must be a string"))?;
            let (space, name) = self.parse_ref(raw)?;
            match self.service.get_by_ref(&space, &name).await.map_err(exec)? {
                Some(row) => found.push(json!({
                    "ref": format!("{}/{}", row.space, row.name),
                    "description": row.description,
                    "content": row.content,
                    "updated_at": row.updated_at,
                })),
                None => missing.push(json!(raw)),
            }
        }
        Ok(json!({ "memories": found, "not_found": missing }))
    }

    async fn create(&self, input: Value) -> Result<Value, ToolCallError> {
        let space = match input.get("space").and_then(Value::as_str) {
            Some(s) => {
                self.check_space(s)?;
                s.to_string()
            }
            None => match self.spaces.as_slice() {
                [only] => only.clone(),
                _ => {
                    return Err(bad(format!(
                        "'space' is required when several spaces are available: {}",
                        self.spaces.join(", ")
                    )));
                }
            },
        };
        let name = str_arg(&input, "name")?;
        if self
            .service
            .get_by_ref(&space, &name)
            .await
            .map_err(exec)?
            .is_some()
        {
            return Err(bad(format!(
                "memory '{space}/{name}' already exists — use {UPDATE} to change it"
            )));
        }
        let view = self
            .service
            .create_memory(MemoryCreateInput {
                space,
                name,
                description: str_arg(&input, "description")?,
                content: str_arg(&input, "content")?,
            })
            .await
            .map_err(bad)?;
        Ok(json!({
            "ref": format!("{}/{}", view.space, view.name),
            "saved": true,
        }))
    }

    async fn update(&self, input: Value) -> Result<Value, ToolCallError> {
        let row = self.resolve(&input).await?;
        let description = input
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string);
        let content = input
            .get("content")
            .and_then(Value::as_str)
            .map(str::to_string);
        if description.is_none() && content.is_none() {
            return Err(bad("supply 'description', 'content', or both"));
        }
        self.service
            .update_memory(
                row.id,
                MemoryUpdateInput {
                    description,
                    content,
                },
            )
            .await
            .map_err(bad)?;
        Ok(json!({ "ref": format!("{}/{}", row.space, row.name), "updated": true }))
    }

    async fn delete(&self, input: Value) -> Result<Value, ToolCallError> {
        let row = self.resolve(&input).await?;
        self.service.delete_memory(row.id).await.map_err(exec)?;
        Ok(json!({ "ref": format!("{}/{}", row.space, row.name), "deleted": true }))
    }

    async fn list(&self, input: Value) -> Result<Value, ToolCallError> {
        let spaces = match input.get("space").and_then(Value::as_str) {
            Some(s) => {
                self.check_space(s)?;
                vec![s.to_string()]
            }
            None => self.spaces.clone(),
        };
        let rows = self.service.memories_in(&spaces).await.map_err(exec)?;
        let items: Vec<Value> = rows
            .iter()
            .map(|r| {
                json!({
                    "ref": format!("{}/{}", r.space, r.name),
                    "description": r.description,
                })
            })
            .collect();
        Ok(json!({ "memories": items }))
    }

    /// Split a `space/name` address and confirm the space is one this session
    /// selected.
    fn parse_ref(&self, raw: &str) -> Result<(String, String), ToolCallError> {
        let (space, name) = raw
            .split_once('/')
            .ok_or_else(|| bad(format!("'{raw}' is not a memory address — use space/name")))?;
        if space.is_empty() || name.is_empty() || name.contains('/') {
            return Err(bad(format!(
                "'{raw}' is not a memory address — use space/name"
            )));
        }
        self.check_space(space)?;
        Ok((space.to_string(), name.to_string()))
    }

    fn check_space(&self, space: &str) -> Result<(), ToolCallError> {
        if self.spaces.iter().any(|s| s == space) {
            Ok(())
        } else {
            Err(bad(format!(
                "memory space '{space}' is not available to this session; available: {}",
                self.spaces.join(", ")
            )))
        }
    }

    /// Resolve the `ref` argument of update/delete to an existing row.
    async fn resolve(&self, input: &Value) -> Result<MemoryRow, ToolCallError> {
        let raw = input
            .get("ref")
            .and_then(Value::as_str)
            .ok_or_else(|| bad("'ref' is required — a memory address, space/name"))?;
        let (space, name) = self.parse_ref(raw)?;
        self.service
            .get_by_ref(&space, &name)
            .await
            .map_err(exec)?
            .ok_or_else(|| bad(format!("no memory at '{raw}'")))
    }
}

fn str_arg(input: &Value, key: &str) -> Result<String, ToolCallError> {
    input
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| bad(format!("'{key}' is required and must be a string")))
}

fn bad(msg: impl Into<String>) -> ToolCallError {
    ToolCallError::InvalidInput(msg.into())
}

fn exec(msg: impl Into<String>) -> ToolCallError {
    ToolCallError::ExecutionFailed(msg.into())
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
    use crate::memory::MemoryStore;
    use horsie_agentcore::EmptyToolbox;

    async fn toolbox(spaces: &[&str]) -> (MemoryToolbox, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let pool = crate::db::testing::db().await;
        let service = Arc::new(MemoryService::new(MemoryStore::new(
            pool,
            crate::auth::UserId::new("1"),
        )));
        for s in spaces {
            if *s != "default" {
                service
                    .create_space(horsie_models::memory::MemorySpaceCreateInput {
                        name: (*s).to_string(),
                        description: None,
                    })
                    .await
                    .unwrap();
            }
        }
        let tb = MemoryToolbox::new(
            Arc::new(EmptyToolbox),
            service,
            spaces.iter().map(|s| (*s).to_string()).collect(),
        );
        (tb, tmp)
    }

    #[tokio::test]
    async fn specs_expose_five_tools_and_pass_through_the_inner_box() {
        let (tb, _t) = toolbox(&["default"]).await;
        let names: Vec<String> = tb.specs().into_iter().map(|s| s.name).collect();
        for expected in [
            "memory_load",
            "memory_create",
            "memory_update",
            "memory_delete",
            "memory_list",
        ] {
            assert!(names.contains(&expected.to_string()), "missing {expected}");
        }
        assert_eq!(names.len(), 5, "EmptyToolbox contributes nothing");
    }

    #[tokio::test]
    async fn unknown_tool_falls_through_to_the_inner_box() {
        let (tb, _t) = toolbox(&["default"]).await;
        let err = tb.execute("bash", json!({}), "tc1").await.unwrap_err();
        assert!(matches!(err, ToolCallError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn create_then_list_then_load_roundtrip() {
        let (tb, _t) = toolbox(&["default"]).await;
        let created = tb
            .execute(
                "memory_create",
                json!({"name": "alpha", "description": "a fact", "content": "the body"}),
                "tc1",
            )
            .await
            .unwrap();
        assert_eq!(created["ref"], "default/alpha");

        let listed = tb.execute("memory_list", json!({}), "tc1").await.unwrap();
        let items = listed["memories"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["ref"], "default/alpha");
        assert_eq!(items[0]["description"], "a fact");
        assert!(
            items[0].get("content").is_none(),
            "list must not ship bodies"
        );

        let loaded = tb
            .execute("memory_load", json!({"refs": ["default/alpha"]}), "tc1")
            .await
            .unwrap();
        let mems = loaded["memories"].as_array().unwrap();
        assert_eq!(mems[0]["content"], "the body");
    }

    #[tokio::test]
    async fn create_omitting_space_errors_when_several_are_selected() {
        let (tb, _t) = toolbox(&["default", "ops"]).await;
        let err = tb
            .execute(
                "memory_create",
                json!({"name": "alpha", "description": "d", "content": "c"}),
                "tc1",
            )
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("default") && msg.contains("ops"), "{msg}");
    }

    #[tokio::test]
    async fn writes_outside_the_selected_spaces_are_rejected() {
        let (tb, _t) = toolbox(&["default"]).await;
        let err = tb
            .execute(
                "memory_create",
                json!({"space": "ops", "name": "a", "description": "d", "content": "c"}),
                "tc1",
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("ops"));
    }

    #[tokio::test]
    async fn duplicate_name_points_at_memory_update() {
        let (tb, _t) = toolbox(&["default"]).await;
        let args = json!({"name": "alpha", "description": "d", "content": "c"});
        tb.execute("memory_create", args.clone(), "tc1")
            .await
            .unwrap();
        let err = tb.execute("memory_create", args, "tc1").await.unwrap_err();
        assert!(err.to_string().contains("memory_update"));
    }

    #[tokio::test]
    async fn load_reports_unknown_refs_without_failing_the_call() {
        let (tb, _t) = toolbox(&["default"]).await;
        tb.execute(
            "memory_create",
            json!({"name": "alpha", "description": "d", "content": "c"}),
            "tc1",
        )
        .await
        .unwrap();
        let out = tb
            .execute(
                "memory_load",
                json!({"refs": ["default/alpha", "default/ghost"]}),
                "tc1",
            )
            .await
            .unwrap();
        assert_eq!(out["memories"].as_array().unwrap().len(), 1);
        assert_eq!(
            out["not_found"].as_array().unwrap(),
            &[json!("default/ghost")]
        );
    }

    #[tokio::test]
    async fn malformed_ref_is_an_input_error() {
        let (tb, _t) = toolbox(&["default"]).await;
        let err = tb
            .execute("memory_load", json!({"refs": ["alpha"]}), "tc1")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("space/name"));
    }

    #[tokio::test]
    async fn update_and_delete_by_ref() {
        let (tb, _t) = toolbox(&["default"]).await;
        tb.execute(
            "memory_create",
            json!({"name": "alpha", "description": "d", "content": "c"}),
            "tc1",
        )
        .await
        .unwrap();

        tb.execute(
            "memory_update",
            json!({"ref": "default/alpha", "content": "rewritten"}),
            "tc1",
        )
        .await
        .unwrap();
        let loaded = tb
            .execute("memory_load", json!({"refs": ["default/alpha"]}), "tc1")
            .await
            .unwrap();
        assert_eq!(loaded["memories"][0]["content"], "rewritten");
        assert_eq!(loaded["memories"][0]["description"], "d");

        tb.execute("memory_delete", json!({"ref": "default/alpha"}), "tc1")
            .await
            .unwrap();
        let listed = tb.execute("memory_list", json!({}), "tc1").await.unwrap();
        assert!(listed["memories"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn update_and_delete_reject_refs_outside_the_selected_spaces() {
        let (tb, _t) = toolbox(&["default"]).await;
        for tool in ["memory_update", "memory_delete"] {
            let err = tb
                .execute(tool, json!({"ref": "ops/alpha", "content": "x"}), "tc1")
                .await
                .unwrap_err();
            assert!(err.to_string().contains("ops"), "{tool}");
        }
    }
}
