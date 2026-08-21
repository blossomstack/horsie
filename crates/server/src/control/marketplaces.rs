//! The marketplaces resource: registered plugin catalogues.
//!
//! There is deliberately no `create` here. A marketplace is registered by
//! pasting its URL into `POST /api/plugins`, which is the one box the whole
//! design turns on; a second way in would be a second thing to keep consistent.

use crate::control::{ControlError, Expose, Method, NameRef, NoInput, Operation, Resource, op};
use crate::projects::ProjectServices;
use std::sync::Arc;

/// Registered plugin catalogues and what each one offers.
pub struct Marketplaces;

impl Resource for Marketplaces {
    fn name(&self) -> &'static str {
        "marketplaces"
    }

    fn operations(&self) -> Vec<Operation> {
        vec![
            op(
                "list",
                Method::Get,
                "/marketplaces",
                "Every registered source and its cached catalogue. The entries \
                 ride along, so picking a plugin needs no second call.",
                Expose::ApiAndTool,
                |s: Arc<ProjectServices>, _i: NoInput| async move {
                    s.plugins
                        .list_marketplaces()
                        .await
                        .map_err(ControlError::Internal)
                },
            ),
            op(
                "refresh",
                Method::Post,
                "/marketplaces/{name}/refresh",
                "Re-clone a source and re-read its index.",
                Expose::ApiAndTool,
                |s: Arc<ProjectServices>, i: NameRef| async move {
                    s.plugins
                        .refresh_marketplace(&i.name)
                        .await
                        .map_err(ControlError::Invalid)
                },
            ),
            op(
                "remove",
                Method::Delete,
                "/marketplaces/{name}",
                "Drop a source. Bundles installed from it stay installed.",
                Expose::ApiAndTool,
                |s: Arc<ProjectServices>, i: NameRef| async move {
                    s.plugins
                        .remove_marketplace(&i.name)
                        .await
                        .map_err(ControlError::Invalid)?;
                    Ok::<(), ControlError>(())
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
        Marketplaces.operations()
    }

    #[test]
    fn every_action_is_declared_once_on_one_resource() {
        let mut actions: Vec<&str> = operations().iter().map(|o| o.action).collect();
        actions.sort_unstable();
        assert_eq!(actions, ["list", "refresh", "remove"]);
        assert_eq!(Marketplaces.name(), "marketplaces");
    }

    #[test]
    fn every_path_param_is_a_field_of_its_input() {
        crate::control::tests::assert_path_params_are_inputs(&operations());
    }
}
