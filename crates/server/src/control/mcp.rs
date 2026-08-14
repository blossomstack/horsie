//! The mcp resource: the remote MCP servers an account has configured.
//!
//! The OAuth pair (`connect` and the callback) stays in [`crate::http::mcp`].
//! Both build their `redirect_uri` from the host headers of the request that
//! carried them, and an operation never sees a request — a tool call has no
//! origin to hand back to a browser at all.

use crate::control::{ControlError, Expose, Method, NameRef, NoInput, Operation, Resource, op};
use crate::users::UserServices;
use horsie_models::mcp::{McpServerInput, McpServerList};
use std::sync::Arc;

/// Configured remote MCP servers. Views are redacted; no secret leaves here.
pub struct Mcp;

impl Resource for Mcp {
    fn name(&self) -> &'static str {
        "mcp"
    }

    fn operations(&self) -> Vec<Operation> {
        vec![
            op(
                "list",
                Method::Get,
                "/api/mcp/servers",
                "Every configured MCP server, with its tokens redacted to a \
                 has-token flag.",
                Expose::ApiAndTool,
                |s: Arc<UserServices>, _i: NoInput| async move {
                    let servers = s.mcp.list().await.map_err(ControlError::Internal)?;
                    // The route answers an object, not a bare array.
                    Ok::<McpServerList, ControlError>(McpServerList { servers })
                },
            ),
            op(
                "upsert",
                Method::Put,
                "/api/mcp/servers/{name}",
                "Create or replace a server. The name is the id of record. \
                 Omitting a secret keeps the stored one; an empty string clears \
                 it.",
                Expose::ApiAndTool,
                |s: Arc<UserServices>, i: McpServerInput| async move {
                    s.mcp.upsert(i).await.map_err(ControlError::Invalid)
                },
            ),
            op(
                "delete",
                Method::Delete,
                "/api/mcp/servers/{name}",
                "Forget a server.",
                Expose::ApiAndTool,
                // 200 with an empty result rather than 204, and a name that was
                // never configured is accepted rather than reported: the store
                // cannot say whether a row matched, so there is nothing to turn
                // into a miss. `deleting_an_unknown_mcp_server_is_silently_accepted`
                // pins the asymmetry with `/api/runtime-vendors`.
                |s: Arc<UserServices>, i: NameRef| async move {
                    s.mcp
                        .delete(&i.name)
                        .await
                        .map_err(ControlError::Internal)?;
                    Ok::<(), ControlError>(())
                },
            ),
            op(
                "test",
                Method::Post,
                "/api/mcp/servers/{name}/test",
                "Connect to a server, record the outcome, and return it. A \
                 connection that fails is a result with `ok: false`, not an \
                 error — only a server that is not configured at all is a miss.",
                Expose::ApiAndTool,
                |s: Arc<UserServices>, i: NameRef| async move {
                    let outcome = s.mcp.test(&i.name).await.map_err(ControlError::Internal)?;
                    outcome.ok_or_else(|| {
                        ControlError::NotFound(format!("no MCP server '{}'", i.name))
                    })
                },
            ),
        ]
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

    fn operations() -> Vec<Operation> {
        Mcp.operations()
    }

    #[test]
    fn every_action_is_declared_once_on_one_resource() {
        let mut actions: Vec<&str> = operations().iter().map(|o| o.action).collect();
        actions.sort_unstable();
        assert_eq!(actions, ["delete", "list", "test", "upsert"]);
        assert_eq!(Mcp.name(), "mcp");
    }

    #[test]
    fn every_path_param_is_a_field_of_its_input() {
        crate::control::tests::assert_path_params_are_inputs(&operations());
    }
}
