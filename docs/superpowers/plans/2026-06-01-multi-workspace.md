# Multiple Workspaces Per Job — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let one job run against several co-equal named workspace roots; address skills, instruction files, and fs/exec tools by workspace name, with all name→path translation centralized in the runtime.

**Architecture:** A job carries `Vec<Workspace>` (name derived from path, daemon-side). The runtime holds a `WorkspaceRegistry` built from repeatable `--workspace name=path` args and resolves every tool's `workspace` field and the scan filter against it. The per-workspace scan returns `Vec<WorkspaceScan>`; the client composes one prompt block per workspace and the `skill`/`inspect_workspace` tools forward a workspace name to the runtime. The sandbox grants every root via the single `WorkingDir` grant.

**Tech Stack:** Rust workspace; fluorite codegen (`fluorite/*.fl` → `models`); kameo-style actors; nono sandbox; tokio.

Design spec: `docs/superpowers/specs/2026-06-01-multi-workspace-design.md`.

**Build/verify gate (run before any push):**
```bash
cargo build -p models && cargo build --workspace \
  && cargo clippy --all-targets --all-features -- -D warnings \
  && cargo test --workspace \
  && cargo fmt --check \
  && cargo deny check 2>/dev/null; echo "GATE EXIT: $?"
```
Print `GATE_GREEN` only when all pass. CI pins rustc 1.96.0 — fmt/clippy must be clean under it. Never `cargo +nightly fmt`.

---

## Task 1: `models::Workspace` + name derivation

A plain hand-written serde struct (NOT fluorite — it is a storage/in-memory pair) and the pure naming function. Lives in `models` so `supervisor`, `cli`, and `runtime` can all use it.

**Files:**
- Modify: `models/src/lib.rs`

- [ ] **Step 1: Failing test** — add to `models/src/lib.rs` tests:

```rust
#[cfg(test)]
mod workspace_tests {
    use super::{derive_workspaces, Workspace};
    use std::path::PathBuf;

    fn names(ws: &[Workspace]) -> Vec<&str> { ws.iter().map(|w| w.name.as_str()).collect() }

    #[test]
    fn basenames_when_unique() {
        let ws = derive_workspaces(&[PathBuf::from("./api"), PathBuf::from("./web"), PathBuf::from("../shared")]).unwrap();
        assert_eq!(names(&ws), ["api", "web", "shared"]);
    }
    #[test]
    fn lengthens_on_conflict() {
        let ws = derive_workspaces(&[PathBuf::from("./services/api"), PathBuf::from("./tools/api")]).unwrap();
        assert_eq!(names(&ws), ["services/api", "tools/api"]);
    }
    #[test]
    fn lengthens_until_unique() {
        let ws = derive_workspaces(&[PathBuf::from("/a/x/api"), PathBuf::from("/b/x/api")]).unwrap();
        assert_eq!(names(&ws), ["a/x/api", "b/x/api"]);
    }
    #[test]
    fn identical_paths_error() {
        assert!(derive_workspaces(&[PathBuf::from("./api"), PathBuf::from("./api")]).is_err());
    }
}
```

- [ ] **Step 2: Run** `cargo test -p models workspace_tests` → FAIL (unresolved `Workspace`/`derive_workspaces`).

- [ ] **Step 3: Implement** in `models/src/lib.rs`:

```rust
use std::path::PathBuf;

/// A named workspace root. Storage/in-memory pair (hand-written, not fluorite):
/// `JobSpec` persists it and the runtime registry is built from it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Workspace {
    pub name: String,
    pub path: PathBuf,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum WorkspaceError {
    #[error("two workspaces resolve to the same path: {0}")]
    DuplicatePath(String),
    #[error("workspace path has no name component: {0}")]
    Empty(String),
}

/// Derive a unique name per path: start from the basename, and while any two names
/// collide, prepend the next parent segment to each colliding one (joined with `/`),
/// until all are unique. Byte-identical paths are an error.
pub fn derive_workspaces(paths: &[PathBuf]) -> Result<Vec<Workspace>, WorkspaceError> {
    // Detect exact duplicates up front.
    for i in 0..paths.len() {
        for j in (i + 1)..paths.len() {
            if paths[i] == paths[j] {
                return Err(WorkspaceError::DuplicatePath(paths[i].display().to_string()));
            }
        }
    }
    // Per path, the reversed component list (basename first) for progressive lengthening.
    let comps: Vec<Vec<String>> = paths
        .iter()
        .map(|p| {
            p.components()
                .filter_map(|c| match c {
                    std::path::Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
                    _ => None,
                })
                .collect::<Vec<_>>()
        })
        .collect();
    for (p, c) in paths.iter().zip(&comps) {
        if c.is_empty() {
            return Err(WorkspaceError::Empty(p.display().to_string()));
        }
    }
    // depth[i] = how many trailing segments are in name i (>=1).
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
        // Find colliding indices.
        let mut bumped = false;
        for i in 0..names.len() {
            let collides = names.iter().enumerate().any(|(j, n)| j != i && *n == names[i]);
            if collides && depth[i] < comps[i].len() {
                depth[i] += 1;
                bumped = true;
            }
        }
        if !bumped {
            // Either unique, or maxed out (identical-path case already excluded; equal
            // names with maxed depth cannot occur for distinct absolute paths, but if
            // distinct relative paths normalize equal we accept the duplicate-ish name).
            return Ok(paths
                .iter()
                .zip(names)
                .map(|(p, name)| Workspace { name, path: p.clone() })
                .collect());
        }
    }
}
```

