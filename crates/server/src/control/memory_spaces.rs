//! The memory-spaces resource: the named namespaces long-term memories live in.
//!
//! Spaces only. The memories inside one stay in `http::memory`, because the
//! agent already reaches them through `MemoryToolbox` — a second tool over the
//! same rows would be two vocabularies for one thing, and the model would have
//! to guess which one a session had.

use crate::control::{ControlError, Expose, Method, NoInput, Operation, Resource, op};
use crate::projects::ProjectServices;
use horsie_models::memory::{MemorySpaceCreateInput, MemorySpaceUpdateInput};
use std::sync::Arc;

/// The path param is `space` rather than `name` because
/// `MemorySpaceUpdateInput.name` is the *new* name: the merge in
/// [`crate::control::http`] answers 422 when a path param and a body field of
/// the same name disagree, which is precisely what a rename is. `delete` spells
/// it `space` too — axum will not accept two names for one path segment.
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct SpaceRef {
    /// Slug of the memory space, as it appears in the path.
    pub space: String,
}

/// `update` addresses a space from the path and carries the changes in the body.
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct UpdateSpace {
    /// Slug of the memory space to update.
    pub space: String,
    #[serde(flatten)]
    pub changes: MemorySpaceUpdateInput,
}

/// The named namespaces long-term memories live in.
pub struct MemorySpaces;

impl Resource for MemorySpaces {
    fn name(&self) -> &'static str {
        "memory-spaces"
    }

    fn operations(&self) -> Vec<Operation> {
        vec![
            op(
                "list",
                Method::Get,
                "/memory-spaces",
                "Every memory space, with how many memories each holds.",
                Expose::ApiAndTool,
                |s: Arc<ProjectServices>, _i: NoInput| async move {
                    s.memory.list_spaces().await.map_err(ControlError::Internal)
                },
            ),
            op(
                "create",
                Method::Post,
                "/memory-spaces",
                "Create a memory space. The name must be a slug: lowercase letters, \
             digits, '.', '_' and '-', starting with a letter or digit.",
                Expose::ApiAndTool,
                |s: Arc<ProjectServices>, i: MemorySpaceCreateInput| async move {
                    s.memory
                        .create_space(i)
                        .await
                        .map_err(ControlError::Invalid)
                },
            )
            .created(),
            op(
                "update",
                Method::Put,
                "/memory-spaces/{space}",
                "Rename a space and/or change its description. Omitted fields are \
             left as they are; a rename carries the space's memories across.",
                Expose::ApiAndTool,
                |s: Arc<ProjectServices>, i: UpdateSpace| async move {
                    s.memory
                        .update_space(&i.space, i.changes)
                        .await
                        .map_err(ControlError::Invalid)
                },
            ),
            op(
                "delete",
                Method::Delete,
                "/memory-spaces/{space}",
                "Delete a memory space and every memory in it.",
                Expose::ApiAndTool,
                |s: Arc<ProjectServices>, i: SpaceRef| async move {
                    s.memory
                        .delete_space(&i.space)
                        .await
                        .map_err(ControlError::NotFound)
                },
            )
            .no_content(),
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
        MemorySpaces.operations()
    }

    #[test]
    fn every_action_is_declared_once_on_one_resource() {
        let mut actions: Vec<&str> = operations().iter().map(|o| o.action).collect();
        actions.sort_unstable();
        assert_eq!(actions, ["create", "delete", "list", "update"]);
        assert_eq!(MemorySpaces.name(), "memory-spaces");
    }

    #[test]
    fn every_path_param_is_a_field_of_its_input() {
        crate::control::tests::assert_path_params_are_inputs(&operations());
    }
}
