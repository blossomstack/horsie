#[allow(clippy::doc_markdown, clippy::too_many_arguments)]
pub mod agent {
    include!(concat!(env!("OUT_DIR"), "/agent/mod.rs"));
}

#[allow(clippy::doc_markdown, clippy::too_many_arguments)]
pub mod capabilities {
    include!(concat!(env!("OUT_DIR"), "/capabilities/mod.rs"));
}

#[allow(clippy::doc_markdown, clippy::too_many_arguments)]
pub mod events {
    include!(concat!(env!("OUT_DIR"), "/events/mod.rs"));
}

#[allow(clippy::doc_markdown, clippy::too_many_arguments)]
pub mod executor {
    include!(concat!(env!("OUT_DIR"), "/executor/mod.rs"));
}

#[allow(clippy::doc_markdown, clippy::too_many_arguments)]
pub mod runtime {
    include!(concat!(env!("OUT_DIR"), "/runtime/mod.rs"));
}

#[allow(clippy::doc_markdown, clippy::too_many_arguments)]
pub mod workflow {
    include!(concat!(env!("OUT_DIR"), "/workflow/mod.rs"));
}

#[allow(clippy::doc_markdown, clippy::too_many_arguments)]
pub mod daemon {
    include!(concat!(env!("OUT_DIR"), "/daemon/mod.rs"));
}

impl capabilities::CapabilitySpec {
    /// Load and parse a capability file (the runtime's `--sandbox-caps` path, or a
    /// user-authored file the CLI resolves). Shared by the runtime and the CLI; the
    /// built-in *default* spec is owned by the CLI, not here.
    pub fn load(path: &std::path::Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("read capability file {}: {e}", path.display()))?;
        serde_json::from_str(&text)
            .map_err(|e| format!("parse capability file {}: {e}", path.display()))
    }
}

impl agent::Message {
    pub fn user(id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            role: agent::Role::User,
            parts: vec![agent::ContentPart::Text(agent::TextPart {
                text: text.into(),
            })],
        }
    }

    pub fn tool_result(
        tool_call_id: impl Into<String>,
        output: impl Into<String>,
        is_error: bool,
    ) -> Self {
        let tool_call_id = tool_call_id.into();
        Self {
            id: format!("result:{tool_call_id}"),
            role: agent::Role::Tool,
            parts: vec![agent::ContentPart::ToolResult(agent::ToolResultPart {
                tool_call_id,
                output: output.into(),
                is_error,
            })],
        }
    }
}

impl agent::AgentInput {
    pub fn user_message(id: impl Into<String>, text: impl Into<String>) -> Self {
        Self::UserMessage(agent::UserMessageInput {
            id: id.into(),
            text: text.into(),
        })
    }

    pub fn tool_result(
        tool_call_id: impl Into<String>,
        output: impl Into<String>,
        is_error: bool,
    ) -> Self {
        Self::ToolResult(agent::ToolResultInput {
            tool_call_id: tool_call_id.into(),
            output: output.into(),
            is_error,
        })
    }

    pub fn message_id(&self) -> String {
        match self {
            Self::UserMessage(u) => u.id.clone(),
            Self::ToolResult(t) => format!("result:{}", t.tool_call_id),
        }
    }

    pub fn to_message(&self) -> agent::Message {
        match self {
            Self::UserMessage(u) => agent::Message {
                id: u.id.clone(),
                role: agent::Role::User,
                parts: vec![agent::ContentPart::Text(agent::TextPart {
                    text: u.text.clone(),
                })],
            },
            Self::ToolResult(t) => agent::Message {
                id: format!("result:{}", t.tool_call_id),
                role: agent::Role::Tool,
                parts: vec![agent::ContentPart::ToolResult(agent::ToolResultPart {
                    tool_call_id: t.tool_call_id.clone(),
                    output: t.output.clone(),
                    is_error: t.is_error,
                })],
            },
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::capabilities::{Access, CapabilitySpec, Grant, NetworkPolicy};

    #[test]
    fn capability_spec_load_parses_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("caps.json");
        std::fs::write(
            &path,
            r#"{
                "network": "Allow",
                "grants": [
                    { "type": "Dir", "value": { "path": "/usr", "access": "Read" } },
                    { "type": "WorkingDir", "value": { "access": "ReadWrite" } }
                ]
            }"#,
        )
        .unwrap();
        let spec = CapabilitySpec::load(&path).expect("valid file parses");
        assert_eq!(spec.network, NetworkPolicy::Allow);
        assert!(matches!(
            spec.grants.first(),
            Some(Grant::Dir(d)) if d.path == "/usr" && d.access == Access::Read
        ));
    }

    #[test]
    fn capability_spec_load_rejects_missing_file() {
        let err = CapabilitySpec::load(std::path::Path::new("/nonexistent/october-caps.json"))
            .expect_err("missing file must error");
        assert!(err.contains("read capability file"));
    }
}