- [ ] **Step 4: Run** `cargo test -p models workspace_tests` → PASS.
- [ ] **Step 5: Commit** `git add models/src/lib.rs && git commit -m "feat(models): Workspace + derive_workspaces"`

---

## Task 2: fluorite schema changes

Edit `.fl` files, then regenerate. **No `///` doc comments on union variants** (breaks codegen).

**Files:**
- Modify: `fluorite/runtime.fl`, `fluorite/daemon.fl`, `fluorite/executor.fl`

- [ ] **Step 1: `fluorite/runtime.fl`** — add `workspace: Option<String>` to every tool input struct and rework the scan types:

```
struct BashInput { command: String, workspace: Option<String> }
struct ReadFileInput { path: String, start_line: Option<u64>, end_line: Option<u64>, workspace: Option<String> }
struct WriteFileInput { path: String, content: String, workspace: Option<String> }
struct EditFileInput { path: String, old_text: String, new_text: String, workspace: Option<String> }
struct ReplaceInFileInput { path: String, replacement: String, mode: ReplaceMode, workspace: Option<String> }
struct ListFilesInput { path: String, workspace: Option<String> }
struct GlobInput { pattern: String, path: Option<String>, max_results: Option<u64>, workspace: Option<String> }
struct GrepInput { pattern: String, path: Option<String>, file_pattern: Option<String>, max_results: Option<u64>, workspace: Option<String> }
```
(leave `RegexMode`/`LinesMode`/`ReplaceMode`/`ToolCall` unchanged.)

Scan section — replace `ScanRequest`, `WorkspaceScan`, `ScanResponse`:
```
struct ScanRequest {
    call_id: String,
    workspace: Option<String>,
    instruction_candidates: Vec<String>,
    skills_glob: String,
}

struct ScannedFile { path: String, content: String }
struct WorkspaceScan {
    name: String,
    path: String,
    is_git_repo: bool,
    instructions: Option<ScannedFile>,
    skills: Vec<ScannedFile>,
}
struct ScanResponse { call_id: String, workspaces: Vec<WorkspaceScan> }
```

- [ ] **Step 2: `fluorite/daemon.fl`** — `SubmitRequest.workdir` → `workdirs`, and `JobSummary.workdir` stays (display only, set to first path or joined):
```
struct SubmitRequest {
    workflow: WorkflowDefinition,
    workdirs: Vec<String>,
    input: String,
    capabilities: Option<CapabilitySpec>,
    workflow_name: String,
}
```

- [ ] **Step 3: `fluorite/executor.fl`** — `RuntimeConfig` carries named workspaces:
```
struct WorkspaceConfig { name: String, path: String }
struct RuntimeConfig { workspaces: Vec<WorkspaceConfig> }
```

- [ ] **Step 4: Regenerate** `cargo build -p models` → succeeds and emits the new types. (If `OUT_DIR/.../mod.rs` missing → a union got a doc comment; remove it.)
- [ ] **Step 5: Commit** `git add fluorite && git commit -m "feat(fluorite): multi-workspace scan, tool, config, submit types"` (models regen is build-time, nothing to add).

---

## Task 3: runtime `WorkspaceRegistry`

**Files:**
- Create: `runtime/src/workspace.rs`
- Modify: `runtime/src/lib.rs` (add `pub mod workspace;`)

