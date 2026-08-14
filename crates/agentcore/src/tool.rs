use crate::error::ToolCallError;
use async_trait::async_trait;
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// What dispatching a tool call did.
///
/// A tool that returns [`StopRun`](ToolOutcome::StopRun) ends the run it was
/// called in. Nothing is recorded for the call — no result message, no
/// completion event — so the `tool_use` stays dangling, and that dangling call
/// *is* the shape of a parked agent: an answer can arrive against it later, or
/// never.
///
/// Declaring it here rather than in the agent's configuration is deliberate.
/// The object that advertises a tool's spec is the object that decides the
/// stop, so the two can never disagree — and a wrapper that filters a spec out
/// removes its ability to stop a run along with it.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolOutcome {
    /// An ordinary result: it goes back to the model and the run continues.
    Result(Value),
    /// The run ends here.
    StopRun,
}

#[cfg(any(test, feature = "test-util"))]
#[allow(clippy::panic)]
impl ToolOutcome {
    /// The value an ordinary call answered with, for tests that exercise tools
    /// which never end a run. Panics on [`ToolOutcome::StopRun`].
    pub fn expect_value(self) -> Value {
        match self {
            Self::Result(v) => v,
            Self::StopRun => panic!("expected a value, got a call that ended the run"),
        }
    }
}

impl From<Value> for ToolOutcome {
    fn from(value: Value) -> Self {
        Self::Result(value)
    }
}

#[async_trait]
pub trait Toolbox: Send + Sync {
    fn specs(&self) -> Vec<ToolSpec>;
    /// `tool_call_id` is the id the model gave this call. Carried so anything
    /// downstream can name the call it acted on — a remote runtime keys its
    /// cancellation by it, and a plugin hook's record joins back to the tool
    /// result in the transcript through it. Most toolboxes ignore it.
    async fn execute(
        &self,
        name: &str,
        input: Value,
        tool_call_id: &str,
    ) -> Result<ToolOutcome, ToolCallError>;
}

/// A single named tool.
///
/// Always ordinary: a tool registered here returns a value the model reads.
/// Ending a run is a property of a whole toolbox layer (`ask_user`,
/// `submit_result`), which is why it is [`Toolbox`] that can answer
/// [`ToolOutcome::StopRun`] and this trait cannot.
#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    /// `tool_call_id` is the model's id for this call, forwarded from
    /// [`Toolbox::execute`]. A tool that reaches a remote runtime passes it on;
    /// the rest ignore it.
    async fn execute(&self, input: Value, tool_call_id: &str) -> Result<Value, ToolCallError>;
}

/// Generic Toolbox impl — register individual Tool implementations into it.
pub struct ToolboxImpl {
    tools: Vec<Box<dyn Tool>>,
}

impl Default for ToolboxImpl {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolboxImpl {
    pub fn new() -> Self {
        Self { tools: Vec::new() }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn add(mut self, tool: impl Tool + 'static) -> Self {
        self.tools.push(Box::new(tool));
        self
    }
}

#[async_trait]
impl Toolbox for ToolboxImpl {
    fn specs(&self) -> Vec<ToolSpec> {
        self.tools.iter().map(|t| t.spec()).collect()
    }

    async fn execute(
        &self,
        name: &str,
        input: Value,
        tool_call_id: &str,
    ) -> Result<ToolOutcome, ToolCallError> {
        match self.tools.iter().find(|t| t.spec().name == name) {
            Some(tool) => tool
                .execute(input, tool_call_id)
                .await
                .map(ToolOutcome::from),
            None => Err(ToolCallError::InvalidInput(format!(
                "no tool named '{name}'"
            ))),
        }
    }
}

pub struct EmptyToolbox;

#[async_trait]
impl Toolbox for EmptyToolbox {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![]
    }

    async fn execute(
        &self,
        name: &str,
        _input: Value,
        _tool_call_id: &str,
    ) -> Result<ToolOutcome, ToolCallError> {
        Err(ToolCallError::InvalidInput(format!(
            "no tool named '{name}'"
        )))
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
    use serde_json::json;

    struct EchoTool;

    #[async_trait]
    impl Tool for EchoTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "echo".to_string(),
                description: "echoes input".to_string(),
                input_schema: json!({"type": "object"}),
            }
        }
        async fn execute(&self, input: Value, _tool_call_id: &str) -> Result<Value, ToolCallError> {
            Ok(input)
        }
    }

    /// A registered [`Tool`] is never terminal: its value is wrapped as an
    /// ordinary result, which is what keeps "this call ends the run" a decision
    /// only a whole toolbox layer can take.
    #[tokio::test]
    async fn toolbox_impl_routes_by_name_and_wraps_the_result() {
        let tb = ToolboxImpl::new().add(EchoTool);
        let result = tb.execute("echo", json!({"x": 1}), "tc1").await.unwrap();
        assert_eq!(result, ToolOutcome::Result(json!({"x": 1})));
    }

    #[tokio::test]
    async fn toolbox_impl_unknown_tool_returns_error() {
        let tb = ToolboxImpl::new();
        let err = tb.execute("nope", json!({}), "tc1").await.unwrap_err();
        assert!(matches!(err, ToolCallError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn toolbox_impl_specs_returns_all() {
        let tb = ToolboxImpl::new().add(EchoTool);
        let specs = tb.specs();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "echo");
    }
}
