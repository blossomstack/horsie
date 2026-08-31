//! Reading a stored MCP selection, including the ones written before tools
//! could be selected at all.
//!
//! A selection used to be a bare server name, and it is stored as JSON in two
//! places that cannot be migrated with a `SELECT`: the `agents.mcp_servers`
//! column, and `AgentSettings` inside every journalled session spec. Both are
//! full of `["linear"]`. A plain type change makes each of those rows fail to
//! deserialize, and a session whose spec will not load is a live session that
//! cannot recover — the exact failure `sessions::spec` already guards against
//! for `WorkspaceDef` and `SessionOrigin`.
//!
//! So the *storage* read tolerates both spellings and nothing else does: the
//! wire type is one shape, the picker sends one shape, and a name read this way
//! becomes `{ name, tools: None }` — every tool that server offers, which is
//! exactly what selecting it used to mean.

use horsie_models::mcp::McpServerSelection;
use serde::{Deserialize, Deserializer};

/// One stored selection: an object, or a bare name from before tools could be
/// chosen.
#[derive(Deserialize)]
#[serde(untagged)]
enum StoredSelection {
    Name(String),
    Full(McpServerSelection),
}

impl From<StoredSelection> for McpServerSelection {
    fn from(s: StoredSelection) -> Self {
        match s {
            StoredSelection::Name(name) => McpServerSelection { name, tools: None },
            StoredSelection::Full(sel) => sel,
        }
    }
}

/// `#[serde(deserialize_with)]` for a `Vec<McpServerSelection>` field that may
/// have been written as a list of names.
pub fn de_selections<'de, D>(d: D) -> Result<Vec<McpServerSelection>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Vec::<StoredSelection>::deserialize(d)?
        .into_iter()
        .map(Into::into)
        .collect())
}

/// A whole server: every tool it offers, now and later. The shape a test or a
/// caller with nothing to narrow wants.
#[must_use]
pub fn whole(name: &str) -> McpServerSelection {
    McpServerSelection {
        name: name.to_string(),
        tools: None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use serde::Serialize;

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Holder {
        #[serde(deserialize_with = "de_selections")]
        mcp_servers: Vec<McpServerSelection>,
    }

    /// The shape every agent row and every journalled session spec written
    /// before this was a thing.
    #[test]
    fn a_list_of_names_reads_as_every_tool_of_each() {
        let h: Holder = serde_json::from_str(r#"{"mcp_servers":["linear","github"]}"#).unwrap();
        assert_eq!(h.mcp_servers.len(), 2);
        assert_eq!(h.mcp_servers[0].name, "linear");
        // `None`, not `Some(vec![])`: selecting a server used to mean all of
        // it, and it still does.
        assert_eq!(h.mcp_servers[0].tools, None);
        assert_eq!(h.mcp_servers[1].name, "github");
    }

    #[test]
    fn the_new_shape_reads_as_itself() {
        let h: Holder = serde_json::from_str(
            r#"{"mcp_servers":[{"name":"linear","tools":["search_issues"]},{"name":"github","tools":null}]}"#,
        )
        .unwrap();
        assert_eq!(
            h.mcp_servers[0].tools.as_deref(),
            Some(["search_issues".to_string()].as_slice())
        );
        assert_eq!(h.mcp_servers[1].tools, None);
    }

    /// A row half-migrated by hand, or one written while a rollout was in
    /// flight, is not a reason to lose the session.
    #[test]
    fn the_two_shapes_mix_in_one_list() {
        let h: Holder =
            serde_json::from_str(r#"{"mcp_servers":["linear",{"name":"github","tools":[]}]}"#)
                .unwrap();
        assert_eq!(h.mcp_servers[0].tools, None);
        assert_eq!(h.mcp_servers[1].tools.as_deref(), Some([].as_slice()));
    }
}