- [ ] **Step 1: Failing test** in `runtime/src/workspace.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn reg() -> WorkspaceRegistry {
        WorkspaceRegistry::new(vec![
            models::Workspace { name: "api".into(), path: PathBuf::from("/ws/api") },
            models::Workspace { name: "web".into(), path: PathBuf::from("/ws/web") },
        ])
    }

    #[test]
    fn resolves_named() {
        assert_eq!(reg().resolve(&Some("web".into())).unwrap(), PathBuf::from("/ws/web"));
    }
    #[test]
    fn missing_with_multiple_errors() {
        assert!(reg().resolve(&None).is_err());
    }
    #[test]
    fn missing_with_single_defaults() {
        let r = WorkspaceRegistry::new(vec![models::Workspace { name: "only".into(), path: PathBuf::from("/x") }]);
        assert_eq!(r.resolve(&None).unwrap(), PathBuf::from("/x"));
    }
    #[test]
    fn unknown_name_errors() {
        assert!(reg().resolve(&Some("nope".into())).is_err());
    }
    #[test]
    fn parse_arg_splits_name_path() {
        let w = WorkspaceRegistry::parse_arg("api=/ws/api").unwrap();
        assert_eq!(w.name, "api");
        assert_eq!(w.path, PathBuf::from("/ws/api"));
    }
}
```

- [ ] **Step 2: Run** `cargo test -p runtime workspace::` → FAIL.
- [ ] **Step 3: Implement** `runtime/src/workspace.rs`:

```rust
use models::Workspace;
use std::path::{Path, PathBuf};

/// Name → path registry the runtime resolves tool/scan `workspace` fields against.
/// Order-preserving; the single name→path translation site for the runtime.
#[derive(Clone, Debug)]
pub struct WorkspaceRegistry {
    workspaces: Vec<Workspace>,
}

impl WorkspaceRegistry {
    pub fn new(workspaces: Vec<Workspace>) -> Self {
        Self { workspaces }
    }

    /// Parse a `name=path` CLI argument.
    pub fn parse_arg(s: &str) -> Result<Workspace, String> {
        let (name, path) = s
            .split_once('=')
            .ok_or_else(|| format!("expected name=path, got '{s}'"))?;
        if name.is_empty() || path.is_empty() {
            return Err(format!("empty name or path in '{s}'"));
        }
        Ok(Workspace { name: name.to_string(), path: PathBuf::from(path) })
    }

    pub fn workspaces(&self) -> &[Workspace] {
        &self.workspaces
    }

    /// Resolve a `workspace` field to a root path. `None` defaults to the sole
    /// workspace, or errors if there are several. Unknown name errors with the list.
    pub fn resolve(&self, workspace: &Option<String>) -> Result<PathBuf, String> {
        match workspace {
            Some(name) => self
                .workspaces
                .iter()
                .find(|w| &w.name == name)
                .map(|w| w.path.clone())
                .ok_or_else(|| format!("unknown workspace '{name}'; available: {}", self.names_csv())),
            None => match self.workspaces.as_slice() {
                [only] => Ok(only.path.clone()),
                [] => Err("no workspaces configured".to_string()),
                _ => Err(format!("multiple workspaces; specify one of: {}", self.names_csv())),
            },
        }
    }

    /// Resolve for scan: `None` → all roots; `Some(name)` → just that one (empty if unknown).
    pub fn select(&self, workspace: &Option<String>) -> Vec<Workspace> {
        match workspace {
            None => self.workspaces.clone(),
            Some(name) => self.workspaces.iter().filter(|w| &w.name == name).cloned().collect(),
        }
    }

    fn names_csv(&self) -> String {
        self.workspaces.iter().map(|w| w.name.as_str()).collect::<Vec<_>>().join(", ")
    }
}

/// True if `path/.git` exists (file or dir — submodules use a `.git` file).
pub fn is_git_repo(path: &Path) -> bool {
    path.join(".git").exists()
}
```

- [ ] **Step 4: Run** `cargo test -p runtime workspace::` → PASS.
- [ ] **Step 5: Commit** `git add runtime/src/workspace.rs runtime/src/lib.rs && git commit -m "feat(runtime): WorkspaceRegistry"`

---

## Task 4: runtime scan over registry with filter

**Files:**
- Modify: `runtime/src/scan.rs`

- [ ] **Step 1: Rewrite `exec`** to take the registry + filter and return `Vec<WorkspaceScan>`:

```rust
use crate::workspace::{is_git_repo, WorkspaceRegistry};
use models::runtime::{ScanRequest, ScannedFile, WorkspaceScan};

pub fn exec(registry: &WorkspaceRegistry, req: ScanRequest) -> Vec<WorkspaceScan> {
    registry
        .select(&req.workspace)
        .into_iter()
        .map(|ws| {
            let dir = ws.path.as_path();
            let instructions = req.instruction_candidates.iter().find_map(|name| {
                std::fs::read_to_string(dir.join(name)).ok().map(|content| ScannedFile {
                    path: name.clone(),
                    content,
                })
            });
            let pattern = format!("{}/{}", dir.display(), req.skills_glob);
            let mut skills = Vec::new();
            if let Ok(paths) = glob::glob(&pattern) {
                for entry in paths.flatten() {
                    if let Ok(content) = std::fs::read_to_string(&entry) {
                        skills.push(ScannedFile { path: entry.to_string_lossy().into_owned(), content });
                    }
                }
            }
            WorkspaceScan {
                name: ws.name.clone(),
                path: dir.display().to_string(),
                is_git_repo: is_git_repo(dir),
                instructions,
                skills,
            }
        })
        .collect()
}
```

