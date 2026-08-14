//! The runtime-vendors resource: the vendors the server builds itself.
//!
//! Only configured vendors appear here. A vendor that dials in announces itself
//! and is listed in the settings view instead — there is nothing to create or
//! delete about a process someone else is running.

use crate::control::{ControlError, Expose, Method, NameRef, NoInput, Operation, Resource, op};
use crate::runtime_vendor::config::VendorConfigError;
use crate::users::UserServices;
use horsie_models::runtime_vendor::RuntimeVendorConfigInput;
use std::sync::Arc;

/// `name_in_use` is the envelope code these routes have always answered with,
/// and a client branching on it must keep working — which is why this is spelled
/// out here rather than folded into the `duplicate`/`conflict` macro in
/// [`crate::control`].
fn control_error(e: VendorConfigError) -> ControlError {
    match e {
        VendorConfigError::NotFound(m) => ControlError::NotFound(m),
        VendorConfigError::Conflict(m) => ControlError::Conflict {
            code: "name_in_use".to_string(),
            message: m,
        },
        VendorConfigError::Invalid(m) => ControlError::Invalid(m),
        VendorConfigError::Internal(m) => ControlError::Internal(m),
    }
}

/// The vendors the server builds itself, as configured in settings.
pub struct RuntimeVendors;

impl Resource for RuntimeVendors {
    fn name(&self) -> &'static str {
        "runtime-vendors"
    }

    fn operations(&self) -> Vec<Operation> {
        vec![
            op(
                "list",
                Method::Get,
                "/api/runtime-vendors",
                "Every configured runtime vendor. A stored credential shows only as \
             `hasCredential` — the token itself never leaves the server.",
                Expose::ApiAndTool,
                |s: Arc<UserServices>, _i: NoInput| async move {
                    s.runtime_vendors.list_views().await.map_err(control_error)
                },
            ),
            // Api-only: `RuntimeVendorConfigInput.credential` carries the
            // vendor's substrate token, and a tool schema is an invitation for
            // the model to invent one and put it in a conversation transcript.
            // A human types this on the settings page.
            op(
                "save",
                Method::Put,
                "/api/runtime-vendors/{name}",
                "Create or fully replace a configured runtime vendor. One verb \
             rather than POST-then-PUT: a vendor row is a connection setting keyed \
             by its name, and re-saving one is how a rotated token is applied. Omit \
             the credential to keep the stored one.",
                Expose::Api,
                |s: Arc<UserServices>, i: RuntimeVendorConfigInput| async move {
                    let name = i.name.clone();
                    s.runtime_vendors
                        .save_input(&name, i)
                        .await
                        .map_err(control_error)
                },
            ),
            op(
                "test",
                Method::Post,
                "/api/runtime-vendors/{name}/test",
                "Ask the substrate whether this vendor is usable right now, without \
             creating anything. The substrate saying no is `ok: false` with a \
             message, not an error — the request itself succeeded.",
                Expose::ApiAndTool,
                |s: Arc<UserServices>, i: NameRef| async move {
                    // A name nothing is configured under is a different thing
                    // entirely from a vendor that answered badly, so it 404s.
                    s.runtime_vendors
                        .test_named(&i.name)
                        .await
                        .map_err(control_error)?
                        .ok_or_else(|| {
                            ControlError::NotFound(format!("no runtime vendor named '{}'", i.name))
                        })
                },
            ),
            op(
                "delete",
                Method::Delete,
                "/api/runtime-vendors/{name}",
                "Delete a configured runtime vendor.",
                Expose::ApiAndTool,
                |s: Arc<UserServices>, i: NameRef| async move {
                    s.runtime_vendors
                        .delete_named(&i.name)
                        .await
                        .map_err(control_error)
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
        RuntimeVendors.operations()
    }

    #[test]
    fn every_action_is_declared_once_on_one_resource() {
        let mut actions: Vec<&str> = operations().iter().map(|o| o.action).collect();
        actions.sort_unstable();
        assert_eq!(actions, ["delete", "list", "save", "test"]);
        assert_eq!(RuntimeVendors.name(), "runtime-vendors");
    }

    #[test]
    fn every_path_param_is_a_field_of_its_input() {
        crate::control::tests::assert_path_params_are_inputs(&operations());
    }

    /// A credential the model can write is a credential that ends up in a
    /// transcript, so `save` is the one operation here with no tool.
    #[test]
    fn saving_a_vendor_is_not_a_tool() {
        let save = operations()
            .into_iter()
            .find(|o| o.action == "save")
            .expect("save is declared");
        assert_eq!(save.expose, Expose::Api);
    }
}
