//! The routines resource: an agent preset plus a schedule.

use crate::control::{
    ControlError, Expose, Method, NameRef, NoInput, Operation, Resource, ask, op,
};
use crate::sessions::supervisor::SessionSupervisorCommand;
use crate::users::UserServices;
use horsie_models::now_ms;
use horsie_models::routines::{RoutineInput, RoutineRunResponse, RoutineView};
use std::sync::Arc;

/// An agent preset plus a schedule.
pub struct Routines;

impl Resource for Routines {
    fn name(&self) -> &'static str {
        "routines"
    }

    fn operations(&self) -> Vec<Operation> {
        vec![
            op(
                "list",
                Method::Get,
                "/api/routines",
                "Every saved routine.",
                Expose::ApiAndTool,
                |s: Arc<UserServices>, _i: NoInput| async move {
                    Ok::<Vec<RoutineView>, ControlError>(s.routines.list().await?)
                },
            ),
            op(
                "get",
                Method::Get,
                "/api/routines/{name}",
                "One routine by slug.",
                Expose::ApiAndTool,
                |s: Arc<UserServices>, i: NameRef| async move {
                    Ok::<RoutineView, ControlError>(s.routines.get(&i.name).await?)
                },
            ),
            op(
                "create",
                Method::Post,
                "/api/routines",
                "Save a new routine: an agent preset, a fixed prompt, and when to \
             fire it. The agent preset must already exist.",
                Expose::ApiAndTool,
                |s: Arc<UserServices>, i: RoutineInput| async move {
                    Ok::<RoutineView, ControlError>(s.routines.create(i, now_ms()).await?)
                },
            )
            .created(),
            op(
                "replace",
                Method::Put,
                "/api/routines/{name}",
                "Replace a routine wholesale. The name is immutable — it is the id \
             of record.",
                Expose::ApiAndTool,
                |s: Arc<UserServices>, i: RoutineInput| async move {
                    let name = i.name.clone();
                    Ok::<RoutineView, ControlError>(s.routines.replace(&name, i, now_ms()).await?)
                },
            ),
            op(
                "delete",
                Method::Delete,
                "/api/routines/{name}",
                "Delete a routine and every session it created.",
                Expose::ApiAndTool,
                |s: Arc<UserServices>, i: NameRef| async move { delete(&s, &i.name).await },
            )
            .no_content(),
            op(
                "run",
                Method::Post,
                "/api/routines/{name}/run",
                "Fire a routine now, whatever its schedule says and whether or not \
             it is enabled.",
                Expose::ApiAndTool,
                |s: Arc<UserServices>, i: NameRef| async move {
                    let session = s.routine_runner.run(&i.name, now_ms()).await?;
                    Ok::<RoutineRunResponse, ControlError>(RoutineRunResponse { session })
                },
            )
            .created(),
        ]
    }
}

/// A routine's runs go with it. Best effort per session: a routine whose delete
/// half-failed is worse than one whose runs outlive it by a restart.
async fn delete(services: &UserServices, name: &str) -> Result<(), ControlError> {
    services.routines.get(name).await?;
    let sessions = ask(services, |reply| SessionSupervisorCommand::List { reply }).await?;
    let ids: Vec<String> = sessions
        .iter()
        .filter(|(_, rec)| rec.spec.routine() == Some(name))
        .map(|(id, _)| id.clone())
        .collect();
    for id in ids {
        if let Err(e) = ask(services, |reply| SessionSupervisorCommand::Delete {
            id: id.clone(),
            reply,
        })
        .await?
        {
            tracing::warn!(routine = %name, session = %id, error = %e, "deleting a routine run failed");
        }
    }
    services.routines.delete(name).await?;
    Ok(())
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
        Routines.operations()
    }

    #[test]
    fn every_action_is_declared_once_on_one_resource() {
        let mut actions: Vec<&str> = operations().iter().map(|o| o.action).collect();
        actions.sort_unstable();
        assert_eq!(
            actions,
            ["create", "delete", "get", "list", "replace", "run"]
        );
        assert_eq!(Routines.name(), "routines");
    }

    #[test]
    fn every_path_param_is_a_field_of_its_input() {
        crate::control::tests::assert_path_params_are_inputs(&operations());
    }
}