- [ ] **Step 2: Update the existing tests** to build a registry and assert per-entry. Add the filter cases:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use models::Workspace;
    use tempfile::TempDir;

    fn reg(dirs: &[(&str, &std::path::Path)]) -> WorkspaceRegistry {
        WorkspaceRegistry::new(dirs.iter().map(|(n, p)| Workspace { name: (*n).into(), path: p.to_path_buf() }).collect())
    }
    fn req(workspace: Option<String>) -> ScanRequest {
        ScanRequest {
            call_id: "c".into(),
            workspace,
            instruction_candidates: vec!["AGENTS.md".into(), "AGENT.md".into(), "CLAUDE.md".into()],
            skills_glob: ".claude/skills/*/SKILL.md".into(),
        }
    }

    #[test]
    fn scans_all_workspaces() {
        let a = TempDir::new().unwrap();
        let b = TempDir::new().unwrap();
        std::fs::write(a.path().join("AGENTS.md"), "a-rules").unwrap();
        let scans = exec(&reg(&[("a", a.path()), ("b", b.path())]), req(None));
        assert_eq!(scans.len(), 2);
        assert_eq!(scans[0].name, "a");
        assert_eq!(scans[0].instructions.as_ref().unwrap().content, "a-rules");
        assert!(scans[1].instructions.is_none());
    }
    #[test]
    fn filter_selects_one() {
        let a = TempDir::new().unwrap();
        let b = TempDir::new().unwrap();
        let scans = exec(&reg(&[("a", a.path()), ("b", b.path())]), req(Some("b".into())));
        assert_eq!(scans.len(), 1);
        assert_eq!(scans[0].name, "b");
    }
    #[test]
    fn unknown_filter_is_empty() {
        let a = TempDir::new().unwrap();
        assert!(exec(&reg(&[("a", a.path())]), req(Some("zzz".into()))).is_empty());
    }
    #[test]
    fn detects_git_repo() {
        let a = TempDir::new().unwrap();
        std::fs::create_dir(a.path().join(".git")).unwrap();
        let scans = exec(&reg(&[("a", a.path())]), req(None));
        assert!(scans[0].is_git_repo);
    }
    #[test]
    fn instruction_precedence_first_match_wins() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("AGENT.md"), "second").unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "third").unwrap();
        let scans = exec(&reg(&[("w", dir.path())]), req(None));
        assert_eq!(scans[0].instructions.as_ref().unwrap().path, "AGENT.md");
    }
    #[test]
    fn globs_skills_in_hidden_dir() {
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join(".claude/skills/git-bisect");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), "body").unwrap();
        let scans = exec(&reg(&[("w", dir.path())]), req(None));
        assert_eq!(scans[0].skills.len(), 1);
        assert_eq!(scans[0].skills[0].content, "body");
    }
}
```

- [ ] **Step 3: Run** `cargo test -p runtime scan::` (will fail to compile until Task 5 wires `main.rs`; OK to defer to end-of-group build). Mark done when written.
- [ ] **Step 4: Commit** with Task 5.

---

## Task 5: runtime `main.rs` — parse `--workspace`, thread registry, dispatch, sandbox

**Files:**
- Modify: `runtime/src/main.rs`, `runtime/src/tools/mod.rs`, `runtime/src/sandbox.rs`

- [ ] **Step 1: `runtime/src/main.rs` CLI** — replace `working_dir`:

```rust
/// Repeatable `name=path`. At least one required.
#[arg(long = "workspace", required = true, value_parser = parse_workspace_arg)]
workspaces: Vec<models::Workspace>,
```
Add a free function:
```rust
fn parse_workspace_arg(s: &str) -> Result<models::Workspace, String> {
    runtime::workspace::WorkspaceRegistry::parse_arg(s)
}
```

- [ ] **Step 2: build registry + thread it.** In `main`, build `let registry = Arc::new(runtime::workspace::WorkspaceRegistry::new(cli.workspaces.clone()));`. Sandbox apply now takes the paths slice:
```rust
let paths: Vec<std::path::PathBuf> = cli.workspaces.iter().map(|w| w.path.clone()).collect();
if let Err(e) = runtime::sandbox::apply(&paths, socket, caps_file) { ... }
```
Change `run_loop(ws, registry.clone(), runtime_id)` (replace the `working_dir` param with `registry: Arc<WorkspaceRegistry>`).

- [ ] **Step 3: dispatch arms.** In `run_loop`, the `ToolCall` arm clones `registry` instead of `working_dir` and calls `runtime::tools::dispatch(&registry, req.call).await`. The `ScanWorkspace` arm calls `runtime::scan::exec(&registry, req)` and sends `ScanResponse { call_id, workspaces: scan }`.

- [ ] **Step 4: `runtime/src/tools/mod.rs` dispatch** resolves the field once:

```rust
use crate::workspace::WorkspaceRegistry;
use models::runtime::{ToolCall, ToolError, ToolResult};

