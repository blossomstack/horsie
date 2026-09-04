//! Dependencies available while one actor command is being decided.
//!
//! Durable history, live foreground state, immutable configuration, and the
//! actor mailbox are named separately so handlers cannot confuse their owners.

use crate::agent_loop::params::AgentParams;
use crate::agent_loop::step::StepRun;
use crate::agent_loop::{AgentCommand, AgentState, CoreCommand};
use horsie_actor::ActorContext;

/// The durable and live inputs available while handling one command.
pub(crate) struct CommandContext<'a> {
    pub state: &'a AgentState,
    pub step_run: &'a mut StepRun,
    pub runtime: &'a crate::agent_loop::context::AgentRuntimeContext,
    pub params: &'a AgentParams,
    pub actor: &'a ActorContext<AgentCommand>,
}

impl CommandContext<'_> {
    /// Announce that this agent has moved, waking every reader waiting on it.
    /// Announcing twice for one change is harmless.
    pub fn publish_revision(&self) {
        self.runtime.revision.send_modify(|r| *r += 1);
    }

    /// Put a follow-up command behind the current command's durable write.
    pub async fn tell(&self, cmd: AgentCommand) {
        let _ = self.actor.self_ref().tell(cmd).await;
    }

    /// Reconsider what the agent should do after a live change wrote no event.
    ///
    /// Rarely needed: the actor advances itself after every durable write, so
    /// this is for the changes that journal nothing at all.
    pub async fn advance(&self) {
        self.tell(AgentCommand::Core(CoreCommand::Advance)).await;
    }
}
