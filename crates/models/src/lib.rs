// `large_enum_variant`: `HistoryEntry::Hook` carries a whole `HookRecord` (a
// dozen optional strings) and dwarfs `Llm(Message)`. Both types are
// fluorite-generated, so boxing the variant isn't available here, and a
// transcript entry is moved once per append — not on a hot path.
#[allow(
    clippy::doc_markdown,
    clippy::too_many_arguments,
    clippy::large_enum_variant
)]
pub mod agent {
    include!(concat!(env!("OUT_DIR"), "/agent/mod.rs"));

    impl Usage {
        /// A `Usage` with no cache data reported — the common case for test
        /// fixtures and any call site that doesn't yet know about caching.
        ///
        /// Named `without_cache` rather than `new`: fluorite's codegen already
        /// derives an all-fields `Usage::new(input_tokens, output_tokens,
        /// cache_creation_tokens, cache_read_tokens)` via `derive_new::new` on
        /// every generated struct, so a hand-written 2-arg `new` here would be
        /// a duplicate inherent-method definition (E0592).
        pub fn without_cache(input_tokens: u32, output_tokens: u32) -> Self {
            Self {
                input_tokens,
                output_tokens,
                cache_creation_tokens: None,
                cache_read_tokens: None,
            }
        }
    }

    impl HistoryEntry {
        /// This entry's cursor id — the same space `/history` pages with and the
        /// SSE stream uses as its event id, whichever kind of entry it is.
        ///
        /// The two id spaces are disjoint by construction: a `Message` id is a
        /// provider id or `result:{tool_call_id}`, a hook id is
        /// `hook:{tool_call_id}:{n}`. That is what lets one lookup over the
        /// transcript stay unambiguous.
        #[must_use]
        pub fn id(&self) -> &str {
            match self {
                Self::Llm(m) => &m.id,
                Self::Hook(h) => &h.id,
            }
        }

        /// When this entry joined the transcript.
        #[must_use]
        pub fn created_at_ms(&self) -> u64 {
            match self {
                Self::Llm(m) => m.created_at_ms,
                Self::Hook(h) => h.created_at_ms,
            }
        }
    }

    impl AgentLogBody {
        /// This entry's own identity, where it has one.
        ///
        /// Not a cursor — `AgentLogEntry::seq` is the cursor, and splitting the
        /// two is what let ordering stop depending on a scan. This is the id a
        /// tool result joins its call on, and the id a client dedupes an
        /// optimistic echo against. A lifecycle entry has neither need and so
        /// has no id, and neither does a compaction boundary: nothing joins to
        /// one, and a client that wants to name it uses the entry's `seq`,
        /// which is also what makes it a conversation's identity.
        #[must_use]
        pub fn id(&self) -> Option<&str> {
            match self {
                Self::Llm(m) => Some(&m.id),
                Self::Hook(h) => Some(&h.id),
                Self::Lifecycle(_) | Self::Compaction(_) => None,
            }
        }
    }
}

#[allow(clippy::doc_markdown, clippy::too_many_arguments)]
pub mod auth {
    include!(concat!(env!("OUT_DIR"), "/auth/mod.rs"));
}

#[allow(clippy::doc_markdown, clippy::too_many_arguments)]
pub mod capabilities {
    include!(concat!(env!("OUT_DIR"), "/capabilities/mod.rs"));
}

#[allow(clippy::doc_markdown, clippy::too_many_arguments)]
pub mod events {
    include!(concat!(env!("OUT_DIR"), "/events/mod.rs"));
}

#[allow(clippy::doc_markdown, clippy::too_many_arguments)]
pub mod executor {
    include!(concat!(env!("OUT_DIR"), "/executor/mod.rs"));
}

#[allow(clippy::doc_markdown, clippy::too_many_arguments)]
pub mod runtime {
    include!(concat!(env!("OUT_DIR"), "/runtime/mod.rs"));

    impl SessionStartSource {
        /// The spelling a hook is given, which is the spec's and not Rust's.
        ///
        /// The enum is horsie's own vocabulary; `"startup"` is the foreign one a
        /// plugin's `matcher` is written against and its script compares to. One
        /// mapping, here, rather than a `String` threaded through every seam on
        /// the chance that someone spells it right.
        #[must_use]
        pub fn as_wire(&self) -> &'static str {
            match self {
                Self::Startup => "startup",
                Self::Resume => "resume",
                Self::Clear => "clear",
                Self::Compact => "compact",
                Self::Fork => "fork",
            }
        }
    }
}

#[allow(clippy::doc_markdown, clippy::too_many_arguments)]
pub mod runtime_vendor {
    include!(concat!(env!("OUT_DIR"), "/runtime_vendor/mod.rs"));
}

#[allow(clippy::doc_markdown, clippy::too_many_arguments)]
pub mod workflow {
    include!(concat!(env!("OUT_DIR"), "/workflow/mod.rs"));

    impl OutcomeFilter {
        /// Whether this filter admits `outcome`.
        #[must_use]
        pub fn matches(&self, outcome: &str) -> bool {
            match self {
                Self::In(f) => f.values.iter().any(|v| v == outcome),
                Self::NotIn(f) => !f.values.iter().any(|v| v == outcome),
            }
        }

