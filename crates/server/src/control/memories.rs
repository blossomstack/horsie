//! The memories resource: the contents of memory spaces, across all of them.
//!
//! A second vocabulary over the same rows the `memory_*` tools reach, and
//! deliberately so — they answer different questions.
//!
//! `memory_*` is *my* memory. It is implicitly scoped to the spaces its session
//! was created with, never names one, and belongs to an agent curating what it
//! has learned. `horsie_memories` is curation of *any* space, by something that
//! is not the agent whose memory it is: a scheduled job reading an agent's past
//! runs and deciding a memory it wrote is now wrong. That caller cannot know
//! which spaces to ask for at the time its own session is created, because the
//! agent it is tuning is chosen at run time.
//!
//! The same split already exists one level up, between `memory_*` and
//! `horsie_memory-spaces`. This is its other half.
//!
//! It is gated by the control-plane grant, which is the right weight: rewriting
//! another agent's memory is authority, not convenience.

use crate::control::{ControlError, Expose, Method, Operation, Resource, op};
use crate::projects::ProjectServices;
use horsie_models::agents::{RevisionView, RevisionsPage};
use horsie_models::memory::{MemoryCreateInput, MemoryUpdateInput, MemoryView};
use std::sync::Arc;

#[derive(serde::Deserialize, schemars::JsonSchema, Default)]
pub struct ListMemories {
    /// Only memories in this space. Absent lists every space's.
    pub space: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct MemoryRef {
    /// The memory's numeric id, as `list` reports it.
    pub id: u64,
}

/// `update` addresses a memory from the path and carries the changes in the
/// body.
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct UpdateMemory {
    pub id: u64,
    #[serde(flatten)]
    pub changes: MemoryUpdateInput,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct RestoreMemory {
    pub id: u64,
    /// Which past version to put back, as `revisions` numbers them.
    pub revision: u64,
}

/// The memories inside spaces, readable and writable across all of them.
pub struct Memories;

impl Resource for Memories {
    fn name(&self) -> &'static str {
        "memories"
    }

    fn operations(&self) -> Vec<Operation> {
        vec![
            op(
                "list",
                Method::Get,
                "/memories",
                "Every memory, or every memory in one space. Bodies included — \
                 narrow with `space` on an account with many.",
                Expose::ToolOnly,
                |s: Arc<ProjectServices>, i: ListMemories| async move {
                    s.memory
                        .list_memories(i.space.as_deref())
                        .await
                        .map_err(ControlError::Internal)
                },
            ),
            op(
                "get",
                Method::Get,
                "/memories/{id}",
                "One memory, body included.",
                Expose::ToolOnly,
                |s: Arc<ProjectServices>, i: MemoryRef| async move {
                    s.memory
                        .get_memory(id_of(i.id))
                        .await
                        .map_err(ControlError::NotFound)
                },
            ),
            op(
                "create",
                Method::Post,
                "/memories",
                "Write a new memory into a space. The space must already exist.",
                Expose::ToolOnly,
                |s: Arc<ProjectServices>, i: MemoryCreateInput| async move {
                    s.memory
                        .create_memory(i)
                        .await
                        .map_err(ControlError::Invalid)
                },
            )
            .created(),
            op(
                "update",
                Method::Put,
                "/memories/{id}",
                "Change a memory's description, its body, or both. Omitted \
                 fields are left as they are. Pass `expected_revision` — the \
                 `revision` you read — and the write is refused if the memory \
                 changed in between.",
                Expose::ToolOnly,
                |s: Arc<ProjectServices>, i: UpdateMemory| async move {
                    s.memory
                        .update_memory(id_of(i.id), i.changes)
                        .await
                        .map_err(update_failed)
                },
            ),
            op(
                "delete",
                Method::Delete,
                "/memories/{id}",
                "Remove a memory. Its content stays readable through \
                 `revisions`, but it cannot be restored in place — a new \
                 memory would get a new id.",
                Expose::ToolOnly,
                |s: Arc<ProjectServices>, i: MemoryRef| async move {
                    s.memory
                        .delete_memory(id_of(i.id))
                        .await
                        .map_err(ControlError::NotFound)
                },
            )
            .no_content(),
            op(
                "revisions",
                Method::Get,
                "/memories/{id}/revisions",
                "A memory's past versions, newest first — including the one \
                 that recorded its deletion.",
                Expose::ToolOnly,
                |s: Arc<ProjectServices>, i: MemoryRef| async move {
                    Ok::<RevisionsPage, ControlError>(RevisionsPage {
                        revisions: s
                            .memory
                            .revisions(id_of(i.id))
                            .await
                            .map_err(ControlError::Internal)?
                            .into_iter()
                            .map(wire_revision)
                            .collect(),
                    })
                },
            ),
            op(
                "restore",
                Method::Post,
                "/memories/{id}/restore",
                "Put a memory back to one of its past versions, recorded as a \
                 new revision rather than a rewind.",
                Expose::ToolOnly,
                |s: Arc<ProjectServices>, i: RestoreMemory| async move {
                    s.memory
                        .restore_memory(id_of(i.id), i64::try_from(i.revision).unwrap_or(i64::MAX))
                        .await
                        .map_err(ControlError::Invalid)
                },
            ),
        ]
    }
}

/// A memory id as the store keys it.
fn id_of(id: u64) -> i64 {
    i64::try_from(id).unwrap_or(i64::MAX)
}

/// An update refusal, split so a stale write reads as one.
///
/// `MemoryService` answers in strings, so this is a substring match — which is
/// ugly and is why the check is anchored on the one phrase `CasError` always
/// produces. Fixing it properly means giving that service a typed error, which
/// is a refactor of every one of its callers and not this change.
fn update_failed(message: String) -> ControlError {
    if message.contains("was changed since you read it")
        || message.contains("has no revision history")
    {
        return ControlError::Conflict {
            code: "stale_revision".to_string(),
            message,
        };
    }
    ControlError::Invalid(message)
}

fn wire_revision(r: crate::revisions::Revision) -> RevisionView {
    RevisionView {
        revision: u64::try_from(r.revision).unwrap_or(0),
        payload: r.payload,
        deleted: r.deleted,
        created_at: r.created_at,
    }
}

/// The type the operations answer with, named so a rename cannot silently leave
/// a tool returning something else.
#[allow(dead_code)]
fn _answers(_: MemoryView) {}

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
        Memories.operations()
    }

    fn find(action: &str) -> Operation {
        operations()
            .into_iter()
            .find(|o| o.action == action)
            .unwrap()
    }

    #[test]
    fn every_action_is_declared_once_on_one_resource() {
        let mut actions: Vec<&str> = operations().iter().map(|o| o.action).collect();
        actions.sort_unstable();
        assert_eq!(
            actions,
            [
                "create",
                "delete",
                "get",
                "list",
                "restore",
                "revisions",
                "update"
            ]
        );
        assert_eq!(Memories.name(), "memories");
    }

    #[test]
    fn every_path_param_is_a_field_of_its_input() {
        crate::control::tests::assert_path_params_are_inputs(&operations());
    }

    /// `http::memory` already mounts these paths for the web UI. Declaring them
    /// as routes here as well would panic axum at boot on a duplicate — which
    /// is what `control::http`'s own fold test guards, but only for operations
    /// it can see.
    #[test]
    fn the_routes_stay_where_the_web_ui_already_mounts_them() {
        assert!(
            operations().iter().all(|o| o.expose == Expose::ToolOnly),
            "these paths are hand-mounted in http::memory; a second claim on \
             them is a boot panic, not a test failure"
        );
    }

    async fn services() -> (Arc<ProjectServices>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::testing::state(dir.path()).build().await;
        let services = state.services().await;
        services
            .memory
            .create_space(horsie_models::memory::MemorySpaceCreateInput {
                name: "notes".into(),
                description: None,
            })
            .await
            .unwrap();
        (services, dir)
    }

    async fn write(services: &Arc<ProjectServices>, content: &str) -> serde_json::Value {
        find("create")
            .run(
                services.clone(),
                serde_json::json!({
                    "space": "notes", "name": "deploys",
                    "description": "how deploys go", "content": content
                }),
            )
            .await
            .unwrap()
    }

    /// The whole reason this resource exists: a caller that is not the agent
    /// whose memory this is can read and rewrite it, naming the space.
    #[tokio::test]
    async fn a_memory_in_any_space_can_be_read_and_rewritten() {
        let (services, _dir) = services().await;
        let created = write(&services, "deploy on fridays").await;
        let id = created["id"].as_u64().unwrap();

        let listed = find("list")
            .run(services.clone(), serde_json::json!({"space": "notes"}))
            .await
            .unwrap();
        assert_eq!(listed.as_array().unwrap().len(), 1);

        find("update")
            .run(
                services.clone(),
                serde_json::json!({"id": id, "content": "never deploy on fridays"}),
            )
            .await
            .unwrap();
        let now = find("get")
            .run(services, serde_json::json!({"id": id}))
            .await
            .unwrap();
        assert_eq!(now["content"], "never deploy on fridays");
        assert_eq!(now["description"], "how deploys go", "omitted stays put");
    }

    /// Two writers over one memory — the session that owns it and something
    /// curating it — is exactly the case a revision closes.
    #[tokio::test]
    async fn a_stale_update_is_refused_and_changes_nothing() {
        let (services, _dir) = services().await;
        let created = write(&services, "v1").await;
        let id = created["id"].as_u64().unwrap();
        let revision = created["revision"].as_u64().unwrap();

        find("update")
            .run(
                services.clone(),
                serde_json::json!({"id": id, "content": "theirs"}),
            )
            .await
            .unwrap();

        let err = find("update")
            .run(
                services.clone(),
                serde_json::json!({
                    "id": id, "content": "mine", "expectedRevision": revision
                }),
            )
            .await
            .unwrap_err();
        match err {
            ControlError::Conflict { code, .. } => assert_eq!(code, "stale_revision"),
            other => panic!("expected a stale-revision conflict, got {other:?}"),
        }

        let now = find("get")
            .run(services, serde_json::json!({"id": id}))
            .await
            .unwrap();
        assert_eq!(now["content"], "theirs");
    }

    #[tokio::test]
    async fn a_memory_can_be_put_back_to_what_it_used_to_say() {
        let (services, _dir) = services().await;
        let id = write(&services, "v1").await["id"].as_u64().unwrap();
        find("update")
            .run(
                services.clone(),
                serde_json::json!({"id": id, "content": "v2"}),
            )
            .await
            .unwrap();

        let restored = find("restore")
            .run(services, serde_json::json!({"id": id, "revision": 1}))
            .await
            .unwrap();
        assert_eq!(restored["content"], "v1");
        assert_eq!(restored["revision"], 3, "a restore is a new version");
    }

    /// A deleted memory's history stays readable, but restoring it in place is
    /// refused rather than quietly creating a different memory under a new id.
    #[tokio::test]
    async fn a_deleted_memory_is_readable_but_not_restorable_in_place() {
        let (services, _dir) = services().await;
        let id = write(&services, "v1").await["id"].as_u64().unwrap();
        find("delete")
            .run(services.clone(), serde_json::json!({"id": id}))
            .await
            .unwrap();

        let history = find("revisions")
            .run(services.clone(), serde_json::json!({"id": id}))
            .await
            .unwrap();
        assert_eq!(history["revisions"][0]["deleted"], true);
        assert!(
            history["revisions"][0]["payload"]
                .as_str()
                .unwrap()
                .contains("v1")
        );

        let err = find("restore")
            .run(services, serde_json::json!({"id": id, "revision": 1}))
            .await
            .unwrap_err();
        assert!(matches!(err, ControlError::Invalid(_)), "{err:?}");
    }
}
