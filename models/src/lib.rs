#[allow(clippy::doc_markdown, clippy::too_many_arguments)]
pub mod agent {
    include!(concat!(env!("OUT_DIR"), "/agent/mod.rs"));
}

#[allow(clippy::doc_markdown, clippy::too_many_arguments)]
pub mod events {
    include!(concat!(env!("OUT_DIR"), "/events/mod.rs"));
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