        /// The label a reader sees on the edge — `outcome in [p0, p1]`.
        ///
        /// Here rather than in the server because the run log stores it, the
        /// CLI prints it and the browser draws it: three renderings of one
        /// filter would drift, and an edge labelled two different ways in two
        /// places is a bug nobody can see.
        #[must_use]
        pub fn render(&self) -> String {
            let (op, values) = match self {
                Self::In(f) => ("in", &f.values),
                Self::NotIn(f) => ("not in", &f.values),
            };
            format!("outcome {op} [{}]", values.join(", "))
        }

        /// The values this filter names, whichever way round it is.
        #[must_use]
        pub fn values(&self) -> &[String] {
            match self {
                Self::In(f) => &f.values,
                Self::NotIn(f) => &f.values,
            }
        }
    }
}

// `large_enum_variant`: `DaemonRequest::Submit` carries the full `SubmitRequest`
// (workflow + caps) and is intrinsically larger than the other
// control variants. The enum is fluorite-generated, so boxing the variant isn't
// available here; the size is acceptable for a one-shot control message.
// `large_enum_variant` here too: `AgentStreamEvent::Appended` carries a
// `HistoryEntry`, so it inherits the imbalance described above.
#[allow(
    clippy::doc_markdown,
    clippy::too_many_arguments,
    clippy::large_enum_variant
)]
pub mod session {
    include!(concat!(env!("OUT_DIR"), "/session/mod.rs"));
}

#[allow(clippy::doc_markdown, clippy::too_many_arguments)]
pub mod session_api {
    include!(concat!(env!("OUT_DIR"), "/session_api/mod.rs"));
}

#[allow(clippy::doc_markdown, clippy::too_many_arguments)]
pub mod settings {
    include!(concat!(env!("OUT_DIR"), "/settings/mod.rs"));
}

#[allow(clippy::doc_markdown, clippy::too_many_arguments)]
pub mod model_cards {
    include!(concat!(env!("OUT_DIR"), "/model_cards/mod.rs"));
}

#[allow(clippy::doc_markdown, clippy::too_many_arguments)]
pub mod github {
    include!(concat!(env!("OUT_DIR"), "/github/mod.rs"));
}

// `large_enum_variant`: `HookAction`'s arms differ by a few optional strings
// each. The types are fluorite-generated, so boxing a variant isn't available
// here, and a record is moved once per hook run — not on a hot path.
#[allow(
    clippy::doc_markdown,
    clippy::too_many_arguments,
    clippy::large_enum_variant
)]
pub mod hooks {
    include!(concat!(env!("OUT_DIR"), "/hooks/mod.rs"));
}

#[allow(clippy::doc_markdown, clippy::too_many_arguments)]
pub mod mcp {
    include!(concat!(env!("OUT_DIR"), "/mcp/mod.rs"));
}

#[allow(clippy::doc_markdown, clippy::too_many_arguments)]
pub mod memory {
    include!(concat!(env!("OUT_DIR"), "/memory/mod.rs"));
}

#[allow(clippy::doc_markdown, clippy::too_many_arguments)]
pub mod plugins {
    include!(concat!(env!("OUT_DIR"), "/plugins/mod.rs"));
}

#[allow(clippy::doc_markdown, clippy::too_many_arguments)]
pub mod agents {
    include!(concat!(env!("OUT_DIR"), "/agents/mod.rs"));
}

#[allow(clippy::doc_markdown, clippy::too_many_arguments)]
pub mod environments {
    include!(concat!(env!("OUT_DIR"), "/environments/mod.rs"));
}

#[allow(clippy::doc_markdown, clippy::too_many_arguments)]
pub mod projects {
    include!(concat!(env!("OUT_DIR"), "/projects/mod.rs"));
}

#[allow(clippy::doc_markdown, clippy::too_many_arguments)]
pub mod routines {
    include!(concat!(env!("OUT_DIR"), "/routines/mod.rs"));
}

#[allow(clippy::doc_markdown, clippy::too_many_arguments)]
pub mod tools {
    include!(concat!(env!("OUT_DIR"), "/tools/mod.rs"));
}

#[allow(clippy::doc_markdown, clippy::too_many_arguments)]
pub mod inbox {
    include!(concat!(env!("OUT_DIR"), "/inbox/mod.rs"));
}

/// The bearer a runtime presents on its dial-back, minted by whoever spawned it
/// for that runtime's id alone.
///
/// Carried in the environment rather than argv deliberately: argv is readable
/// by any process on the host through `ps`, while a process environment is
/// owner-only. It is a credential, so it travels the private channel.
pub const ENV_CONNECT_TOKEN: &str = "HORSIE_CONNECT_TOKEN";

/// JSON array of [`runtime::BundleRef`]s the runtime fetches at startup
/// (written by the server's plugin provisioner into the runtime env).
///
/// No URLs: a bundle is named by its version, and the address it is fetched
/// from arrives separately as [`ENV_SERVER_URL`].
pub const ENV_PLUGIN_MANIFEST: &str = "HORSIE_PLUGIN_MANIFEST";

