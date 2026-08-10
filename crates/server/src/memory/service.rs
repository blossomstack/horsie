//! Validation, timestamps, and row→wire mapping over `MemoryStore`. Also the
//! agent-facing reads (`memories_in`, `get_by_ref`) the toolbox and the prompt
//! index use, so the session layer never touches the store directly.

use crate::memory::{
    MAX_CONTENT_CHARS, MAX_DESCRIPTION_CHARS, MemoryRow, MemorySpaceRow, MemoryStore, validate_slug,
};
use horsie_models::memory::{
    MemoryCreateInput, MemorySpaceCreateInput, MemorySpaceUpdateInput, MemorySpaceView,
    MemoryUpdateInput, MemoryView,
};

pub struct MemoryService {
    store: MemoryStore,
}

impl MemoryService {
    pub fn new(store: MemoryStore) -> Self {
        Self { store }
    }

    // --- spaces ---

    pub async fn list_spaces(&self) -> Result<Vec<MemorySpaceView>, String> {
        let spaces = self.store.list_spaces().await?;
        let all = self.store.list_memories(None).await?;
        Ok(spaces
            .into_iter()
            .map(|s| {
                let count = all.iter().filter(|m| m.space == s.name).count();
                space_view(&s, count)
            })
            .collect())
    }

    pub async fn create_space(
        &self,
        input: MemorySpaceCreateInput,
    ) -> Result<MemorySpaceView, String> {
        validate_slug(&input.name)?;
        // Asked here rather than left to the UNIQUE index, which surfaced as a
        // raw SQLite constraint message naming the internal table and columns —
        // the one validation on this page that did not return a sentence.
        if self.store.get_space(&input.name).await?.is_some() {
            return Err(format!(
                "a memory space named '{}' already exists",
                input.name
            ));
        }
        let now = now_secs();
        self.store
            .create_space(&MemorySpaceRow {
                name: input.name.clone(),
                description: input.description.unwrap_or_default(),
                created_at: now.clone(),
                updated_at: now,
            })
            .await?;
        self.space_view_of(&input.name).await
    }

    /// Rename and/or re-describe. The rename runs first so the description
    /// update lands on the new row.
    pub async fn update_space(
        &self,
        name: &str,
        input: MemorySpaceUpdateInput,
    ) -> Result<MemorySpaceView, String> {
        if self.store.get_space(name).await?.is_none() {
            return Err(format!("unknown memory space '{name}'"));
        }
        let now = now_secs();
        let mut current = name.to_string();
        if let Some(new_name) = input.name
            && new_name != current
        {
            validate_slug(&new_name)?;
            self.store.rename_space(&current, &new_name, &now).await?;
            current = new_name;
        }
        if let Some(description) = input.description {
            self.store
                .update_space_description(&current, &description, &now)
                .await?;
        }
        self.space_view_of(&current).await
    }

    pub async fn delete_space(&self, name: &str) -> Result<(), String> {
        if self.store.delete_space(name).await? {
            Ok(())
        } else {
            Err(format!("unknown memory space '{name}'"))
        }
    }

    async fn space_view_of(&self, name: &str) -> Result<MemorySpaceView, String> {
        let row = self
            .store
            .get_space(name)
            .await?
            .ok_or_else(|| format!("unknown memory space '{name}'"))?;
        let count = self.store.list_memories(Some(name)).await?.len();
        Ok(space_view(&row, count))
    }

    // --- memories ---

    pub async fn list_memories(&self, space: Option<&str>) -> Result<Vec<MemoryView>, String> {
        Ok(self
            .store
            .list_memories(space)
            .await?
            .iter()
            .map(memory_view)
            .collect())
    }

    pub async fn get_memory(&self, id: i64) -> Result<MemoryView, String> {
        self.store
            .get_memory(id)
            .await?
            .as_ref()
            .map(memory_view)
            .ok_or_else(|| format!("no memory with id {id}"))
    }

    pub async fn create_memory(&self, input: MemoryCreateInput) -> Result<MemoryView, String> {
        validate_slug(&input.space)?;
        validate_slug(&input.name)?;
        check_description(&input.description)?;
        check_content(&input.content)?;
        let now = now_secs();
        let id = self
            .store
            .create_memory(&MemoryRow {
                id: 0,
                space: input.space,
                name: input.name,
                description: input.description,
                content: input.content,
                created_at: now.clone(),
                updated_at: now,
            })
            .await?;
        self.get_memory(id).await
    }

