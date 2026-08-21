//! The plugins resource: the installed bundle library, plus the slash commands
//! horsie answers itself.
//!
//! Serving a bundle's zip is not here. It is bytes rather than JSON, and it is
//! reached by a runtime holding only a dial token, so it stays a hand-written
//! route in `http::plugins`.

use crate::control::{ControlError, Expose, Method, NameRef, NoInput, Operation, Resource, op};
use crate::projects::ProjectServices;
use horsie_models::plugins::{CatalogEntryView, PluginDefaultInput, PluginInstallInput};
use std::sync::Arc;

/// The bundle is named by the path; what to set is the body. One type so the
/// route and a tool call see the same shape.
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct SetPluginDefault {
    /// Slug of the installed bundle.
    pub name: String,
    #[serde(flatten)]
    pub input: PluginDefaultInput,
}

/// The installed plugin-bundle library.
pub struct Plugins;

impl Resource for Plugins {
    fn name(&self) -> &'static str {
        "plugins"
    }

    fn operations(&self) -> Vec<Operation> {
        vec![
            op(
                "list",
                Method::Get,
                "/plugins",
                "Every installed bundle, with the skills, commands and MCP \
                 servers it contributes.",
                Expose::ApiAndTool,
                |s: Arc<ProjectServices>, _i: NoInput| async move {
                    s.plugins.list().await.map_err(ControlError::Internal)
                },
            ),
            op(
                "builtins",
                Method::Get,
                "/builtins",
                "The slash commands horsie answers itself. Offered whether or \
                 not any bundle is installed.",
                Expose::ApiAndTool,
                |_s: Arc<ProjectServices>, _i: NoInput| async move {
                    Ok::<Vec<CatalogEntryView>, ControlError>(
                        horsie_support::plugin::builtins::catalogue_entries(),
                    )
                },
            ),
            op(
                "install",
                Method::Post,
                "/plugins",
                "Install a bundle from a URL, or from a marketplace by name. A \
                 URL that turns out to declare several plugins is recorded as a \
                 marketplace instead, and the outcome says which happened.",
                Expose::ApiAndTool,
                |s: Arc<ProjectServices>, i: PluginInstallInput| async move {
                    s.plugins.install(i).await.map_err(ControlError::Invalid)
                },
            )
            .created(),
            op(
                "set-default",
                Method::Put,
                "/plugins/{name}",
                "Toggle whether a bundle is pre-selected for new sessions.",
                Expose::ApiAndTool,
                |s: Arc<ProjectServices>, i: SetPluginDefault| async move {
                    s.plugins
                        .set_default(&i.name, i.input)
                        .await
                        .map_err(ControlError::Invalid)
                },
            ),
            op(
                "delete",
                Method::Delete,
                "/plugins/{name}",
                "Remove a bundle and garbage-collect its artifact.",
                Expose::ApiAndTool,
                |s: Arc<ProjectServices>, i: NameRef| async move {
                    s.plugins
                        .remove(&i.name)
                        .await
                        .map_err(ControlError::Invalid)
                },
            )
            .no_content(),
            op(
                "update",
                Method::Post,
                "/plugins/{name}/update",
                "Re-clone a bundle from the source it was installed from.",
                Expose::ApiAndTool,
                |s: Arc<ProjectServices>, i: NameRef| async move {
                    s.plugins
                        .update(&i.name)
                        .await
                        .map_err(ControlError::Invalid)
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
        Plugins.operations()
    }

    #[test]
    fn every_action_is_declared_once_on_one_resource() {
        let mut actions: Vec<&str> = operations().iter().map(|o| o.action).collect();
        actions.sort_unstable();
        assert_eq!(
            actions,
            [
                "builtins",
                "delete",
                "install",
                "list",
                "set-default",
                "update"
            ]
        );
        assert_eq!(Plugins.name(), "plugins");
    }

    #[test]
    fn every_path_param_is_a_field_of_its_input() {
        crate::control::tests::assert_path_params_are_inputs(&operations());
    }
}