/// How a bundle version is spelled in a URL path and as a directory name.
///
/// One function rather than a format string on each side: the server builds
/// the route from it and the runtime builds the request and its store key from
/// it, and a mismatch between those two is a 404 nobody can read.
///
/// Prefixed by kind, so a generation can never be mistaken for a hash — and so
/// a store entry keeps saying which sort of thing it is.
#[must_use]
pub fn bundle_version_slug(version: &runtime::BundleVersion) -> String {
    match version {
        runtime::BundleVersion::Hash(h) => format!("sha256-{}", h.hash),
        runtime::BundleVersion::Generation(g) => format!("gen-{}", g.generation),
    }
}

/// [`bundle_version_slug`] read backwards. `None` for anything else, which the
/// route answers as "no such bundle" rather than guessing.
#[must_use]
pub fn parse_bundle_version_slug(slug: &str) -> Option<runtime::BundleVersion> {
    if let Some(hash) = slug.strip_prefix("sha256-") {
        return (!hash.is_empty() && hash.chars().all(|c| c.is_ascii_hexdigit())).then(|| {
            runtime::BundleVersion::Hash(runtime::BundleHash {
                hash: hash.to_string(),
            })
        });
    }
    let generation = slug.strip_prefix("gen-")?.parse().ok()?;
    Some(runtime::BundleVersion::Generation(
        runtime::BundleGeneration { generation },
    ))
}

/// The base URL this runtime reaches its server at.
///
/// Supplied by whoever spawned the runtime, because only that party knows the
/// address: loopback for a local vendor process, an advertise address for a
/// remote one, and for a cloud vendor the HTTP form of its configured callback
/// URL. The server itself cannot know it.
///
/// One variable for every server call a runtime makes — fetching its bundles
/// and minting a git credential both build on it. It was `HORSIE_PLUGINS_BASE`
/// while bundles were the only caller.
pub const ENV_SERVER_URL: &str = "HORSIE_SERVER_URL";

/// Directory the runtime unpacks fetched bundles into and scans as its
/// plugins_dir. One per runtime: the runtime scans the whole directory, so a
/// shared one would show a session another session's skills.
pub const ENV_PLUGINS_DIR: &str = "HORSIE_PLUGINS_DIR";

impl capabilities::CapabilitySpec {
    /// Load and parse a capability file (the runtime's `--sandbox-caps` path, or a
    /// user-authored file the CLI resolves). Shared by the runtime and the CLI; the
    /// built-in *default* spec is owned by the CLI, not here.
    pub fn load(path: &std::path::Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("read capability file {}: {e}", path.display()))?;
        serde_json::from_str(&text)
            .map_err(|e| format!("parse capability file {}: {e}", path.display()))
    }
}

/// Wall-clock milliseconds since the Unix epoch — the one stamp behind every
/// `at_ms` / `created_at_ms` on the wire. Lives here because every crate that
/// stamps one already depends on `horsie-models`, and a single reading of the
/// clock keeps journal, history and SSE talking about the same instant.
///
/// A clock before the epoch reads as 0 rather than panicking; the alternative
/// is killing a turn over a misconfigured host clock.
#[must_use]
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

impl agent::Message {
    pub fn user(id: impl Into<String>, text: impl Into<String>, created_at_ms: u64) -> Self {
        Self {
            id: id.into(),
            role: agent::Role::User,
            parts: vec![agent::ContentPart::Text(agent::TextPart {
                text: text.into(),
            })],
            created_at_ms,
            started_at_ms: None,
        }
    }

    pub fn assistant_text(
        id: impl Into<String>,
        text: impl Into<String>,
        created_at_ms: u64,
    ) -> Self {
        Self {
            id: id.into(),
            role: agent::Role::Assistant,
            parts: vec![agent::ContentPart::Text(agent::TextPart {
                text: text.into(),
            })],
            created_at_ms,
            started_at_ms: None,
        }
    }

    pub fn tool_result(
        tool_call_id: impl Into<String>,
        output: impl Into<String>,
        is_error: bool,
        created_at_ms: u64,
    ) -> Self {
        let tool_call_id = tool_call_id.into();
        Self {
            id: format!("result:{tool_call_id}"),
            role: agent::Role::Tool,
            parts: vec![agent::ContentPart::ToolResult(agent::ToolResultPart {
                tool_call_id,
                output: output.into(),
                is_error,
            })],
            created_at_ms,
            started_at_ms: None,
        }
    }
}

impl agent::SubAgentResultPart {
    /// The text a provider sees. Before results became their own part this was
    /// the exact string merged into the parent's user message, and it must stay
    /// that string: the wire is not supposed to notice this change. Both
    /// provider serializers render the part through here, so there is one
    /// definition of the format rather than two that can drift.
    #[must_use]
    pub fn to_wire_text(&self) -> String {
        let header = format!("[subagent \"{}\" {}]", self.title, self.status);
        if self.text.is_empty() {
            header
        } else {
            format!("{header}\n\n{}", self.text)
        }
    }
}

impl agent::AgentInput {
    pub fn user_message(id: impl Into<String>, text: impl Into<String>) -> Self {
        Self::UserMessage(agent::UserMessageInput {
            id: id.into(),
            text: text.into(),
            subagent_results: Vec::new(),
        })
    }