fn workspace_of(call: &ToolCall) -> &Option<String> {
    match call {
        ToolCall::Bash(i) => &i.workspace,
        ToolCall::ReadFile(i) => &i.workspace,
        ToolCall::WriteFile(i) => &i.workspace,
        ToolCall::EditFile(i) => &i.workspace,
        ToolCall::ReplaceInFile(i) => &i.workspace,
        ToolCall::ListFiles(i) => &i.workspace,
        ToolCall::Glob(i) => &i.workspace,
        ToolCall::Grep(i) => &i.workspace,
    }
}

pub async fn dispatch(registry: &WorkspaceRegistry, call: ToolCall) -> ToolResult {
    let dir = match registry.resolve(workspace_of(&call)) {
        Ok(d) => d,
        Err(reason) => return ToolResult::Err(ToolError { reason }),
    };
    match call {
        ToolCall::Bash(input) => bash::exec(&dir, input).await,
        ToolCall::ReadFile(input) => read_file::exec(&dir, input).await,
        ToolCall::WriteFile(input) => write_file::exec(&dir, input).await,
        ToolCall::EditFile(input) => edit_file::exec(&dir, input).await,
        ToolCall::ReplaceInFile(input) => replace_in_file::exec(&dir, input).await,
        ToolCall::ListFiles(input) => list_files::exec(&dir, input).await,
        ToolCall::Glob(input) => glob::exec(&dir, input).await,
        ToolCall::Grep(input) => grep::exec(&dir, input).await,
    }
}
```
(The individual `tools/*.rs::exec` fns keep `working_dir: &Path` and simply ignore the new `workspace` field on their input struct — no other change. Confirm each builds.)

- [ ] **Step 5: `runtime/src/sandbox.rs`** — `apply(working_dirs: &[PathBuf], socket, caps_file)`. The `Grant::WorkingDir` arm loops:
```rust
Grant::WorkingDir(g) => {
    for dir in working_dirs {
        caps = caps.allow_path(dir, access_mode(&g.access)).map_err(|e| e.to_string())?;
    }
}
```

- [ ] **Step 6: Build** `cargo build -p runtime` → succeeds. Run `cargo test -p runtime` → scan + workspace + tool tests pass.
- [ ] **Step 7: Commit** `git add runtime && git commit -m "feat(runtime): registry-resolved tools + per-workspace scan + multi-root sandbox"`

---

## Task 6: runtime-client transport + client scan signature + tool `workspace` passthrough

**Files:**
- Modify: `runtime-client/src/transport.rs`, `runtime-client/src/client.rs`, `runtime-client/src/tools/mod.rs`, `runtime-client/src/tools/*.rs`

- [ ] **Step 1: transport trait** — `scan_workspace` gains `workspace: Option<String>` and returns `Vec<WorkspaceScan>`:
```rust
async fn scan_workspace(
    &self,
    call_id: &str,
    workspace: Option<String>,
    instruction_candidates: Vec<String>,
    skills_glob: String,
) -> Result<Vec<WorkspaceScan>, TransportError>;
```
Update `MockTransport`: store `scan: Vec<WorkspaceScan>` (default empty vec), `with_scan(Vec<WorkspaceScan>)`, return it. Update its test usages.

- [ ] **Step 2: `client.rs`** — `scan_workspace(workspace, candidates, glob) -> Result<Vec<WorkspaceScan>, RuntimeCallError>`; forward `workspace`. Fix the client unit test (`client_scan_returns_mock_scan`) to wrap in a vec and pass `None`.

- [ ] **Step 3: shared helpers** in `runtime-client/src/tools/mod.rs`:
```rust
use serde_json::{Map, Value};

/// Inject the standard `workspace` property into a tool's input schema.
pub(crate) fn with_workspace(mut schema: Value) -> Value {
    if let Some(props) = schema.get_mut("properties").and_then(Value::as_object_mut) {
        props.insert(
            "workspace".to_string(),
            serde_json::json!({
                "type": "string",
                "description": "Which workspace to act in (see '# Workspaces'). Required when there is more than one workspace."
            }),
        );
    }
    schema
}
pub(crate) fn workspace_arg(input: &Value) -> Option<String> {
    input.get("workspace").and_then(Value::as_str).map(str::to_string)
}
let _ = Map::<String, Value>::new; // (remove; placeholder)
```
(Drop the unused `Map` import; shown only to indicate `serde_json` use.)

- [ ] **Step 4: each `tools/*.rs`** — wrap the schema and pass the field. Example `bash.rs`:
```rust
input_schema: crate::tools::with_workspace(json!({
    "type": "object",
    "properties": { "command": { "type": "string" } },
    "required": ["command"]
})),
```
and in `execute`:
```rust
let workspace = crate::tools::workspace_arg(&input);
self.client.invoke(ToolCall::Bash(BashInput { command, workspace })).await ...
```
Repeat for `read_file`, `write_file`, `edit_file`, `replace_in_file`, `list_files`, `glob`, `grep` — adding `workspace` to each `*Input { .. }` constructor and `with_workspace(...)` around each schema.

- [ ] **Step 5: Build** `cargo build -p runtime-client` → succeeds; `cargo test -p runtime-client` → PASS.
- [ ] **Step 6: Commit** `git add runtime-client && git commit -m "feat(runtime-client): workspace filter on scan + workspace field on tools"`

---

## Task 7: executor socket_transport + ws_transport + process_provider

**Files:**
- Modify: `executor/src/socket_transport.rs`, `executor-client/src/ws_transport.rs`, `executor/src/process_provider.rs`

- [ ] **Step 1: `socket_transport.rs`** — `ScanReply = Result<Vec<WorkspaceScan>, _>`; `scan_workspace` adds the `workspace` arg into the `ScanRequest`, and the reader maps `ScanResult(resp)` → `resp.workspaces`. The in-process test scan builder (~line 218) becomes `ScanResponse { call_id, workspaces: vec![] }` (or a sample entry).

- [ ] **Step 2: `ws_transport.rs`** relay stub — match the new signature, still returns `Err(...)` (relay scan remains unimplemented, per spec out-of-scope):
```rust
async fn scan_workspace(&self, _call_id: &str, _workspace: Option<String>, _instruction_candidates: Vec<String>, _skills_glob: String)
    -> Result<Vec<WorkspaceScan>, TransportError> {
    Err(TransportError::SendFailed("workspace scan unavailable in distributed mode".into()))
}
```

- [ ] **Step 3: `process_provider.rs`** — replace the single `--working-dir` arg with one `--workspace name=path` per workspace:
```rust
for ws in &config.workspaces {
    cmd.arg("--workspace").arg(format!("{}={}", ws.name, ws.path));
}
```

- [ ] **Step 4: Build** `cargo build -p executor -p executor-client` → succeeds.
- [ ] **Step 5: Commit** `git add executor executor-client && git commit -m "feat(executor): thread workspaces to runtime + Vec scan reply"`

---

## Task 8: supervisor JobSpec + job_actor RuntimeConfig

**Files:**
- Modify: `supervisor/src/spec.rs`, `supervisor/src/job_actor.rs`

- [ ] **Step 1: `spec.rs`** — `JobSpec.workdir: PathBuf` → `workspaces: Vec<models::Workspace>`.
- [ ] **Step 2: `job_actor.rs`** — build `RuntimeConfig { workspaces }` mapping each `models::Workspace` → `models::executor::WorkspaceConfig { name, path: path.to_string_lossy().into_owned() }`. Replace any other `spec.workdir` reads (e.g. logging) with `spec.workspaces`.
- [ ] **Step 3: Build** `cargo build -p supervisor` → succeeds.
- [ ] **Step 4: Commit** `git add supervisor && git commit -m "feat(supervisor): JobSpec holds Vec<Workspace>"`

---

## Task 9: CLI + daemon — comma-split `--workdir`, derive names

**Files:**
- Modify: `cli/src/main.rs`, `cli/src/daemon/mod.rs`

- [ ] **Step 1: `cli/src/main.rs`** — `--workdir` value `"a,b,c"` → `Vec<PathBuf>`. Use a `value_delimiter = ','` or split manually. Send `SubmitRequest.workdirs: Vec<String>` (the raw paths). `build_submit` and the `run` handler carry `Vec<PathBuf>`/`Vec<String>` instead of one `PathBuf`. For `job list` display, use the joined paths or first.
- [ ] **Step 2: `cli/src/daemon/mod.rs`** — on submit (~line 169), derive: `let workspaces = models::derive_workspaces(&paths).map_err(|e| /* ErrorResponse */)?;` and set `JobSpec.workspaces = workspaces`. Map a `WorkspaceError` to a user-facing daemon `ErrorResponse`.
- [ ] **Step 3: Build** `cargo build -p cli` → succeeds.
- [ ] **Step 4: Commit** `git add cli && git commit -m "feat(cli): comma-separated --workdir; daemon derives workspace names"`

---

## Task 10: workflow workspace.rs — per-workspace context + prompt

**Files:**
- Modify: `workflow/src/workspace.rs`

- [ ] **Step 1: new types + interpret**:
```rust
#[derive(Clone, Default)]
pub struct WorkspaceContext { pub workspaces: Vec<WorkspaceInfo> }

