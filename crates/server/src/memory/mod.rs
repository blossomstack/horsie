//! Agent-managed long-term memories, grouped into named spaces. A session
//! selects spaces at creation; the agent sees a one-line-per-memory index in
//! its system prompt and loads full bodies on demand. Everything here executes
//! in the server process -- the sandboxed runtime is never involved.
//!
//! Mirrors the `plugins` module's store/service split and shares the config
//! store's SqlitePool.

mod prompt;
mod service;
mod store;
mod toolbox;

pub use prompt::render_index;
pub use service::MemoryService;
pub use store::{MemoryRow, MemorySpaceRow, MemoryStore};
pub use toolbox::MemoryToolbox;

/// Cap on a memory's one-line description. The index ships every description
/// in the system prompt on every turn, so this bounds the fixed per-turn cost.
pub const MAX_DESCRIPTION_CHARS: usize = 200;

/// Cap on a memory's body. The index only ships descriptions, but a body is
/// loaded verbatim into a turn on request, and nothing bounded it at all — a
/// 100 KB memory was accepted, then read whole into the prompt.
pub const MAX_CONTENT_CHARS: usize = 32_000;

/// Cap on how many memories the rendered index lists before truncating.
pub const MAX_INDEX_ENTRIES: usize = 200;

/// Space and memory names are slugs. Rejecting `/` is what keeps the
/// `space/name` address the agent uses unambiguous.
pub fn validate_slug(s: &str) -> Result<(), String> {
    if s.is_empty() {
        return Err("name must not be empty".to_string());
    }
    if s.chars().count() > 64 {
        return Err("name must be at most 64 characters".to_string());
    }
    let first = s.chars().next().unwrap_or('-');
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return Err(format!(
            "name '{s}' must start with a lowercase letter or digit"
        ));
    }
    if !s
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
    {
        return Err(format!(
            "name '{s}' may only contain lowercase letters, digits, '.', '_' and '-'"
        ));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::validate_slug;

    #[test]
    fn accepts_lowercase_slugs() {
        for s in ["a", "default", "my-space", "repo.name_2", "9lives"] {
            assert!(validate_slug(s).is_ok(), "{s} should be valid");
        }
    }

    #[test]
    fn rejects_slashes_uppercase_spaces_and_empty() {
        for s in ["", "Has-Upper", "has space", "a/b", "-leading", ".dot"] {
            assert!(validate_slug(s).is_err(), "{s} should be invalid");
        }
    }

    #[test]
    fn rejects_overlong_names() {
        assert!(validate_slug(&"a".repeat(65)).is_err());
        assert!(validate_slug(&"a".repeat(64)).is_ok());
    }
}