    pub async fn update_memory(
        &self,
        id: i64,
        input: MemoryUpdateInput,
    ) -> Result<MemoryView, String> {
        if let Some(d) = input.description.as_deref() {
            check_description(d)?;
        }
        // Checked on update as well as create. It was only checked on create,
        // so the edit form happily saved an empty body the create form had
        // refused — and a memory with no body is one the agent loads to learn
        // nothing.
        if let Some(c) = input.content.as_deref() {
            check_content(c)?;
        }
        let changed = self
            .store
            .update_memory(
                id,
                input.description.as_deref(),
                input.content.as_deref(),
                &now_secs(),
            )
            .await?;
        if !changed {
            return Err(format!("no memory with id {id}"));
        }
        self.get_memory(id).await
    }

    pub async fn delete_memory(&self, id: i64) -> Result<(), String> {
        if self.store.delete_memory(id).await? {
            Ok(())
        } else {
            Err(format!("no memory with id {id}"))
        }
    }

    // --- agent-facing reads ---

    /// Rows across a session's selected spaces, for the prompt index and
    /// `memory_list`.
    pub async fn memories_in(&self, spaces: &[String]) -> Result<Vec<MemoryRow>, String> {
        self.store.memories_in(spaces).await
    }

    /// Resolve a `space/name` address.
    pub async fn get_by_ref(&self, space: &str, name: &str) -> Result<Option<MemoryRow>, String> {
        self.store.get_memory_by_ref(space, name).await
    }
}

/// A memory's body. Capped because a body is loaded verbatim into a turn on
/// request, and nothing else bounded it — 100 KB was accepted, which is a
/// context window's worth of one memory.
fn check_content(c: &str) -> Result<(), String> {
    if c.trim().is_empty() {
        return Err("content must not be empty".to_string());
    }
    if c.chars().count() > MAX_CONTENT_CHARS {
        return Err(format!(
            "content must be at most {MAX_CONTENT_CHARS} characters (got {})",
            c.chars().count()
        ));
    }
    Ok(())
}

fn check_description(d: &str) -> Result<(), String> {
    if d.trim().is_empty() {
        return Err("description must not be empty".to_string());
    }
    if d.chars().count() > MAX_DESCRIPTION_CHARS {
        return Err(format!(
            "description must be at most {MAX_DESCRIPTION_CHARS} characters (got {})",
            d.chars().count()
        ));
    }
    Ok(())
}

fn space_view(row: &MemorySpaceRow, memory_count: usize) -> MemorySpaceView {
    MemorySpaceView {
        name: row.name.clone(),
        description: row.description.clone(),
        memory_count: u32::try_from(memory_count).unwrap_or(u32::MAX),
    }
}

fn memory_view(row: &MemoryRow) -> MemoryView {
    MemoryView {
        id: u64::try_from(row.id).unwrap_or(0),
        space: row.space.clone(),
        name: row.name.clone(),
        description: row.description.clone(),
        content: row.content.clone(),
        created_at: row.created_at.clone(),
        updated_at: row.updated_at.clone(),
    }
}

