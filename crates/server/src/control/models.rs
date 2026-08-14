//! The models resource: the LLM configuration a session routes through —
//! provider connections, the aliases that name a model on one of them, the
//! default runtime vendor, and the model-card catalogue the alias form
//! autocompletes against.

use crate::config::model_cards::ModelCardError;
use crate::control::{ControlError, Expose, Method, NameRef, NoInput, Operation, Resource, op};
use crate::users::UserServices;
use horsie_models::settings::{
    DefaultRuntimeVendorInput, ModelInput, ModelView, ProviderInput, ProviderView,
};
use std::sync::Arc;

/// A model addressed by its alias. `{alias}` rather than `{name}`, so
/// [`NameRef`] does not fit.
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct ModelRef {
    /// The model alias, as it appears in the path.
    pub alias: String,
}

/// Prefix search over the model-card catalogue.
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct CardQuery {
    /// Return only cards whose model id starts with this. Absent means all of
    /// them.
    pub prefix: Option<String>,
}

/// The LLM configuration: providers, model aliases, and the default vendor.
pub struct Models;

impl Resource for Models {
    fn name(&self) -> &'static str {
        "models"
    }

    fn operations(&self) -> Vec<Operation> {
        vec![
            op(
                "list",
                Method::Get,
                "/api/config/models",
                "Every configured model alias. These are the names an agent \
                 preset or a session selects a model by.",
                Expose::ApiAndTool,
                |s: Arc<UserServices>, _i: NoInput| async move {
                    let view = s
                        .config_store
                        .view()
                        .await
                        .map_err(ControlError::Internal)?;
                    Ok::<Vec<ModelView>, ControlError>(view.models)
                },
            ),
            op(
                "replace",
                Method::Put,
                "/api/config/models/{alias}",
                "Create or replace one model alias. `provider` must name a \
                 configured provider.",
                Expose::ApiAndTool,
                |s: Arc<UserServices>, i: ModelInput| async move {
                    s.config_store
                        .upsert_model(i)
                        .await
                        .map_err(ControlError::Invalid)
                },
            ),
            op(
                "delete",
                Method::Delete,
                "/api/config/models/{alias}",
                "Delete a model alias. Sessions and presets naming it stop \
                 resolving.",
                Expose::ApiAndTool,
                |s: Arc<UserServices>, i: ModelRef| async move {
                    // An unknown alias is a 404 rather than a 422: the caller
                    // named a thing, not a malformed thing.
                    s.config_store.delete_model(&i.alias).await.map_err(|e| {
                        if e.starts_with("no such model") {
                            ControlError::NotFound(e)
                        } else {
                            ControlError::Invalid(e)
                        }
                    })
                },
            )
            .no_content(),
            op(
                "list-providers",
                Method::Get,
                "/api/config/model-providers",
                "Every configured LLM provider. Credentials are never returned \
                 — `has_credential` reports only whether one is stored.",
                Expose::ApiAndTool,
                |s: Arc<UserServices>, _i: NoInput| async move {
                    let view = s
                        .config_store
                        .view()
                        .await
                        .map_err(ControlError::Internal)?;
                    Ok::<Vec<ProviderView>, ControlError>(view.providers)
                },
            ),
            // Api-only, both of them: `ProviderInput` carries `api_key`, so
            // exposing either as a tool would hand a model the ability to write
            // an API key it chose — or to overwrite ours with one pointing at a
            // `base_url` it controls. Reading providers is safe because
            // `ProviderView` redacts to `has_credential: bool`; writing them is
            // not, and no summary wording makes it so.
            op(
                "replace-provider",
                Method::Put,
                "/api/config/model-providers/{name}",
                "Create or replace one provider, including its API key.",
                Expose::Api,
                |s: Arc<UserServices>, i: ProviderInput| async move {
                    s.config_store
                        .upsert_provider(i)
                        .await
                        .map_err(ControlError::Invalid)
                },
            ),
            op(
                "delete-provider",
                Method::Delete,
                "/api/config/model-providers/{name}",
                "Delete a provider and, with it, every model that routed \
                 through it.",
                Expose::Api,
                |s: Arc<UserServices>, i: NameRef| async move {
                    s.config_store.delete_provider(&i.name).await.map_err(|e| {
                        if e.starts_with("no such provider") {
                            ControlError::NotFound(e)
                        } else {
                            ControlError::Invalid(e)
                        }
                    })
                },
            )
            .no_content(),
            op(
                "get-config",
                Method::Get,
                "/api/config",
                "The whole settings snapshot: providers, models, the live \
                 vendor roster, and this deployment's paths and version.",
                Expose::ApiAndTool,
                |s: Arc<UserServices>, _i: NoInput| async move {
                    s.config_store.view().await.map_err(ControlError::Internal)
                },
            ),
            op(
                "set-default-runtime-vendor",
                Method::Put,
                "/api/config/default-runtime-vendor",
                "Set the runtime vendor new sessions use when they name none.",
                Expose::ApiAndTool,
                |s: Arc<UserServices>, i: DefaultRuntimeVendorInput| async move {
                    s.config_store
                        .set_default_runtime_vendor(&i.vendor)
                        .await
                        .map_err(ControlError::Invalid)
                },
            ),
            // Answers 200 with the whole settings view, not 204: the caller is
            // the Settings page, which renders `isDefault` per vendor and would
            // otherwise have to refetch to redraw the row it just cleared.
            op(
                "clear-default-runtime-vendor",
                Method::Delete,
                "/api/config/default-runtime-vendor",
                "Forget the default runtime vendor and fall back to the \
                 built-in one. Distinct from setting it to an empty string, \
                 which is refused.",
                Expose::ApiAndTool,
                |s: Arc<UserServices>, _i: NoInput| async move {
                    s.config_store
                        .clear_default_runtime_vendor()
                        .await
                        .map_err(ControlError::Invalid)
                },
            ),
            op(
                "list-cards",
                Method::Get,
                "/api/model-cards",
                "Known model ids and their context windows, by prefix. Use this \
                 to find the `model_id` for a new alias.",
                Expose::ApiAndTool,
                |s: Arc<UserServices>, i: CardQuery| async move {
                    s.model_cards
                        .search_by_prefix(i.prefix.as_deref().unwrap_or(""))
                        .await
                        .map_err(card_error)
                },
            ),
        ]
    }
}