#[derive(Clone)]
pub struct WorkspaceInfo {
    pub name: String,
    pub path: String,
    pub is_git_repo: bool,
    pub instructions: Option<String>,
    pub skills: Arc<SkillSet>,
}

impl WorkspaceContext {
    pub fn names(&self) -> Vec<String> { self.workspaces.iter().map(|w| w.name.clone()).collect() }
    pub fn find(&self, name: &str) -> Option<&WorkspaceInfo> { self.workspaces.iter().find(|w| w.name == name) }
}
```
`scan(client, workspace: Option<String>)` calls `client.scan_workspace(workspace, candidates, glob)`, maps each `WorkspaceScan` → `WorkspaceInfo` (reuse `parse_skill`/dedup per workspace via a small `interpret_one`).

- [ ] **Step 2: `compose_system_prompt`** — emit a `# Workspaces` section, one block per workspace (path, git flag, instructions, skills listing). Keep agent role first. Single-workspace still renders one block.

```rust
pub fn compose_system_prompt(agent_prompt: Option<&str>, ws: &WorkspaceContext) -> Option<String> {
    let mut sections: Vec<String> = Vec::new();
    if let Some(p) = agent_prompt && !p.trim().is_empty() { sections.push(p.trim().to_string()); }
    if !ws.workspaces.is_empty() {
        let mut block = String::from("# Workspaces\nFilesystem, bash, and skill tools take a `workspace` argument naming one of these.");
        for w in &ws.workspaces {
            block.push_str(&format!("\n\n## {} — {}{}", w.name, w.path, if w.is_git_repo { " (git)" } else { "" }));
            if let Some(instr) = &w.instructions { if !instr.trim().is_empty() { block.push_str(&format!("\n{}", instr.trim())); } }
            if !w.skills.is_empty() {
                block.push_str(&format!("\n### Skills (load with the skill tool: workspace=\"{}\")\n{}", w.name, skills_listing(&w.skills)));
            }
        }
        sections.push(block);
    }
    if sections.is_empty() { None } else { Some(sections.join("\n\n")) }
}
```