    /// A user turn that also delivers finished subagents' results. `text` may
    /// be empty — a turn started purely by owed results has nothing typed.
    pub fn user_message_with_results(
        id: impl Into<String>,
        text: impl Into<String>,
        subagent_results: Vec<agent::SubAgentResultPart>,
    ) -> Self {
        Self::UserMessage(agent::UserMessageInput {
            id: id.into(),
            text: text.into(),
            subagent_results,
        })
    }

    pub fn tool_result(
        tool_call_id: impl Into<String>,
        output: impl Into<String>,
        is_error: bool,
    ) -> Self {
        Self::ToolResult(agent::ToolResultInput {
            tool_call_id: tool_call_id.into(),
            output: output.into(),
            is_error,
        })
    }

    /// Several results delivered as one input. Empty is meaningless — there is
    /// nothing to resume with — so callers pass at least one.
    pub fn tool_results(results: Vec<agent::ToolResultInput>) -> Self {
        Self::ToolResults(agent::ToolResultsInput { results })
    }

    /// The id of the message this input becomes. For a batch of results it is
    /// the first one's, which keeps it stable and unique per turn boundary.
    pub fn message_id(&self) -> String {
        match self {
            Self::UserMessage(u) => u.id.clone(),
            Self::ToolResult(t) => format!("result:{}", t.tool_call_id),
            Self::ToolResults(t) => match t.results.first() {
                Some(first) => format!("result:{}", first.tool_call_id),
                None => "result:none".to_string(),
            },
        }
    }

    /// `created_at_ms` is the moment this input became a journaled message —
    /// passed in rather than read from the clock here so a caller that already
    /// stamped a matching event uses the same instant for both.
    pub fn to_message(&self, created_at_ms: u64) -> agent::Message {
        match self {
            Self::UserMessage(u) => {
                // An owed-only turn carries results and nothing typed. The text
                // part is omitted rather than left blank: Anthropic rejects an
                // empty text block outright.
                let mut parts = Vec::with_capacity(1 + u.subagent_results.len());
                if !u.text.is_empty() {
                    parts.push(agent::ContentPart::Text(agent::TextPart {
                        text: u.text.clone(),
                    }));
                }
                parts.extend(
                    u.subagent_results
                        .iter()
                        .cloned()
                        .map(agent::ContentPart::SubAgentResult),
                );
                agent::Message {
                    id: u.id.clone(),
                    role: agent::Role::User,
                    parts,
                    created_at_ms,
                    started_at_ms: None,
                }
            }
            Self::ToolResult(t) => agent::Message {
                id: format!("result:{}", t.tool_call_id),
                role: agent::Role::Tool,
                parts: vec![agent::ContentPart::ToolResult(agent::ToolResultPart {
                    tool_call_id: t.tool_call_id.clone(),
                    output: t.output.clone(),
                    is_error: t.is_error,
                })],
                created_at_ms,
                started_at_ms: None,
            },
            // One message carrying every result: Anthropic takes it as a user
            // message with N `tool_result` blocks, and the OpenAI wire splits it
            // back into one `role: "tool"` message per result.
            Self::ToolResults(t) => agent::Message {
                id: self.message_id(),
                role: agent::Role::Tool,
                parts: t
                    .results
                    .iter()
                    .map(|r| {
                        agent::ContentPart::ToolResult(agent::ToolResultPart {
                            tool_call_id: r.tool_call_id.clone(),
                            output: r.output.clone(),
                            is_error: r.is_error,
                        })
                    })
                    .collect(),
                created_at_ms,
                started_at_ms: None,
            },
        }
    }
}

/// A named workspace root. Storage/in-memory pair (hand-written, deliberately NOT a
/// fluorite type): `JobSpec` persists it and the runtime registry is built from it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Workspace {
    pub name: String,
    pub path: std::path::PathBuf,
}

/// Error from [`derive_workspaces`].
#[derive(Debug, PartialEq, Eq)]
pub enum WorkspaceError {
    /// Two inputs are the same path — a real mistake, not a naming problem.
    DuplicatePath(String),
    /// A path has no usable name component (e.g. `/` or empty).
    Empty(String),
}

impl std::fmt::Display for WorkspaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DuplicatePath(p) => write!(f, "two workspaces resolve to the same path: {p}"),
            Self::Empty(p) => write!(f, "workspace path has no name component: {p}"),
        }
    }
}

impl std::error::Error for WorkspaceError {}