/// The card store's own error vocabulary, in the control plane's. Not a `From`
/// impl because the conversion lives with the one resource that reads cards.
fn card_error(e: ModelCardError) -> ControlError {
    match e {
        ModelCardError::Invalid(m) => ControlError::Invalid(m),
        ModelCardError::Duplicate(m) => ControlError::Conflict {
            code: "duplicate_model_id".to_string(),
            message: m,
        },
        ModelCardError::NotFound(m) => ControlError::NotFound(m),
        ModelCardError::Db(m) => ControlError::Internal(m),
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
        Models.operations()
    }

    #[test]
    fn every_action_is_declared_once_on_one_resource() {
        let mut actions: Vec<&str> = operations().iter().map(|o| o.action).collect();
        actions.sort_unstable();
        assert_eq!(
            actions,
            [
                "clear-default-runtime-vendor",
                "delete",
                "delete-provider",
                "get-config",
                "list",
                "list-cards",
                "list-providers",
                "replace",
                "replace-provider",
                "set-default-runtime-vendor",
            ]
        );
        assert_eq!(Models.name(), "models");
    }

    #[test]
    fn every_path_param_is_a_field_of_its_input() {
        crate::control::tests::assert_path_params_are_inputs(&operations());
    }

    #[test]
    fn writing_a_provider_is_never_a_tool() {
        // `ProviderInput` carries an API key. A regression here would put key
        // material in a model's reach without any other test noticing.
        for operation in operations() {
            if operation.action.ends_with("-provider") {
                assert_eq!(
                    operation.expose,
                    Expose::Api,
                    "{} must not reach the toolbox",
                    operation.action
                );
            }
        }
    }
}