- [ ] **Step 3:** Update `list_skills_result` → a catalog renderer keyed by workspace (used by `inspect_workspace`); see Task 11. Update existing tests in this file (the old `WorkspaceContext { instructions, skills }` shape is gone) to the new shape and assertions (per-workspace blocks; no cross-workspace dedup; intra-workspace kept-first).
- [ ] **Step 4: Build/test** `cargo test -p workflow workspace::` (compiles after Task 11 too). 
- [ ] **Step 5: Commit** with Task 11.

---

## Task 11: workflow context.rs — `for_agent` names, workspace-aware `skill`/`inspect_workspace`

**Files:**
- Modify: `workflow/src/context.rs`, `workflow/src/workflow_actor.rs`

- [ ] **Step 1:** rename const `LIST_SKILLS_TOOL` → `INSPECT_WORKSPACE_TOOL = "inspect_workspace"`.
- [ ] **Step 2:** `ToolboxFactory::for_agent` gains `workspace_names: Vec<String>`; `AgentToolbox` stores it.
- [ ] **Step 3:** `skill` tool: schema `{ workspace?: string, name: string }`. In `execute`:
  - read `workspace` arg; if `None` and `workspace_names.len() != 1` → `InvalidInput("specify a workspace: <names>")`; if `None` and exactly one → use it.
  - `let ws = scan(&client, Some(name)).await;` then `ws.find(&resolved).and_then(|w| w.skills.get(skill_name))` → body; misses → `InvalidInput`.
- [ ] **Step 4:** `inspect_workspace` tool: schema `{ workspace?: string }`. `execute`:
  - `let ws = scan(&client, workspace_arg).await;` render a catalog: per returned `WorkspaceInfo`, a line with name, path, git flag, instruction-presence, then its skills `- name: desc`. Never bodies.
