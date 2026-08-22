//! Plugins authored on this server, for the settings page.
//!
//! `Expose::Api` throughout, deliberately. Agents reach authoring through the
//! `authoring` tool group instead, so the grant is that group rather than the
//! control plane — which also carries this account's runtimes, models and
//! sessions, and is far more than writing a skill should imply.

use crate::control::{ControlError, Expose, Method, NameRef, NoInput, Operation, Resource, op};
use crate::projects::ProjectServices;
use horsie_models::plugins::{
    AuthoredPluginView, AuthoredPluginWriteInput, AuthoredSkillRestoreInput, AuthoredSkillView,
    AuthoredSkillWriteInput,
};
use std::sync::Arc;

/// A skill named by its plugin and its own name, for the routes that address
/// one without changing it.
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct SkillRef {
    pub name: String,
    pub skill: String,
}

pub struct AuthoredPlugins;

impl Resource for AuthoredPlugins {
    fn name(&self) -> &'static str {
        "authored-plugins"
    }

    fn operations(&self) -> Vec<Operation> {
        vec![
            op(
                "list",
                Method::Get,
                "/authored-plugins",
                "Every plugin authored on this server, with the skills it holds.",
                Expose::Api,
                |s: Arc<ProjectServices>, _i: NoInput| async move {
                    s.authored.list().await.map_err(ControlError::Internal)
                },
            ),
            op(
                "get",
                Method::Get,
                "/authored-plugins/{name}",
                "One authored plugin.",
                Expose::Api,
                |s: Arc<ProjectServices>, i: NameRef| async move {
                    s.authored.get(&i.name).await.map_err(ControlError::Invalid)
                },
            ),
            op(
                "create",
                Method::Post,
                "/authored-plugins",
                "Create a plugin to hold authored skills.",
                Expose::Api,
                |s: Arc<ProjectServices>, i: AuthoredPluginWriteInput| async move {
                    s.authored
                        .write_plugin(&i.name, i.description.as_deref())
                        .await
                        .map_err(ControlError::Invalid)
                },
            )
            .created(),
            op(
                "delete",
                Method::Delete,
                "/authored-plugins/{name}",
                "Delete an authored plugin and every skill in it.",
                Expose::Api,
                |s: Arc<ProjectServices>, i: NameRef| async move {
                    s.authored
                        .delete_plugin(&i.name)
                        .await
                        .map_err(ControlError::Invalid)
                },
            )
            .no_content(),
            op(
                "get-skill",
                Method::Get,
                "/authored-plugins/{name}/skills/{skill}",
                "One authored skill, body and files included.",
                Expose::Api,
                |s: Arc<ProjectServices>, i: SkillRef| async move {
                    s.authored
                        .get_skill(&i.name, &i.skill)
                        .await
                        .map_err(ControlError::Invalid)
                },
            ),
            op(
                "write-skill",
                Method::Put,
                "/authored-plugins/{name}/skills/{skill}",
                "Create or replace one skill. Omitted fields keep their current value.",
                Expose::Api,
                |s: Arc<ProjectServices>, i: WriteSkill| async move {
                    s.authored
                        .write_skill(AuthoredSkillWriteInput {
                            plugin: i.name,
                            name: i.skill,
                            description: i.input.description,
                            body: i.input.body,
                            files: i.input.files,
                        })
                        .await
                        .map_err(ControlError::Invalid)
                },
            ),
            op(
                "delete-skill",
                Method::Delete,
                "/authored-plugins/{name}/skills/{skill}",
                "Remove a skill, keeping its history.",
                Expose::Api,
                |s: Arc<ProjectServices>, i: SkillRef| async move {
                    s.authored
                        .delete_skill(&i.name, &i.skill)
                        .await
                        .map_err(ControlError::Invalid)
                },
            )
            .no_content(),
            op(
                "revisions",
                Method::Get,
                "/authored-plugins/{name}/skills/{skill}/revisions",
                "A skill's past revisions, newest first.",
                Expose::Api,
                |s: Arc<ProjectServices>, i: SkillRef| async move {
                    s.authored
                        .revisions(&i.name, &i.skill)
                        .await
                        .map_err(ControlError::Invalid)
                },
            ),
            op(
                "restore",
                Method::Post,
                "/authored-plugins/{name}/skills/{skill}/restore",
                "Put a skill back to one of its revisions.",
                Expose::Api,
                |s: Arc<ProjectServices>, i: RestoreSkill| async move {
                    s.authored
                        .restore_skill(AuthoredSkillRestoreInput {
                            plugin: i.name,
                            name: i.skill,
                            revision: i.revision,
                        })
                        .await
                        .map_err(ControlError::Invalid)
                },
            ),
        ]
    }
}

/// The skill is named by the path; what to write is the body. One type so the
/// route and its handler see the same shape.
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct WriteSkill {
    pub name: String,
    pub skill: String,
    #[serde(flatten)]
    pub input: SkillBody,
}

/// The writable half of a skill, without the addressing.
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct SkillBody {
    pub description: Option<String>,
    pub body: Option<String>,
    pub files: Option<Vec<horsie_models::plugins::AuthoredFileView>>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct RestoreSkill {
    pub name: String,
    pub skill: String,
    pub revision: u64,
}

/// Unused imports guard: these are the types the operations answer with, named
/// here so a rename cannot silently leave a route returning something else.
#[allow(dead_code)]
fn _answers(_: AuthoredPluginView, _: AuthoredSkillView) {}
