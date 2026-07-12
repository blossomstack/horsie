#[allow(clippy::doc_markdown, clippy::too_many_arguments)]
pub mod agent {
    include!(concat!(env!("OUT_DIR"), "/agent/mod.rs"));
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
}

#[allow(clippy::doc_markdown, clippy::too_many_arguments)]
pub mod workflow {
    include!(concat!(env!("OUT_DIR"), "/workflow/mod.rs"));
}

// `large_enum_variant`: `DaemonRequest::Submit` carries the full `SubmitRequest`
// (workflow + caps + hackamore policy) and is intrinsically larger than the other
// control variants. The enum is fluorite-generated, so boxing the variant isn't
// available here; the size is acceptable for a one-shot control message.
#[allow(
    clippy::doc_markdown,
    clippy::too_many_arguments,
    clippy::large_enum_variant
)]
pub mod daemon {
    include!(concat!(env!("OUT_DIR"), "/daemon/mod.rs"));
}

#[allow(clippy::doc_markdown, clippy::too_many_arguments)]
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

/// Env var carrying the provision-steps JSON a vendor injects into a runtime
/// child. Read by `horsie-runtime` at startup; written by the executor
/// providers from `RuntimeConfig.provision`.
pub const ENV_PROVISION: &str = "HORSIE_PROVISION";

/// Env var carrying a GitHub token used by `git_checkout` provision steps for
/// github.com URLs.
pub const ENV_GITHUB_TOKEN: &str = "GITHUB_TOKEN";

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

impl agent::Message {
    pub fn user(id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            role: agent::Role::User,
            parts: vec![agent::ContentPart::Text(agent::TextPart {
                text: text.into(),
            })],
        }
    }

    pub fn tool_result(
        tool_call_id: impl Into<String>,
        output: impl Into<String>,
        is_error: bool,
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
        }
    }
}

impl agent::AgentInput {
    pub fn user_message(id: impl Into<String>, text: impl Into<String>) -> Self {
        Self::UserMessage(agent::UserMessageInput {
            id: id.into(),
            text: text.into(),
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

    pub fn message_id(&self) -> String {
        match self {
            Self::UserMessage(u) => u.id.clone(),
            Self::ToolResult(t) => format!("result:{}", t.tool_call_id),
        }
    }

    pub fn to_message(&self) -> agent::Message {
        match self {
            Self::UserMessage(u) => agent::Message {
                id: u.id.clone(),
                role: agent::Role::User,
                parts: vec![agent::ContentPart::Text(agent::TextPart {
                    text: u.text.clone(),
                })],
            },
            Self::ToolResult(t) => agent::Message {
                id: format!("result:{}", t.tool_call_id),
                role: agent::Role::Tool,
                parts: vec![agent::ContentPart::ToolResult(agent::ToolResultPart {
                    tool_call_id: t.tool_call_id.clone(),
                    output: t.output.clone(),
                    is_error: t.is_error,
                })],
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
    use super::capabilities::{
        Access, AllowNetwork, BlockNetwork, CapabilitySpec, Grant, NetworkPolicy, ProxyOnlyNetwork,
    };
    use super::session;

    #[test]
    fn session_event_round_trips_with_type_tag() {
        let ev = session::SessionEvent::Delta(session::DeltaEvent { text: "hi".into() });
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains("\"type\""));
        let back: session::SessionEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(ev, back);
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
            include_shared: false,
            call_id: "c1".into(),
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