/// Derive a unique name per path: start from the basename, and while any two names
/// collide, prepend the next parent segment to each colliding one (joined with `/`)
/// until all are unique. Byte-identical paths are an error.
pub fn derive_workspaces(paths: &[std::path::PathBuf]) -> Result<Vec<Workspace>, WorkspaceError> {
    for i in 0..paths.len() {
        for j in (i + 1)..paths.len() {
            if paths[i] == paths[j] {
                return Err(WorkspaceError::DuplicatePath(
                    paths[i].display().to_string(),
                ));
            }
        }
    }
    // Per path, its normal components (basename last) for progressive lengthening.
    let comps: Vec<Vec<String>> = paths
        .iter()
        .map(|p| {
            p.components()
                .filter_map(|c| match c {
                    std::path::Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
                    std::path::Component::Prefix(_)
                    | std::path::Component::RootDir
                    | std::path::Component::CurDir
                    | std::path::Component::ParentDir => None,
                })
                .collect::<Vec<_>>()
        })
        .collect();
    for (p, c) in paths.iter().zip(&comps) {
        if c.is_empty() {
            return Err(WorkspaceError::Empty(p.display().to_string()));
        }
    }
    // depth[i] = number of trailing segments included in name i (>= 1).
    let mut depth = vec![1usize; paths.len()];
    loop {
        let names: Vec<String> = comps
            .iter()
            .zip(&depth)
            .map(|(c, &d)| {
                let start = c.len().saturating_sub(d);
                c[start..].join("/")
            })
            .collect();
        let mut bumped = false;
        for i in 0..names.len() {
            let collides = names
                .iter()
                .enumerate()
                .any(|(j, n)| j != i && *n == names[i]);
            if collides && depth[i] < comps[i].len() {
                depth[i] += 1;
                bumped = true;
            }
        }
        if !bumped {
            return Ok(paths
                .iter()
                .zip(names)
                .map(|(p, name)| Workspace {
                    name,
                    path: p.clone(),
                })
                .collect());
        }
    }
}

