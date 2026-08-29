//! The agent-facing authoring tools.
//!
//! Executes in the server process against the database — the sandboxed runtime
//! is never involved, like `MemoryToolbox` and `ControlToolbox`.
//!
//! Unlike the memory tools these are **governed**: they appear in
//! `crate::tools`'s catalogue and are absent from the default set, so a session
//! gets them only by naming them in its tool selection. Writing a skill that
//! every future session can load is authority, and the selection is the whole
//! grant — there is no second flag that could disagree with it.
//!
//! Specs are static: `CompositeToolbox::execute` calls `specs()` on every box
//! for every tool call, so nothing here may touch the database.

use super::AuthoredService;
use async_trait::async_trait;
use horsie_agentcore::{ToolCallError, ToolOutcome, ToolSpec, Toolbox};
use horsie_models::plugins::{
    AuthoredFileView, AuthoredSkillRestoreInput, AuthoredSkillWriteInput,
};
use serde_json::{Value, json};
use std::sync::Arc;

pub const PLUGIN_WRITE: &str = "plugin_write";
pub const SKILL_WRITE: &str = "skill_write";
pub const SKILL_DELETE: &str = "skill_delete";
pub const SKILL_LIST: &str = "skill_list";
pub const SKILL_HISTORY: &str = "skill_history";
pub const SKILL_RESTORE: &str = "skill_restore";

/// Every tool this layer adds, for the catalogue to advertise.
pub const TOOLS: &[(&str, &str)] = &[
    (
        PLUGIN_WRITE,
        "Create a plugin to hold skills you author, or change its description.",
    ),
    (
        SKILL_WRITE,
        "Write a skill into one of your plugins, or edit one you already wrote.",
    ),
    (SKILL_DELETE, "Remove a skill you wrote."),
    (SKILL_LIST, "List the skills you have written."),
    (
        SKILL_HISTORY,
        "Show a skill's past revisions, including ones since replaced.",
    ),
    (SKILL_RESTORE, "Put a skill back to one of its revisions."),
];

pub struct AuthoringToolbox {
    inner: Arc<dyn Toolbox>,
    service: Arc<AuthoredService>,
}

impl AuthoringToolbox {
    pub fn new(inner: Arc<dyn Toolbox>, service: Arc<AuthoredService>) -> Self {
        Self { inner, service }
    }

