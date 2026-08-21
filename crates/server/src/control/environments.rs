//! The environments resource: reusable runtime + repos bundles.

use crate::control::{ControlError, Expose, Method, NameRef, NoInput, Operation, Resource, op};
use crate::projects::ProjectServices;
use horsie_models::environments::{EnvironmentInput, EnvironmentView};
use std::sync::Arc;

/// Reusable runtime + repos bundles.
pub struct Environments;

impl Resource for Environments {
    fn name(&self) -> &'static str {
        "environments"
    }

    fn operations(&self) -> Vec<Operation> {
        vec![
            op(
                "list",
                Method::Get,
                "/environments",
                "Every saved environment.",
                Expose::ApiAndTool,
                |s: Arc<ProjectServices>, _i: NoInput| async move {
                    Ok::<Vec<EnvironmentView>, ControlError>(s.environments.list().await?)
                },
            ),
            op(
                "get",
                Method::Get,
                "/environments/{name}",
                "One environment by slug.",
                Expose::ApiAndTool,
                |s: Arc<ProjectServices>, i: NameRef| async move {
                    Ok::<EnvironmentView, ControlError>(s.environments.get(&i.name).await?)
                },
            ),
            op(
                "create",
                Method::Post,
                "/environments",
                "Save a new environment: the runtime vendor to run on and the repos \
             to clone into its workspace.",
                Expose::ApiAndTool,
                |s: Arc<ProjectServices>, i: EnvironmentInput| async move {
                    Ok::<EnvironmentView, ControlError>(s.environments.create(i).await?)
                },
            )
            .created(),
            op(
                "replace",
                Method::Put,
                "/environments/{name}",
                "Replace an environment wholesale. The name is immutable — it is the \
             id of record.",
                Expose::ApiAndTool,
                |s: Arc<ProjectServices>, i: EnvironmentInput| async move {
                    let name = i.name.clone();
                    Ok::<EnvironmentView, ControlError>(s.environments.replace(&name, i).await?)
                },
            ),
            op(
                "delete",
                Method::Delete,
                "/environments/{name}",
                "Delete an environment.",
                Expose::ApiAndTool,
                |s: Arc<ProjectServices>, i: NameRef| async move {
                    s.environments.delete(&i.name).await?;
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
        Environments.operations()
    }

    #[test]
    fn every_action_is_declared_once_on_one_resource() {
        let mut actions: Vec<&str> = operations().iter().map(|o| o.action).collect();
        actions.sort_unstable();
        assert_eq!(actions, ["create", "delete", "get", "list", "replace"]);
        assert_eq!(Environments.name(), "environments");
    }

    #[test]
    fn every_path_param_is_a_field_of_its_input() {
        crate::control::tests::assert_path_params_are_inputs(&operations());
    }
}