/// Convert selected repos into `git_checkout` provision steps: default the
/// checkout dir from the URL basename, de-duplicate collisions (`api`,
/// `api-2`, …), and validate that dirs stay inside the workspace.
pub fn provision_from_repos(
    repos: &[session_api::RepoConfig],
) -> Result<Vec<executor::ProvisionStep>, String> {
    let mut taken: Vec<String> = Vec::new();
    let mut steps = Vec::with_capacity(repos.len());
    for r in repos {
        let url = r.url.trim();
        if url.is_empty() {
            return Err("repo url cannot be empty".to_string());
        }
        let base = match r.dir.as_deref().map(str::trim) {
            Some(d) if !d.is_empty() => d.to_string(),
            _ => {
                // Strip the scheme before taking the last path segment, so a
                // scheme-only URL (e.g. "https:///") has no path segment and
                // errors instead of yielding "https:". Same fix as
                // `runtime::steps::dir_from_url`.
                let without_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
                let b = without_scheme
                    .trim_end_matches('/')
                    .rsplit('/')
                    .next()
                    .unwrap_or("")
                    .trim_end_matches(".git");
                if b.is_empty() {
                    return Err(format!("cannot derive a directory name from '{url}'"));
                }
                b.to_string()
            }
        };
        let p = std::path::Path::new(&base);
        if p.is_absolute()
            || p.components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(format!(
                "repo dir '{base}' must be a relative path without '..'"
            ));
        }
        let mut dir = base.clone();
        let mut n = 2;
        while taken.contains(&dir) {
            dir = format!("{base}-{n}");
            n += 1;
        }
        taken.push(dir.clone());
        let mut with = vec![
            executor::StepParam {
                key: "url".into(),
                value: url.to_string(),
            },
            executor::StepParam {
                key: "dir".into(),
                value: dir.clone(),
            },
        ];
        if let Some(git_ref) = r
            .git_ref
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            with.push(executor::StepParam {
                key: "ref".into(),
                value: git_ref.to_string(),
            });
        }
        steps.push(executor::ProvisionStep {
            name: format!("checkout {dir}"),
            uses: "git_checkout".into(),
            with,
        });
    }
    Ok(steps)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod workspace_tests {
    use super::{Workspace, derive_workspaces};
    use std::path::PathBuf;

    fn names(ws: &[Workspace]) -> Vec<&str> {
        ws.iter().map(|w| w.name.as_str()).collect()
    }

    #[test]
    fn basenames_when_unique() {
        let ws = derive_workspaces(&[
            PathBuf::from("./api"),
            PathBuf::from("./web"),
            PathBuf::from("../shared"),
        ])
        .unwrap();
        assert_eq!(names(&ws), ["api", "web", "shared"]);
    }

    #[test]
    fn lengthens_on_conflict() {
        let ws = derive_workspaces(&[
            PathBuf::from("./services/api"),
            PathBuf::from("./tools/api"),
        ])
        .unwrap();
        assert_eq!(names(&ws), ["services/api", "tools/api"]);
    }

    #[test]
    fn lengthens_until_unique() {
        let ws =
            derive_workspaces(&[PathBuf::from("/a/x/api"), PathBuf::from("/b/x/api")]).unwrap();
        assert_eq!(names(&ws), ["a/x/api", "b/x/api"]);
    }

    #[test]
    fn identical_paths_error() {
        assert!(derive_workspaces(&[PathBuf::from("./api"), PathBuf::from("./api")]).is_err());
    }

    #[test]
    fn single_workspace_basename() {
        let ws = derive_workspaces(&[PathBuf::from("/home/me/october")]).unwrap();
        assert_eq!(names(&ws), ["october"]);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::agent;
    use super::capabilities::{
        Access, AllowNetwork, BlockNetwork, CapabilitySpec, Grant, NetworkPolicy, ProxyOnlyNetwork,
    };
    use super::session;

    /// Pins the three-level nesting the log depends on: the body union tags the
    /// arm, the lifecycle union tags the kind, and the outcome union tags again
    /// inside that. Nothing else asserts the wire shape, and a codegen change
    /// that flattened any level would be silently accepted by every other test.
    #[test]
    fn a_lifecycle_entry_round_trips_with_its_tag() {
        let entry = agent::AgentLogEntry {
            seq: 7,
            at_ms: 1_700_000_000_000,
            body: agent::AgentLogBody::Lifecycle(agent::LifecycleEvent::TurnEnded(
                agent::TurnEndedLifecycle {
                    outcome: agent::TurnOutcome::Failed(agent::FailedOutcome {
                        error: "boom".into(),
                    }),
                },
            )),
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["body"]["type"], "Lifecycle", "{json}");
        assert_eq!(json["body"]["value"]["kind"], "TurnEnded", "{json}");
        assert_eq!(json["body"]["value"]["value"]["outcome"]["kind"], "Failed");
        assert_eq!(json["atMs"], 1_700_000_000_000u64, "camelCase on the wire");
        let back: agent::AgentLogEntry = serde_json::from_value(json).unwrap();
        assert_eq!(back, entry);
    }

    /// A task list rides the log as a whole snapshot, so a client that folds it
    /// needs no separate read to know the current plan.
    #[test]
    fn a_task_list_entry_carries_the_whole_list() {
        let body = agent::AgentLogBody::Lifecycle(agent::LifecycleEvent::TaskList(
            agent::TaskListLifecycle {
                tasks: vec![agent::TaskItem {
                    id: 1,
                    content: "do the thing".into(),
                    status: agent::TaskStatus::InProgress,
                }],
            },
        ));
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["value"]["value"]["tasks"][0]["status"], "InProgress");
        let back: agent::AgentLogBody = serde_json::from_value(json).unwrap();
        assert_eq!(back, body);
    }

    fn result_part(title: &str) -> agent::SubAgentResultPart {
        agent::SubAgentResultPart {
            subagent_id: "11111111-1111-1111-1111-111111111111".into(),
            title: title.into(),
            status: "completed".into(),
            text: "did the thing".into(),
            spawned_at_ms: 10,
            ended_at_ms: 50,
        }
    }

    #[test]
    fn a_user_message_appends_subagent_results_after_its_text() {
        let input = agent::AgentInput::user_message_with_results(
            "m1",
            "keep going",
            vec![result_part("audit")],
        );
        let msg = input.to_message(0);
        assert_eq!(msg.parts.len(), 2);
        assert!(matches!(&msg.parts[0], agent::ContentPart::Text(t) if t.text == "keep going"));
        assert!(
            matches!(&msg.parts[1], agent::ContentPart::SubAgentResult(r) if r.title == "audit")
        );
    }

    /// An owed-only turn has no typed text. An empty text block is not merely
    /// noise — Anthropic rejects it — so the part is omitted, not blanked.
    #[test]
    fn an_empty_user_text_produces_no_text_part() {
        let input =
            agent::AgentInput::user_message_with_results("m1", "", vec![result_part("audit")]);
        let msg = input.to_message(0);
        assert_eq!(msg.parts.len(), 1);
        assert!(matches!(
            &msg.parts[0],
            agent::ContentPart::SubAgentResult(_)
        ));
    }

    #[test]
    fn a_plain_user_message_is_unchanged() {
        let msg = agent::AgentInput::user_message("m1", "hello").to_message(0);
        assert_eq!(msg.parts.len(), 1);
        assert!(matches!(&msg.parts[0], agent::ContentPart::Text(t) if t.text == "hello"));
    }

    /// The one string the providers send. Pinned literally: this is the wire
    /// contract that the whole change is built to leave alone.
    #[test]
    fn a_result_renders_the_notification_text_it_always_did() {
        assert_eq!(
            result_part("audit").to_wire_text(),
            "[subagent \"audit\" completed]\n\ndid the thing"
        );
    }

    #[test]
    fn a_result_with_no_body_renders_the_header_alone() {
        let mut part = result_part("audit");
        part.text = String::new();
        assert_eq!(part.to_wire_text(), "[subagent \"audit\" completed]");
    }

    /// One tagged union replaces the two the stream used to need. The tag is
    /// what lets a client match exhaustively instead of inferring the variant
    /// from which field happens to be present.
    #[test]
    fn a_message_frame_round_trips_with_its_type_tag() {
        let delta = session::MessageFrame::Delta(session::MessageDelta {
            entry_seq: 99,
            delta_seq: 3,
            text: "hi".into(),
            reset: false,
        });
        let json = serde_json::to_string(&delta).unwrap();
        assert!(json.contains("\"type\":\"Delta\""), "{json}");
        assert!(
            json.contains("\"entrySeq\":99"),
            "camelCase on the wire: {json}"
        );
        assert_eq!(
            serde_json::from_str::<session::MessageFrame>(&json).unwrap(),
            delta
        );

        let entry = session::MessageFrame::Entry(agent::AgentLogEntry {
            seq: 99,
            at_ms: 1,
            body: agent::AgentLogBody::Lifecycle(agent::LifecycleEvent::TurnBegan(
                agent::TurnBeganLifecycle {
                    consumed: vec!["m1".into()],
                    answered: vec![],
                },
            )),
        });
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"type\":\"Entry\""), "{json}");
        assert_eq!(
            serde_json::from_str::<session::MessageFrame>(&json).unwrap(),
            entry
        );
    }

    #[test]
    fn capability_spec_load_parses_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("caps.json");
        std::fs::write(
            &path,
            r#"{
                "network": { "type": "Allow", "value": {} },
                "grants": [
                    { "type": "Dir", "value": { "path": "/usr", "access": "Read" } },
                    { "type": "WorkingDir", "value": { "access": "ReadWrite" } }
                ]
            }"#,
        )
        .unwrap();
        let spec = CapabilitySpec::load(&path).expect("valid file parses");
        assert_eq!(spec.network, NetworkPolicy::Allow(AllowNetwork {}));
        assert!(matches!(
            spec.grants.first(),
            Some(Grant::Dir(d)) if d.path == "/usr" && d.access == Access::Read
        ));
    }

    #[test]
    fn network_policy_json_round_trips_all_variants() {
        // Pins the wire format of every variant: adjacently tagged with
        // `type`/`value`, the unit-payload variants carrying an empty object.
        let cases = [
            (
                NetworkPolicy::Block(BlockNetwork {}),
                r#"{"type":"Block","value":{}}"#,
            ),
            (
                NetworkPolicy::Allow(AllowNetwork {}),
                r#"{"type":"Allow","value":{}}"#,
            ),
            (
                NetworkPolicy::ProxyOnly(ProxyOnlyNetwork { port: 18080 }),
                r#"{"type":"ProxyOnly","value":{"port":18080}}"#,
            ),
        ];
        for (policy, expected_json) in cases {
            let json = serde_json::to_string(&policy).unwrap();
            assert_eq!(json, expected_json);
            let back: NetworkPolicy = serde_json::from_str(&json).unwrap();
            assert_eq!(back, policy);
        }
    }

    #[test]
    fn capability_spec_load_rejects_missing_file() {
        let err = CapabilitySpec::load(std::path::Path::new("/nonexistent/horsie-caps.json"))
            .expect_err("missing file must error");
        assert!(err.contains("read capability file"));
    }

    #[test]
    fn scan_workspace_inbound_round_trips() {
        use crate::runtime::{RuntimeInboundMessage, ScanRequest};
        let msg = RuntimeInboundMessage::ScanWorkspace(ScanRequest {
            call_id: "c1".into(),
            agent_id: "a1".into(),
            workspace: None,
            instruction_candidates: vec!["AGENTS.md".into()],
            skills_glob: ".claude/skills/*/SKILL.md".into(),
        });
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"ScanWorkspace\""));
        let back: RuntimeInboundMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, RuntimeInboundMessage::ScanWorkspace(r) if r.call_id == "c1"));
    }

    #[test]
    fn provision_from_repos_defaults_and_dedupes_dirs() {
        use crate::session_api::RepoConfig;
        let steps = crate::provision_from_repos(&[
            RepoConfig {
                url: "https://github.com/o/api.git".into(),
                git_ref: None,
                dir: None,
            },
            RepoConfig {
                url: "https://github.com/other/api".into(),
                git_ref: Some("dev".into()),
                dir: None,
            },
            RepoConfig {
                url: "https://github.com/o/web".into(),
                git_ref: None,
                dir: Some("frontend".into()),
            },
        ])
        .unwrap();
        let dirs: Vec<&str> = steps
            .iter()
            .map(|s| {
                s.with
                    .iter()
                    .find(|p| p.key == "dir")
                    .unwrap()
                    .value
                    .as_str()
            })
            .collect();
        assert_eq!(dirs, vec!["api", "api-2", "frontend"]);
        assert_eq!(steps[0].uses, "git_checkout");
        assert!(
            steps[1]
                .with
                .iter()
                .any(|p| p.key == "ref" && p.value == "dev")
        );
        assert_eq!(steps[1].name, "checkout api-2");
    }

    #[test]
    fn provision_from_repos_rejects_bad_input() {
        use crate::session_api::RepoConfig;
        let empty = RepoConfig {
            url: "  ".into(),
            git_ref: None,
            dir: None,
        };
        assert!(crate::provision_from_repos(&[empty]).is_err());
        let escape = RepoConfig {
            url: "https://github.com/o/x".into(),
            git_ref: None,
            dir: Some("../out".into()),
        };
        assert!(crate::provision_from_repos(&[escape]).is_err());
        // Scheme-only URL: no path segment to derive a dir from. Same bug class
        // as runtime::steps::dir_from_url — strip the scheme before taking the
        // last path segment, or "https:///" wrongly yields "https:".
        let scheme_only = RepoConfig {
            url: "https:///".into(),
            git_ref: None,
            dir: None,
        };
        assert!(crate::provision_from_repos(&[scheme_only]).is_err());
    }

    #[test]
    fn scan_result_outbound_round_trips() {
        use crate::runtime::{RuntimeOutboundMessage, ScanResponse, ScannedFile, WorkspaceScan};
        let msg = RuntimeOutboundMessage::ScanResult(ScanResponse {
            shared_skills: vec![],
            shared_agents: None,
            shared_root: None,
            call_id: "c1".into(),
            workspaces: vec![WorkspaceScan {
                name: "october".into(),
                path: "/ws/october".into(),
                is_git_repo: true,
                instructions: Some(ScannedFile {
                    path: "AGENTS.md".into(),
                    content: "hi".into(),
                }),
                skills: vec![ScannedFile {
                    path: ".claude/skills/x/SKILL.md".into(),
                    content: "b".into(),
                }],
                platform: None,
            }],
        });
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"ScanResult\""));
        let back: RuntimeOutboundMessage = serde_json::from_str(&json).unwrap();
        assert!(
            matches!(back, RuntimeOutboundMessage::ScanResult(r) if r.workspaces.len() == 1 && r.workspaces[0].skills.len() == 1)
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod vendor_tests {
    #[test]
    fn vendor_command_round_trips_with_a_type_tag() {
        use crate::runtime_vendor::{
            HibernateRuntimeRequest, RuntimeVendorCommand, RuntimeVendorInboundMessage,
        };
        let msg = RuntimeVendorInboundMessage {
            request_id: "req-1".to_string(),
            command: RuntimeVendorCommand::HibernateRuntime(HibernateRuntimeRequest {
                runtime_id: "rt-1".to_string(),
            }),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"HibernateRuntime\""), "{json}");
        let back: RuntimeVendorInboundMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn vendor_event_round_trips_and_carries_capabilities() {
        use crate::runtime_vendor::{
            RuntimeVendorCapabilities, RuntimeVendorEvent, RuntimeVendorOutboundMessage,
            RuntimeVendorReady,
        };
        let msg = RuntimeVendorOutboundMessage {
            request_id: "req-2".to_string(),
            event: RuntimeVendorEvent::Ready(RuntimeVendorReady {
                vendor_name: "my-laptop".to_string(),
                instance_id: "inst-1".to_string(),
                capabilities: RuntimeVendorCapabilities {
                    supports_provisioning: false,
                },
            }),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let back: RuntimeVendorOutboundMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(back, msg);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod auth_wire_tests {
    use crate::auth::{AuthStatus, LoginRequest, PasswordChangeRequest};

    #[test]
    fn auth_status_is_camel_case_on_the_wire() {
        let json = serde_json::to_string(&AuthStatus {
            enabled: true,
            authenticated: false,
            must_change_password: false,
            external: true,
            login_url: Some("https://id.example/authorize".into()),
            logout_url: Some("https://id.example/logout".into()),
        })
        .unwrap();
        assert!(json.contains("\"mustChangePassword\""), "{json}");
        assert!(!json.contains("must_change_password"), "{json}");
        // A snake_case key here is not an error, it is silently ignored by
        // every client — so the camelCase spelling is worth pinning for the
        // fields the UI branches on.
        assert!(json.contains("\"loginUrl\""), "{json}");
        assert!(json.contains("\"logoutUrl\""), "{json}");
        assert!(!json.contains("login_url"), "{json}");
    }

    #[test]
    fn login_and_password_change_deserialize_from_camel_case() {
        let req: LoginRequest = serde_json::from_str(r#"{"password":"p"}"#).unwrap();
        assert_eq!(req.password, "p");
        let req: PasswordChangeRequest =
            serde_json::from_str(r#"{"currentPassword":"a","newPassword":"b"}"#).unwrap();
        assert_eq!(req.current_password, "a");
        assert_eq!(req.new_password, "b");
    }

    #[test]
    fn device_flow_types_are_camel_case_on_the_wire() {
        use crate::auth::{DeviceCodeResponse, DeviceTokenRequest, TokenPair};
        let json = serde_json::to_string(&DeviceCodeResponse {
            device_code: "d".into(),
            user_code: "BCDF-GHJK".into(),
            verification_uri: "http://x/auth/device".into(),
            verification_uri_complete: "http://x/auth/device?code=BCDF-GHJK".into(),
            expires_in: 600,
            interval: 5,
        })
        .unwrap();
        assert!(json.contains("\"deviceCode\""), "{json}");
        assert!(json.contains("\"verificationUriComplete\""), "{json}");

        let req: DeviceTokenRequest = serde_json::from_str(r#"{"deviceCode":"d"}"#).unwrap();
        assert_eq!(req.device_code, "d");

        let pair = serde_json::to_string(&TokenPair {
            access_token: "a".into(),
            refresh_token: "r".into(),
            expires_in: 3600,
        })
        .unwrap();
        assert!(pair.contains("\"accessToken\""), "{pair}");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod agents_tests {
    #[test]
    fn agent_view_round_trips_with_camel_case_keys() {
        use crate::agents::AgentView;
        let view = AgentView {
            name: "reviewer".into(),
            description: "reviews PRs".into(),
            instructions: Some("Always cite file:line.".into()),
            model: "sonnet".into(),
            plugins: vec!["superpowers".into()],
            mcp_servers: vec![],
            memory_spaces: vec!["default".into()],
            thinking_effort: Some("high".into()),
            created_at: "1".into(),
            updated_at: "2".into(),
            auto_compact: None,
            allowed_tools: None,
            tunable: Some(true),
            revision: Some(3),
        };
        let json = serde_json::to_string(&view).unwrap();
        assert!(json.contains("\"mcpServers\""), "{json}");
        assert!(json.contains("\"tunable\":true"), "{json}");
        assert!(json.contains("\"thinkingEffort\":\"high\""), "{json}");
        let back: AgentView = serde_json::from_str(&json).unwrap();
        assert_eq!(back, view);
    }
}