- [ ] **Step 5:** `workflow_actor.rs::spawn_agent` — `let ws = workspace::scan(&self.rt.runtime_client, None).await;` compose prompt; `let names = ws.names();` pass into `for_agent(agent_def, runtime_client, names)`.
- [ ] **Step 6:** Update context.rs tests: `for_agent` calls gain a names arg; `skill`/`inspect_workspace` tests use `with_scan(vec![WorkspaceScan{ name, path, is_git_repo, instructions, skills }])`; assert workspace-scoped lookups and the missing/ambiguous errors.
- [ ] **Step 7: Build/test** `cargo test -p workflow` → PASS.
- [ ] **Step 8: Commit** `git add workflow && git commit -m "feat(workflow): per-workspace prompt + workspace-aware skill/inspect tools"`

---

## Task 12: e2e test — two workspaces end to end

**Files:**
- Create/Modify: an e2e test under the crate that already hosts full-stack tests (find via `rg -l "spawn|daemon|JobSpec" tests/` and mirror the closest existing e2e harness).

- [ ] **Step 1:** Find the existing e2e pattern: `rg --files tests/ ; rg -n "JobSpec|submit|run_workflow|spawn_runtime" tests/`. Reuse its harness (real runtime binary + real journal — no mocks, per the integration-test philosophy).
- [ ] **Step 2: Write the test** — create two temp workspace dirs `alpha` and `beta`, each with `AGENTS.md` and a `.claude/skills/<s>/SKILL.md`; run a minimal workflow whose agent is driven to:
  1. call `inspect_workspace` (assert both `alpha` and `beta` appear with their skills),
  2. call `skill { workspace: "beta", name: <beta-skill> }` (assert it returns beta's body, not alpha's),
  3. `write_file { workspace: "alpha", path: "out.txt", content: "x" }` then assert the file lands in `alpha/out.txt` and NOT in `beta`.
  Assert the composed agent system prompt contains both `## alpha` and `## beta` blocks. Use the real provider mock that scripts these tool calls if the suite already has one; otherwise assert via the runtime scan + dispatch directly through the real `RuntimeClient`/runtime binary (still real components, not fabricated returns).
- [ ] **Step 3: Run** the e2e: `cargo test -p <crate> --test <name> -- --nocapture` → PASS.
- [ ] **Step 4: Commit** `git add tests && git commit -m "test(e2e): multi-workspace scan, scoped skill, scoped write"`

---

## Task 13: full gate + push

- [ ] **Step 1:** Run the full gate (top of this doc). Fix any clippy (`unwrap_used`/`expect_used`/`panic`/`wildcard_enum_match_arm` are denied in prod code — handle `Result`/`Option` explicitly; tests already opt out). Fix fmt with `cargo fmt` (stable, NOT nightly).
- [ ] **Step 2:** When the gate prints `GATE_GREEN`, optionally squash the WIP commits into one (`git rebase -i main` is unavailable; use `git reset --soft main && git commit`), keeping the design+plan+impl. Then `git push`.
- [ ] **Step 3:** Confirm CI on PR #47 goes green (`gh pr checks 47 --watch`). Address any 1.96.0-specific fmt/clippy deltas or cargo-deny license issues on new crates (none added here — all changes are in existing crates).

---

## Self-review

**Spec coverage:** §1 Workspace/derive → T1; §2 CLI/daemon wire → T9; §3 runtime args+sandbox → T5; §4 ScanRequest filter + per-ws scan types → T2; §5 scan::exec → T4; §6 client interpret → T10; §7 prompt blocks → T10; §8 skill/inspect tools + for_agent names → T11; §9 tool `workspace` resolution (runtime, no jailing) → T2/T5/T6; sandbox one-rule-all-roots → T5; missing-workspace runtime error → T3/T5; e2e → T12. All covered.

**Placeholders:** the `let _ = Map::...` line in T6 Step 3 is explicitly flagged as illustrative — drop it; use the two helper fns as written.

**Type consistency:** `models::Workspace { name, path }` used in T1/T3/T5/T8/T10; fluorite `WorkspaceConfig { name, path: String }` only in RuntimeConfig (T2/T7/T8); `WorkspaceScan { name, path, is_git_repo, instructions, skills }` consistent T2/T4/T6/T10/T11; `scan_workspace(workspace, candidates, glob) -> Vec<WorkspaceScan>` consistent across transport/client/workspace.rs (T6/T10); `resolve` vs `select` on the registry distinct and used correctly (resolve→tools, select→scan).
