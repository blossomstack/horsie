use crate::error::ToolCallError;
use async_trait::async_trait;
use serde_json::Value;

/// Absolute ceiling for text returned by one tool call to the model.
///
/// Individual toolboxes may enforce a smaller, richer limit (for example by
/// preserving complete command output in a spill file). This final guard exists
/// for every toolbox, including server-native and remotely configured MCP tools.
pub const MAX_TOOL_RESULT_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BoundedToolOutput {
    pub output: String,
    pub original_bytes: usize,
    pub retained_bytes: usize,
}

pub(crate) fn bound_tool_output(output: String) -> BoundedToolOutput {
    let original_bytes = output.len();
    if original_bytes <= MAX_TOOL_RESULT_BYTES {
        return BoundedToolOutput {
            retained_bytes: original_bytes,
            original_bytes,
            output,
        };
    }

    let mut marker = String::new();
    let mut retained = MAX_TOOL_RESULT_BYTES;
    for _ in 0..2 {
        let dropped = original_bytes.saturating_sub(retained);
        marker = format!(
            "\n\n… {dropped} byte(s) omitted by the final tool-output guard. \
             Narrow the request to inspect the missing content.\n\n"
        );
        retained = MAX_TOOL_RESULT_BYTES.saturating_sub(marker.len());
    }

    let mut head_end = retained / 2;
    while head_end > 0 && !output.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = original_bytes.saturating_sub(retained.saturating_sub(head_end));
    while tail_start < original_bytes && !output.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let bounded = format!("{}{}{}", &output[..head_end], marker, &output[tail_start..]);
    BoundedToolOutput {
        retained_bytes: bounded.len(),
        original_bytes,
        output: bounded,
    }
}

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
    Result(ToolValue),
    /// The run ends here.
    StopRun,
}

/// What an ordinary tool call answered with.
///
/// A struct rather than a bare `Value` because a tool can produce something
/// that is not text — a screenshot, a rendered PDF — and that has to reach both
/// the transcript and the model. `artifacts` are already-stored references: a
/// toolbox that produces bytes stores them itself, inside its own `execute`,
/// which is why nothing below this point ever handles raw bytes.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ToolValue {
    pub value: Value,
    pub artifacts: Vec<horsie_models::agent::ArtifactRef>,
}

impl ToolValue {
    #[must_use]
    pub fn new(value: Value) -> Self {
        Self {
            value,
            artifacts: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_artifacts(value: Value, artifacts: Vec<horsie_models::agent::ArtifactRef>) -> Self {
        Self { value, artifacts }
    }
}

impl From<Value> for ToolValue {
    fn from(value: Value) -> Self {
        Self::new(value)
    }
}

impl ToolOutcome {
    /// An ordinary result carrying no artifacts — what almost every tool
    /// returns.
    #[must_use]
    pub fn result(value: impl Into<Value>) -> Self {
        Self::Result(ToolValue::new(value.into()))
    }
}

#[cfg(test)]
mod output_tests {
    use super::*;

    #[test]
    fn final_tool_output_guard_keeps_head_tail_and_utf8() {
        let output = format!("HEAD{}TAIL", "界".repeat(MAX_TOOL_RESULT_BYTES));
        let bounded = bound_tool_output(output.clone());
        assert_eq!(bounded.original_bytes, output.len());
        assert_eq!(bounded.retained_bytes, bounded.output.len());
        assert!(bounded.output.len() <= MAX_TOOL_RESULT_BYTES);
        assert!(bounded.output.starts_with("HEAD"));
        assert!(bounded.output.ends_with("TAIL"));
        assert!(bounded.output.contains("final tool-output guard"));
    }
}

#[cfg(any(test, feature = "test-util"))]
#[allow(clippy::panic)]
impl ToolOutcome {
    /// The value an ordinary call answered with, for tests that exercise tools
    /// which never end a run. Panics on [`ToolOutcome::StopRun`].
    pub fn expect_value(self) -> Value {
        match self {
            Self::Result(v) => v.value,
            Self::StopRun => panic!("expected a value, got a call that ended the run"),
        }
    }
}

impl From<Value> for ToolOutcome {
    fn from(value: Value) -> Self {
        Self::Result(ToolValue::new(value))
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
        assert_eq!(result, ToolOutcome::result(json!({"x": 1})));
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
