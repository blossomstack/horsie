//! The tools resource: which built-in tools this server offers, so that a
//! selection can name them.
//!
//! Read-only, and deliberately so — the catalogue is a table compiled into
//! this binary. Nothing here manages anything, which is why it was left out of
//! the control plane at first. That was the wrong call for one concrete
//! reason: `horsie_agents replace` takes `allowed_tools`, and an agent writing
//! a preset had no way to find out what a legal name looks like. It guessed,
//! and a guess that misses is not an error — an unknown name is simply passed
//! through, so the preset ends up narrowed to fewer tools than intended with
//! nothing to say so.
//!
//! Tool-only. `GET /api/tools` already serves this to the browser, unscoped
//! and unchanged; mounting a second route under the project scope would be two
//! addresses for one table.

use crate::control::{ControlError, Expose, Method, NoInput, Operation, Resource, op};
use crate::projects::ProjectServices;
use horsie_models::tools::ToolCatalog;
use std::sync::Arc;

/// The built-in tools an agent preset or a session may be narrowed to.
pub struct Tools;

impl Resource for Tools {
    fn name(&self) -> &'static str {
        "tools"
    }

    fn operations(&self) -> Vec<Operation> {
        vec![op(
            "list",
            Method::Get,
            "/tools",
            "Every built-in tool this server offers, grouped, with what each \
             one does and whether it only reads. These are the names \
             `allowed_tools` takes on an agent preset or a session — a name \
             that is not in this list is not governed by a selection and is \
             silently ignored, so check here before narrowing one. MCP and \
             skill tools are deliberately absent: they are chosen by selecting \
             the server or the bundle, and their names do not exist until \
             something has been connected to.",
            Expose::ToolOnly,
            |_s: Arc<ProjectServices>, _i: NoInput| async move {
                Ok::<ToolCatalog, ControlError>(crate::tools::catalog())
            },
        )]
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use crate::control::{Expose, Method};

    /// The gap this resource closes. `horsie_agents` takes `allowed_tools`, and
    /// a name outside the catalogue is not rejected — it is passed through
    /// ungoverned — so an agent that guesses one narrows a preset to fewer
    /// tools than it meant to, silently. Without a way to read the catalogue,
    /// guessing was the only option.
    #[test]
    fn an_agent_can_read_the_names_allowed_tools_takes() {
        let listed: Vec<String> = crate::tools::catalog()
            .groups
            .iter()
            .flat_map(|g| g.tools.iter().map(|t| t.name.clone()))
            .collect();
        assert!(
            listed.iter().any(|n| n == "bash"),
            "the catalogue a tool call answers with is the one a selection governs",
        );

        let ops = crate::control::operations();
        let list = ops
            .iter()
            .find(|o| o.resource == "tools" && o.action == "list")
            .expect("tools.list must be in the operation table");
        assert_eq!(list.method, Method::Get);
    }

    /// `GET /api/tools` already serves this, unscoped. A second mounted route
    /// would be two addresses for one compiled-in table, and the browser and
    /// the model would be reading different URLs for the same answer.
    #[test]
    fn it_mounts_no_route_of_its_own() {
        let ops = crate::control::operations();
        let list = ops
            .iter()
            .find(|o| o.resource == "tools")
            .expect("tools.list must be in the operation table");
        assert_eq!(list.expose, Expose::ToolOnly);
    }

    /// The one control resource that only reads, and the badge has to say so:
    /// every other `horsie_*` tool can change something, and a reader deciding
    /// whether to grant the group is asking exactly this.
    #[test]
    fn it_is_the_read_only_member_of_the_control_group() {
        let catalog = crate::tools::catalog();
        let control = catalog
            .groups
            .iter()
            .find(|g| g.key == crate::tools::GROUP_CONTROL)
            .expect("the control group is generated from the operation table");
        let tools = control
            .tools
            .iter()
            .find(|t| t.name == "horsie_tools")
            .expect("a resource in the table appears in the group without anyone adding it");
        assert_eq!(tools.access, horsie_models::tools::ToolAccess::Read);
        assert!(
            !tools.in_default_set,
            "authority over this server is asked for, never inherited",
        );
        assert!(
            tools.description.starts_with("Read this server's"),
            "a resource with no write action must not read as manageable: {}",
            tools.description,
        );
    }
}
