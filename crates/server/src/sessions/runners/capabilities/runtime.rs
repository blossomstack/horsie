//! The sandbox: shell, files, workspaces, plugin tools and skills.
//!
//! The one capability with an open namespace. What tools exist is not knowable
//! until the turn is prepared — it is whatever the runtime accepts plus
//! whatever the plugin library scan discovered — so this capability claims
//! anything nobody else did, and assembly therefore sorts it last.

use super::{CapSlice, Capability, Decision, SetupError};
use crate::sessions::runners::action::{AgentSpec, ToolLayer};
use crate::sessions::runners::message::{Caller, Message};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuntimeCapability;

#[async_trait::async_trait]
impl Capability for RuntimeCapability {
    fn name(&self) -> &'static str {
        "runtime"
    }

    async fn setup(&self, spec: &mut AgentSpec) -> Result<(), SetupError> {
        spec.layers.push(ToolLayer::Runtime);
        Ok(())
    }

    /// Claims every tool call, and nothing else.
    ///
    /// It journals nothing: a sandbox call's effect is on disk and in the
    /// agent's own transcript, so there is no session-level fact to record.
    /// The empty decision is what says "taken, with nothing to journal".
    fn handle(&self, _caller: Caller, msg: &Message) -> Option<Decision> {
        matches!(msg, Message::Tool(_)).then(Decision::default)
    }

    fn save(&self) -> CapSlice {
        CapSlice::Runtime(self.clone())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::super::testing::*;
    use super::*;
    use crate::sessions::runners::message::{AskMsg, Message};

    /// It claims any tool, which is why it sorts last.
    #[test]
    fn it_claims_any_tool_call() {
        let c = RuntimeCapability;
        assert!(
            c.handle(caller(), &tool("bash", serde_json::json!({})))
                .is_some()
        );
        assert!(
            c.handle(caller(), &tool("some_plugin_tool", serde_json::json!({})))
                .is_some()
        );
    }

    /// But only tools. A child's outcome or an answer is addressed to its
    /// owner, and a fallback that swallowed those would break the addressing.
    #[test]
    fn it_claims_nothing_that_is_not_a_tool_call() {
        let c = RuntimeCapability;
        let ask = Message::Ask(AskMsg::Answered { answers: vec![] });
        assert!(c.handle(caller(), &ask).is_none());
    }
}