    fn specs_for_authoring(&self) -> Vec<ToolSpec> {
        vec![
            ToolSpec {
                name: PLUGIN_WRITE.to_string(),
                description: "Create a plugin to hold skills you author, or change an \
                     existing one's description. A plugin is the unit a session selects, \
                     so group skills that belong together. Names are lowercase letters, \
                     digits, '-' and '.'."
                    .to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "description": "Slug for the plugin, e.g. 'deploy-notes'."},
                        "description": {"type": "string", "description": "One line: what this plugin is for."}
                    },
                    "required": ["name"]
                }),
            },
            ToolSpec {
                name: SKILL_WRITE.to_string(),
                description: "Write a skill into one of your plugins, or edit one you \
                     already wrote. Supply only the fields you are changing: an omitted \
                     one keeps its current value. Both 'description' and 'body' are \
                     required the first time, because a skill nobody can choose between \
                     is not one. Takes effect for sessions started after this one."
                    .to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "plugin": {"type": "string", "description": "Which of your plugins to write into."},
                        "name": {"type": "string", "description": "Slug for the skill, e.g. 'rolling-back-a-deploy'."},
                        "description": {"type": "string", "description": "One line, shown in the skill index — this is what a model picks from, so say when to use it."},
                        "body": {"type": "string", "description": "The skill's instructions, in Markdown."},
                        "files": {
                            "type": "array",
                            "description": "Files that sit beside the skill, replacing the current set. Paths are relative to the skill, e.g. 'scripts/run.sh'.",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "path": {"type": "string"},
                                    "content": {"type": "string"}
                                },
                                "required": ["path", "content"]
                            }
                        }
                    },
                    "required": ["plugin", "name"]
                }),
            },
            ToolSpec {
                name: SKILL_DELETE.to_string(),
                description: "Remove a skill you wrote. Its history is kept, so it can \
                     be restored later."
                    .to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "plugin": {"type": "string"},
                        "name": {"type": "string"}
                    },
                    "required": ["plugin", "name"]
                }),
            },
            ToolSpec {
                name: SKILL_LIST.to_string(),
                description: "List the skills you have written, with their descriptions."
                    .to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "plugin": {"type": "string", "description": "Limit to one plugin. Omit for all of them."}
                    }
                }),
            },
            ToolSpec {
                name: SKILL_HISTORY.to_string(),
                description: "Show a skill's past revisions, newest first, including \
                     ones that have since been replaced or deleted."
                    .to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "plugin": {"type": "string"},
                        "name": {"type": "string"}
                    },
                    "required": ["plugin", "name"]
                }),
            },
            ToolSpec {
                name: SKILL_RESTORE.to_string(),
                description: "Put a skill back to one of its own revisions. The restore \
                     is itself a new revision, so nothing is lost by undoing it."
                    .to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "plugin": {"type": "string"},
                        "name": {"type": "string"},
                        "revision": {"type": "integer", "description": "From skill_history."}
                    },
                    "required": ["plugin", "name", "revision"]
                }),
            },
        ]
    }

    async fn write_plugin(&self, input: Value) -> Result<Value, ToolCallError> {
        let name = str_arg(&input, "name")?;
        let description = opt_str(&input, "description");
        let view = self
            .service
            .write_plugin(&name, description.as_deref())
            .await
            .map_err(bad)?;
        Ok(json!({
            "plugin": view.name,
            "generation": view.generation,
            "skills": view.skills.len(),
        }))
    }

    async fn write_skill(&self, input: Value) -> Result<Value, ToolCallError> {
        let files = match input.get("files") {
            None | Some(Value::Null) => None,
            Some(Value::Array(items)) => Some(
                items
                    .iter()
                    .map(|f| {
                        Ok(AuthoredFileView {
                            path: str_arg(f, "path")?,
                            content: str_arg(f, "content")?,
                        })
                    })
                    .collect::<Result<Vec<_>, ToolCallError>>()?,
            ),
            Some(_) => return Err(bad("'files' must be an array")),
        };
        let view = self
            .service
            .write_skill(AuthoredSkillWriteInput {
                plugin: str_arg(&input, "plugin")?,
                name: str_arg(&input, "name")?,
                description: opt_str(&input, "description"),
                body: opt_str(&input, "body"),
                files,
            })
            .await
            .map_err(bad)?;
        Ok(json!({
            "skill": format!("{}/{}", view.plugin, view.name),
            "revision": view.revision,
            "saved": true,
        }))
    }

    async fn delete_skill(&self, input: Value) -> Result<Value, ToolCallError> {
        let plugin = str_arg(&input, "plugin")?;
        let name = str_arg(&input, "name")?;
        self.service
            .delete_skill(&plugin, &name)
            .await
            .map_err(bad)?;
        Ok(json!({ "deleted": format!("{plugin}/{name}") }))
    }

    async fn list_skills(&self, input: Value) -> Result<Value, ToolCallError> {
        let plugin = opt_str(&input, "plugin");
        let rows = self
            .service
            .list_skills(plugin.as_deref())
            .await
            .map_err(exec)?;
        Ok(json!({
            "skills": rows
                .into_iter()
                .map(|s| json!({
                    "plugin": s.plugin,
                    "name": s.name,
                    "description": s.description,
                    "revision": s.revision,
                }))
                .collect::<Vec<_>>()
        }))
    }

    async fn history(&self, input: Value) -> Result<Value, ToolCallError> {
        let plugin = str_arg(&input, "plugin")?;
        let name = str_arg(&input, "name")?;
        let rows = self.service.revisions(&plugin, &name).await.map_err(exec)?;
        Ok(json!({
            "revisions": rows
                .into_iter()
                .map(|r| json!({
                    "revision": r.revision,
                    "description": r.description,
                    "deleted": r.deleted,
                    "created_at": r.created_at,
                }))
                .collect::<Vec<_>>()
        }))
    }

    async fn restore(&self, input: Value) -> Result<Value, ToolCallError> {
        let revision = input
            .get("revision")
            .and_then(Value::as_u64)
            .ok_or_else(|| bad("'revision' is required and must be a number"))?;
        let view = self
            .service
            .restore_skill(AuthoredSkillRestoreInput {
                plugin: str_arg(&input, "plugin")?,
                name: str_arg(&input, "name")?,
                revision,
            })
            .await
            .map_err(bad)?;
        Ok(json!({
            "skill": format!("{}/{}", view.plugin, view.name),
            "restored_from": revision,
            "revision": view.revision,
        }))
    }
}

#[async_trait]
impl Toolbox for AuthoringToolbox {
    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = self.inner.specs();
        specs.extend(self.specs_for_authoring());
        specs
    }

    async fn execute(
        &self,
        name: &str,
        input: Value,
        tool_call_id: &str,
    ) -> Result<ToolOutcome, ToolCallError> {
        match name {
            PLUGIN_WRITE => self.write_plugin(input).await.map(ToolOutcome::from),
            SKILL_WRITE => self.write_skill(input).await.map(ToolOutcome::from),
            SKILL_DELETE => self.delete_skill(input).await.map(ToolOutcome::from),
            SKILL_LIST => self.list_skills(input).await.map(ToolOutcome::from),
            SKILL_HISTORY => self.history(input).await.map(ToolOutcome::from),
            SKILL_RESTORE => self.restore(input).await.map(ToolOutcome::from),
            _ => self.inner.execute(name, input, tool_call_id).await,
        }
    }
}

fn str_arg(input: &Value, key: &str) -> Result<String, ToolCallError> {
    input
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| bad(format!("'{key}' is required and must be a string")))
}

fn opt_str(input: &Value, key: &str) -> Option<String> {
    input.get(key).and_then(Value::as_str).map(str::to_string)
}

fn bad(msg: impl Into<String>) -> ToolCallError {
    ToolCallError::InvalidInput(msg.into())
}

fn exec(msg: impl Into<String>) -> ToolCallError {
    ToolCallError::ExecutionFailed(msg.into())
}