fn now_secs() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        .to_string()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    async fn service() -> (MemoryService, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let pool = crate::db::testing::db().await;
        (
            MemoryService::new(MemoryStore::new(pool, crate::auth::UserId::new("1"))),
            tmp,
        )
    }

    fn create(space: &str, name: &str) -> MemoryCreateInput {
        MemoryCreateInput {
            space: space.into(),
            name: name.into(),
            description: "a fact".into(),
            content: "the body".into(),
        }
    }

    #[tokio::test]
    async fn create_returns_a_view_with_an_id_and_timestamps() {
        let (s, _t) = service().await;
        let v = s.create_memory(create("default", "alpha")).await.unwrap();
        assert!(v.id > 0);
        assert_eq!(v.space, "default");
        assert_eq!(v.content, "the body");
        assert!(!v.created_at.is_empty());
        assert_eq!(v.created_at, v.updated_at);
    }

    #[tokio::test]
    async fn rejects_invalid_slugs_and_overlong_descriptions() {
        let (s, _t) = service().await;
        let mut bad = create("default", "Not A Slug");
        assert!(s.create_memory(bad.clone()).await.is_err());

        bad = create("default", "alpha");
        bad.description = "x".repeat(crate::memory::MAX_DESCRIPTION_CHARS + 1);
        let err = s.create_memory(bad).await.unwrap_err();
        assert!(err.contains("description"), "{err}");
    }

    #[tokio::test]
    async fn update_replaces_only_supplied_fields_and_bumps_updated_at() {
        let (s, _t) = service().await;
        let v = s.create_memory(create("default", "alpha")).await.unwrap();
        let updated = s
            .update_memory(
                i64::try_from(v.id).unwrap(),
                MemoryUpdateInput {
                    description: None,
                    content: Some("new body".into()),
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.description, "a fact");
        assert_eq!(updated.content, "new body");
        assert_eq!(updated.created_at, v.created_at);
    }

    #[tokio::test]
    async fn missing_memory_errors_on_get_update_and_delete() {
        let (s, _t) = service().await;
        assert!(s.get_memory(999).await.is_err());
        assert!(
            s.update_memory(
                999,
                MemoryUpdateInput {
                    description: None,
                    content: Some("x".into())
                }
            )
            .await
            .is_err()
        );
        assert!(s.delete_memory(999).await.is_err());
    }

    #[tokio::test]
    async fn space_views_carry_a_memory_count() {
        let (s, _t) = service().await;
        s.create_memory(create("default", "alpha")).await.unwrap();
        s.create_memory(create("default", "beta")).await.unwrap();
        let spaces = s.list_spaces().await.unwrap();
        let d = spaces.iter().find(|x| x.name == "default").unwrap();
        assert_eq!(d.memory_count, 2);
    }

    #[tokio::test]
    async fn create_space_validates_and_rejects_duplicates() {
        let (s, _t) = service().await;
        assert!(
            s.create_space(MemorySpaceCreateInput {
                name: "Bad Name".into(),
                description: None,
            })
            .await
            .is_err()
        );
        s.create_space(MemorySpaceCreateInput {
            name: "ops".into(),
            description: Some("operational facts".into()),
        })
        .await
        .unwrap();
        assert!(
            s.create_space(MemorySpaceCreateInput {
                name: "ops".into(),
                description: None,
            })
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn update_space_can_rename_and_carries_memories() {
        let (s, _t) = service().await;
        s.create_memory(create("default", "alpha")).await.unwrap();
        let v = s
            .update_space(
                "default",
                MemorySpaceUpdateInput {
                    name: Some("renamed".into()),
                    description: Some("moved".into()),
                },
            )
            .await
            .unwrap();
        assert_eq!(v.name, "renamed");
        assert_eq!(v.description, "moved");
        assert_eq!(v.memory_count, 1);
    }

    #[tokio::test]
    async fn deleting_a_missing_space_errors() {
        let (s, _t) = service().await;
        assert!(s.delete_space("nope").await.is_err());
    }
    // The one validation on that page that leaked its implementation: a
    // duplicate name came back as a raw SQLite constraint error naming the
    // internal table and columns.
    #[tokio::test]
    async fn a_duplicate_space_name_is_a_sentence_not_a_constraint() {
        let (s, _t) = service().await;
        s.create_space(MemorySpaceCreateInput {
            name: "ops".into(),
            description: None,
        })
        .await
        .unwrap();
        let err = s
            .create_space(MemorySpaceCreateInput {
                name: "ops".into(),
                description: None,
            })
            .await
            .unwrap_err();
        assert!(err.contains("already exists"), "{err}");
        assert!(!err.to_lowercase().contains("unique"), "{err}");
        assert!(!err.contains("memory_spaces"), "{err}");
    }

    // The create form refused an empty body and the edit form did not, so a
    // memory could be emptied after the fact. Nothing capped a body either.
    #[tokio::test]
    async fn a_body_must_be_present_and_bounded_on_create_and_update() {
        let (s, _t) = service().await;
        let mut empty = create("default", "alpha");
        empty.content = "  ".into();
        assert!(s.create_memory(empty).await.is_err());

        let mut huge = create("default", "alpha");
        huge.content = "x".repeat(crate::memory::MAX_CONTENT_CHARS + 1);
        let err = s.create_memory(huge).await.unwrap_err();
        assert!(err.contains("content"), "{err}");

        let v = s.create_memory(create("default", "alpha")).await.unwrap();
        let id = i64::try_from(v.id).unwrap();
        assert!(
            s.update_memory(
                id,
                MemoryUpdateInput {
                    description: None,
                    content: Some(String::new()),
                }
            )
            .await
            .is_err()
        );
        assert!(
            s.update_memory(
                id,
                MemoryUpdateInput {
                    description: None,
                    content: Some("x".repeat(crate::memory::MAX_CONTENT_CHARS + 1)),
                }
            )
            .await
            .is_err()
        );
    }
}
