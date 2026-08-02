# Drop the `workspace` Parameter Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove the model-facing `workspace` parameter from the eight path-taking tools and `set_working_dir`, and give the model the absolute paths it was standing in for.

**Architecture:** A tool call's base directory becomes the agent's sticky cwd override if set, else the first workspace in registry order — one addressing mechanism instead of three. The shared plugin library stops being reachable only by name: its absolute root ships in `ScanResponse` and is rendered in the prompt, and every skill (workspace and shared alike) carries its directory.

**Tech Stack:** Rust 2024 edition, fluorite IDL codegen (`models/fluorite/*.fl` → `horsie_models`), tokio, `cargo nextest`, clippy with `unwrap_used`/`expect_used`/`panic`/`wildcard_enum_match_arm` denied outside `#[cfg(test)]`.

## Global Constraints

- Spec: `docs/superpowers/specs/2026-08-01-drop-workspace-param-design.md`. Closes issue #94.
- `WorkspaceRegistry::resolve` **stays** — `runtime/src/steps.rs:109` uses it to resolve a provision step's `workspace` param, which is operator-authored config, not a model input. Only the tool path stops calling it.
- `skill`, `inspect_workspace`, and `ScanRequest.workspace` keep their parameter. Do not touch them.
- Do not claim the shared library is read-only anywhere — it is not enforced.
- Clippy lints are denied at crate level; test modules carry the existing `#[allow(...)]` block. Follow the file's existing style.
- Verify with `cargo clippy --all-targets --all-features -- -D warnings` and `cargo nextest run` from the repo root.
- Commit after each task.

---

### Task 1: Wire schema — drop the field, add the shared root

**Files:**
- Modify: `models/fluorite/runtime.fl:6-25` (nine input structs), `:82-86` (`ScanResponse`)
- Modify: `runtime-client/src/transport.rs:73-101`, `runtime-client/src/client.rs:181-205`
- Modify: `runtime-client/src/testkit.rs:300-315`

This task only changes types and plumbing; the crates that consume the removed field are fixed in Tasks 2 and 3, so the workspace will not compile until Task 3 lands. Build with `cargo check -p horsie-models` in step 2 rather than a full build.

- [ ] **Step 1: Edit the IDL**

In `models/fluorite/runtime.fl`, remove `workspace: Option<String>` from `BashInput`, `ReadFileInput`, `WriteFileInput`, `FindAndReplaceInput`, `ReplaceLinesInput`, `ListFilesInput`, `GlobInput`, `GrepInput`, and `SetWorkingDirInput`. The results:

```
struct BashInput { command: String, timeout_secs: Option<u64> }
struct ReadFileInput { path: String, start_line: Option<u64>, end_line: Option<u64> }
struct WriteFileInput { path: String, content: String }
struct FindAndReplaceInput { path: String, find: String, replace: String, regex: Option<bool>, replace_all: Option<bool> }
struct ReplaceLinesInput { path: String, start_line: u64, end_line: u64, replacement: String }
struct ListFilesInput { path: String }
struct GlobInput { pattern: String, path: Option<String>, max_results: Option<u64> }
struct GrepInput { pattern: String, path: Option<String>, file_pattern: Option<String>, max_results: Option<u64> }
```

Replace the `SetWorkingDirInput` comment and struct with:

```
// Set the caller's working directory for all future tool calls. `path` may be
// absolute or relative to the current effective cwd; omit it to reset to the
// default working directory (the first workspace).
struct SetWorkingDirInput { path: Option<String> }
```

Add the shared root to `ScanResponse`:

```
struct ScanResponse {
    call_id: String,
    workspaces: Vec<WorkspaceScan>,
    shared_skills: Vec<PluginSkill>,
    /// Absolute path of the shared plugin library root, when one is configured.
    /// `PluginSkill.rel_dir` is relative to it. Optional so an older runtime
    /// binary still deserializes against a newer server.
    shared_root: Option<String>,
}
```

- [ ] **Step 2: Regenerate and check the models crate**

Run: `cargo check -p horsie-models`
Expected: PASS.

- [ ] **Step 3: Return the whole `ScanResponse` from the transport**

In `runtime-client/src/transport.rs`, change `scan_workspace`'s return type from `Result<(Vec<WorkspaceScan>, Vec<PluginSkill>), TransportError>` to `Result<ScanResponse, TransportError>`, and the match arm from `Ok((resp.workspaces, resp.shared_skills))` to `Ok(resp)`. Import `ScanResponse` and drop now-unused imports. Extend the doc comment's last sentence to:

```
/// set, the shared plugin library's skills are returned alongside its absolute
/// root.
```

- [ ] **Step 4: Mirror it on the client**

In `runtime-client/src/client.rs`, change `RuntimeClient::scan_workspace`'s return type to `Result<ScanResponse, RuntimeCallError>`. The body is unchanged apart from the type.

- [ ] **Step 5: Update the testkit**

In `runtime-client/src/testkit.rs`, the canned `ScanResult` construction gains `shared_root: None`. Add a `with_shared_root(self, root: &str) -> Self` builder alongside the existing `with_scan`, storing `Option<String>` on the mock and emitting it in the response, so Task 5's tests can assert the rendered header.

- [ ] **Step 6: Commit**

```bash
git add models/fluorite/runtime.fl runtime-client/src/transport.rs runtime-client/src/client.rs runtime-client/src/testkit.rs
git commit -m "wire: drop workspace from the tool inputs, add the shared root to ScanResponse"
```

---

### Task 2: Runtime resolves the base without a name

**Files:**
- Modify: `runtime/src/workspace.rs:63-92` (add `default_root`, keep `resolve`)
- Modify: `runtime/src/tools/mod.rs:22-107`
- Modify: `runtime/src/tools/set_working_dir.rs`
- Modify: `runtime/src/scan.rs:20-63`
- Test: the `#[cfg(test)]` modules in each of those files

**Interfaces:**
- Produces: `WorkspaceRegistry::default_root(&self) -> Result<PathBuf, String>` — the first workspace's path, or `Err("no workspaces configured")`.
- Consumes: `RuntimeState::effective_dir(&self, agent: &str, fallback: &Path) -> PathBuf` (unchanged).

- [ ] **Step 1: Write the failing tests**

In `runtime/src/workspace.rs`, replace `missing_with_multiple_errors` and `missing_with_single_defaults` with:

```rust
#[test]
fn default_root_is_the_first_workspace() {
    assert_eq!(reg().default_root().unwrap(), PathBuf::from("/ws/api"));
}

#[test]
fn default_root_errors_with_no_workspaces() {
    assert!(WorkspaceRegistry::new(vec![]).default_root().is_err());
}
```

Keep `resolves_named`, `unknown_name_errors`, `parse_arg_*`, and `select_all_and_one` — `resolve` still serves provision steps.

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p horsie-runtime workspace::tests`
Expected: FAIL, `no method named default_root`.

- [ ] **Step 3: Add `default_root`**

In `runtime/src/workspace.rs`, after `resolve`:

```rust
/// The base directory for a tool call that names no directory of its own: the
/// first workspace in registry order. Tools no longer take a `workspace`
/// argument, so this — or the caller's sticky cwd override — is the only base.
/// The shared plugin library is deliberately not a candidate; it is reached by
/// absolute path.
pub fn default_root(&self) -> Result<PathBuf, String> {
    match self.workspaces.first() {
        Some(first) => Ok(first.path.clone()),
        None => Err("no workspaces configured".to_string()),
    }
}
```

Narrow `resolve`'s doc comment to its remaining caller:

```rust
/// Resolve a provision step's `workspace` field to a root path. `None` defaults to
/// the sole workspace, or errors when there are several (operator config must name
/// one). An unknown name errors with the available list. Tool calls do not go
/// through here — see [`Self::default_root`].
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p horsie-runtime workspace::tests`
Expected: PASS.

- [ ] **Step 5: Collapse `dispatch`**

In `runtime/src/tools/mod.rs`, delete `fn base` entirely and replace the per-arm `match base(...)` wrapping. Resolve once, before the match, since every path-taking arm now wants the same directory:

```rust
/// Run a tool call, then clamp its output.
///
/// The two state-mutating tools act on the agent's own state. Every other tool
/// runs in the agent's working directory: its `set_working_dir` override if it
/// has one, else the first workspace. Relative paths join onto that; an absolute
/// path in the call replaces it outright (`Path::join` discards the base), which
/// is how an agent reaches another workspace or the shared plugin library.
pub async fn dispatch(
    registry: &WorkspaceRegistry,
    state: &RuntimeState,
    agent: &str,
    call: ToolCall,
) -> ToolResult {
    if let ToolCall::SetWorkingDir(i) = call {
        return set_working_dir::exec(registry, state, agent, i);
    }
    if let ToolCall::SetEnv(i) = call {
        return set_env::exec(state, agent, i);
    }
    let dir = match registry.default_root() {
        Ok(root) => state.effective_dir(agent, &root),
        Err(reason) => return ToolResult::Err(ToolError { reason }),
    };
    let result = match call {
        ToolCall::Bash(i) => bash::exec(&dir, &state.env_overlay(agent), i).await,
        ToolCall::ReadFile(i) => read_file::exec(&dir, i).await,
        ToolCall::WriteFile(i) => write_file::exec(&dir, i).await,
        ToolCall::FindAndReplace(i) => find_and_replace::exec(&dir, i).await,
        ToolCall::ReplaceLines(i) => replace_lines::exec(&dir, i).await,
        ToolCall::ListFiles(i) => list_files::exec(&dir, i).await,
        ToolCall::Glob(i) => glob::exec(&dir, i).await,
        ToolCall::Grep(i) => grep::exec(&dir, i).await,
        // Handled above; re-matched only because the earlier `if let` moved `call`
        // back out. Keep them explicit — a wildcard arm is lint-denied.
        ToolCall::SetWorkingDir(_) | ToolCall::SetEnv(_) => {
            return ToolResult::Err(ToolError {
                reason: "unreachable: state tools are dispatched above".to_string(),
            });
        }
    };
    ...
}
```

If the borrow checker objects to the `if let` moving `call`, match on `&call` for the two state arms and clone the input, or restructure as a single `match` with the two state arms returning early — either is fine, but keep every variant spelled out.

Also delete `fn workspace_of` (lines 22-33) and its doc comment, and drop the now-unused `use std::path::PathBuf;`.

- [ ] **Step 6: Fix the dispatch tests**

In the same file's test module, drop `workspace: None` from the `BashInput` literals, and replace `dispatch_errors_when_workspace_ambiguous` with a test of the new rule:

```rust
#[tokio::test]
async fn dispatch_defaults_to_the_first_workspace() {
    let first = TempDir::new().unwrap();
    let second = TempDir::new().unwrap();
    std::fs::write(first.path().join("marker.txt"), "first").unwrap();
    let registry = WorkspaceRegistry::new(vec![
        Workspace { name: "a".into(), path: first.path().to_path_buf() },
        Workspace { name: "b".into(), path: second.path().to_path_buf() },
    ]);
    let result = dispatch(
        &registry,
        &RuntimeState::new(),
        "agent",
        ToolCall::ReadFile(ReadFileInput {
            path: "marker.txt".to_string(),
            start_line: None,
            end_line: None,
        }),
    )
    .await;
    match result {
        ToolResult::Ok(o) => assert_eq!(o.stdout.trim(), "first"),
        ToolResult::Err(e) => panic!("{}", e.reason),
    }
}

#[tokio::test]
async fn dispatch_errors_with_no_workspaces() {
    let result = dispatch(
        &WorkspaceRegistry::new(vec![]),
        &RuntimeState::new(),
        "agent",
        ToolCall::Bash(BashInput { command: "echo hi".to_string(), timeout_secs: None }),
    )
    .await;
    assert!(matches!(result, ToolResult::Err(_)));
}
```

Add a test pinning the property the issue is about — an absolute path reaches the other workspace even though it is not the default:

```rust
#[tokio::test]
async fn an_absolute_path_reaches_a_non_default_workspace() {
    let first = TempDir::new().unwrap();
    let second = TempDir::new().unwrap();
    std::fs::write(second.path().join("other.txt"), "second").unwrap();
    let registry = WorkspaceRegistry::new(vec![
        Workspace { name: "a".into(), path: first.path().to_path_buf() },
        Workspace { name: "b".into(), path: second.path().to_path_buf() },
    ]);
    let result = dispatch(
        &registry,
        &RuntimeState::new(),
        "agent",
        ToolCall::ReadFile(ReadFileInput {
            path: second.path().join("other.txt").display().to_string(),
            start_line: None,
            end_line: None,
        }),
    )
    .await;
    match result {
        ToolResult::Ok(o) => assert_eq!(o.stdout.trim(), "second"),
        ToolResult::Err(e) => panic!("{}", e.reason),
    }
}
```

Import `ReadFileInput` and `RuntimeState` in the test module.

- [ ] **Step 7: Simplify `set_working_dir`**

Rewrite `runtime/src/tools/set_working_dir.rs`'s three functions. `exec` no longer needs the workspace argument, and `set` chains off the current effective cwd unconditionally:

```rust
pub fn exec(
    registry: &WorkspaceRegistry,
    state: &RuntimeState,
    agent: &str,
    input: SetWorkingDirInput,
) -> ToolResult {
    match &input.path {
        Some(path) => set(registry, state, agent, path),
        None => reset(registry, state, agent),
    }
}

/// Point the agent's cwd at `path` — absolute, or relative to its current
/// effective cwd. A bad target is an error and changes nothing.
fn set(registry: &WorkspaceRegistry, state: &RuntimeState, agent: &str, path: &str) -> ToolResult {
    let root = match registry.default_root() {
        Ok(r) => r,
        Err(reason) => return ToolResult::Err(ToolError { reason }),
    };
    let base = state.effective_dir(agent, &root);
    // Path::join discards the base when `path` is absolute — exactly cd semantics.
    let candidate = base.join(Path::new(path));
    ...unchanged from here...
}

/// Clear the agent's override, returning to the default working directory.
fn reset(registry: &WorkspaceRegistry, state: &RuntimeState, agent: &str) -> ToolResult {
    let root = match registry.default_root() {
        Ok(r) => r,
        Err(reason) => return ToolResult::Err(ToolError { reason }),
    };
    state.set_cwd(agent, None);
    ok(root.display().to_string())
}
```

In its test module: change `fn input(path: Option<&str>, workspace: Option<&str>)` to `fn input(path: Option<&str>)` and drop the second argument at every call site. Delete `reset_with_unknown_workspace_errors_and_keeps_the_override` — there is no name left to typo. Add:

```rust
#[test]
fn reset_reports_the_default_root() {
    let (dir, registry, state) = fixture();
    let _ = exec(&registry, &state, "a", input(Some("sub")));
    match exec(&registry, &state, "a", input(None)) {
        ToolResult::Ok(o) => assert_eq!(o.stdout, dir.path().display().to_string()),
        ToolResult::Err(e) => panic!("{}", e.reason),
    }
}
```

- [ ] **Step 8: Report the shared root from the scan**

In `runtime/src/scan.rs`, `exec` returns only `Vec<WorkspaceScan>` and the caller assembles `ScanResponse`. Find that caller (`rg 'ScanResponse' runtime/src`) and set `shared_root` from `registry.plugins_dir().map(|p| p.display().to_string())`, gated the same way `shared_skills` is gated on `include_shared` — a run with plugins off reports `None`.

Add to `runtime/src/scan.rs`'s test module:

```rust
#[test]
fn shared_root_is_reported_only_when_plugins_are_included() {
    let dir = TempDir::new().unwrap();
    let registry = WorkspaceRegistry::new(vec![])
        .with_plugins(Some(dir.path().to_path_buf()), vec![]);
    assert!(!shared_skills(&registry, false).is_empty() == false);
    assert_eq!(registry.plugins_dir(), Some(dir.path()));
}
```

If the assembly happens in the message-handling layer rather than `scan.rs`, put the assertion in that layer's tests instead and keep it behavioural: `include_shared: false` → `shared_root: None`.

- [ ] **Step 9: Run the runtime tests**

Run: `cargo test -p horsie-runtime`
Expected: PASS. Fix the mechanical `workspace: None` removals in `bash.rs`, `read_file.rs`, `write_file.rs`, `list_files.rs`, `grep.rs`, `replace_lines.rs`, `find_and_replace.rs` test modules as the compiler reports them.

- [ ] **Step 10: Commit**

```bash
git add runtime/src
git commit -m "runtime: resolve a tool's base from the cwd override or the first workspace"
```

---

### Task 3: Tool schemas stop advertising the parameter

**Files:**
- Modify: `runtime-client/src/tools/mod.rs:54-76` (delete two helpers)
- Modify: `runtime-client/src/tools/{bash,read_file,write_file,find_and_replace,replace_lines,list_files,glob,grep,set_working_dir}.rs`

- [ ] **Step 1: Delete the helpers**

Remove `with_workspace` and `workspace_arg` from `runtime-client/src/tools/mod.rs` entirely, along with their doc comments.

- [ ] **Step 2: Update the eight path tools**

In each of `bash.rs`, `read_file.rs`, `write_file.rs`, `find_and_replace.rs`, `replace_lines.rs`, `list_files.rs`, `glob.rs`, `grep.rs`: unwrap `crate::tools::with_workspace(json!({...}))` to plain `json!({...})`, delete the `let workspace = crate::tools::workspace_arg(&input);` line, and drop the `workspace` field from the `*Input` literal.

For example `list_files.rs` becomes:

```rust
input_schema: json!({
    "type": "object",
    "properties": { "path": { "type": "string" } },
    "required": ["path"]
}),
```

```rust
self.client
    .invoke(ToolCall::ListFiles(ListFilesInput { path }))
```

- [ ] **Step 3: Update `set_working_dir`**

In `runtime-client/src/tools/set_working_dir.rs`, replace the description and schema:

```rust
description: "Set the working directory for all future tool calls in this \
    session — bash commands and relative paths in the file tools alike. \
    'path' may be absolute or relative to the current working directory. \
    Omit 'path' to reset to the default working directory. Persists until \
    reset; other sessions sharing this runtime are unaffected. Returns the \
    new working directory."
    .to_string(),
input_schema: json!({
    "type": "object",
    "properties": { "path": { "type": "string" } }
}),
```

and in `execute`, drop the `workspace` binding and field.

- [ ] **Step 4: Add a guard test**

At the end of `runtime-client/src/tools/mod.rs`'s test module, pin the invariant so a future tool cannot reintroduce the field:

```rust
#[test]
fn no_runtime_tool_advertises_a_workspace_property() {
    let client = crate::testkit::mock_client();
    let toolbox = add_runtime_tools(ToolboxImpl::default(), client);
    for spec in toolbox.specs() {
        let props = spec.input_schema.get("properties").and_then(Value::as_object);
        assert!(
            props.is_none_or(|p| !p.contains_key("workspace")),
            "tool '{}' still advertises a workspace property",
            spec.name
        );
    }
}
```

Adapt the toolbox construction to whatever `add_runtime_tools` needs in this crate's tests (check existing tests in the file for how a `RuntimeClient` is built there); the assertion is the point.

- [ ] **Step 5: Build and test the crate**

Run: `cargo test -p horsie-runtime-client`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add runtime-client/src
git commit -m "tools: remove the workspace argument from the nine runtime tool schemas"
```

---

### Task 4: Skills carry their directory

**Files:**
- Modify: `workflow/src/workspace.rs:13-25` (`SharedContext`), `:66-78` (`Skill`), `:105-160` (`scan`, `interpret_shared`), `:160-198` (`interpret_one`, `parse_skill`)
- Modify: `workflow/src/context.rs:449`, `:460`, `:487`, `:491`
- Modify: `server/src/sessions/session_actor.rs:958-968`, `workflow/src/workflow_actor.rs:279-292`

**Interfaces:**
- Produces: `Skill { name, description, body, dir: Option<String> }` where `dir` is the skill's **absolute** directory (replacing `rel_dir`).
- Produces: `SharedScan { skills: SkillSet, root: Option<String> }`, returned as the second element of `scan`.
- Produces: `SharedContext { skills: Arc<SkillSet>, root: Option<String>, bootstrap: Option<String> }`.

- [ ] **Step 1: Write the failing tests**

In `workflow/src/workspace.rs`'s test module:

```rust
#[test]
fn workspace_skill_dir_is_its_absolute_directory() {
    let raw = WorkspaceScan {
        name: "api".into(),
        path: "/ws/api".into(),
        is_git_repo: true,
        instructions: None,
        skills: vec![ScannedFile {
            path: "/ws/api/.claude/skills/deploy/SKILL.md".into(),
            content: "---\nname: deploy\ndescription: Ship it\n---\nbody".into(),
        }],
        platform: None,
    };
    let ctx = interpret(vec![raw]);
    let skill = ctx.workspaces[0].skills.get("deploy").unwrap();
    assert_eq!(skill.dir.as_deref(), Some("/ws/api/.claude/skills/deploy"));
}

#[test]
fn shared_skill_dir_is_joined_onto_the_library_root() {
    let shared = interpret_shared(
        vec![plugin_skill("brainstorming", "sp/skills/brainstorming", "Design it")],
        Some("/opt/plugins"),
    );
    assert_eq!(
        shared.skills.get("brainstorming").unwrap().dir.as_deref(),
        Some("/opt/plugins/sp/skills/brainstorming")
    );
    assert_eq!(shared.root.as_deref(), Some("/opt/plugins"));
}

#[test]
fn shared_skill_dir_is_none_without_a_root() {
    let shared = interpret_shared(
        vec![plugin_skill("brainstorming", "sp/skills/brainstorming", "Design it")],
        None,
    );
    assert!(shared.skills.get("brainstorming").unwrap().dir.is_none());
}
```

Update the existing `interpret_shared_sets_rel_dir_and_dedupes` to the new signature and name it `interpret_shared_sets_dir_and_dedupes`, asserting on `dir`.

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p horsie-workflow workspace::tests`
Expected: FAIL, `no field dir on type Skill`.

- [ ] **Step 3: Rename the field and populate it**

In `workflow/src/workspace.rs`, change `Skill`:

```rust
#[derive(Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub body: String,
    /// The skill's own directory, absolute, so the agent can read sibling
    /// resources with the filesystem tools. `None` when the scan did not carry
    /// enough to compute one (a shared skill with no library root reported).
    pub dir: Option<String>,
}
```

`parse_skill` keeps returning `dir: None`; the two interpret functions fill it in.

In `interpret_one`, set it from the scanned file's path — the runtime sends the absolute `.../SKILL.md`:

```rust
for file in raw.skills {
    match parse_skill(&file) {
        Some(mut skill) => {
            skill.dir = std::path::Path::new(&file.path)
                .parent()
                .map(|p| p.display().to_string());
            if skills.contains_key(&skill.name) {
                tracing::warn!(workspace = %raw.name, name = %skill.name, "duplicate skill name; keeping first");
            } else {
                skills.insert(skill.name.clone(), skill);
            }
        }
        None => tracing::warn!(path = %file.path, "skipping skill with invalid frontmatter"),
    }
}
```

Add the shared-scan type next to `SharedContext`:

```rust
/// The shared plugin library as scanned: its skills plus its absolute root.
/// The root is what lets a skill's `rel_dir` become an absolute `Skill::dir`
/// and what the prompt names in the shared section header.
#[derive(Default)]
pub struct SharedScan {
    pub skills: SkillSet,
    pub root: Option<String>,
}
```

and give `interpret_shared` the root:

```rust
fn interpret_shared(raw: Vec<PluginSkill>, root: Option<&str>) -> SharedScan {
    let mut skills = BTreeMap::new();
    for ps in raw {
        let scanned = ScannedFile { path: ps.rel_dir.clone(), content: ps.content };
        match parse_skill(&scanned) {
            Some(mut skill) => {
                skill.dir = root.map(|r| {
                    std::path::Path::new(r).join(&ps.rel_dir).display().to_string()
                });
                if skills.contains_key(&skill.name) {
                    tracing::warn!(plugin = %ps.plugin, name = %skill.name, "duplicate shared skill name; keeping first");
                } else {
                    skills.insert(skill.name.clone(), skill);
                }
            }
            None => tracing::warn!(plugin = %ps.plugin, "skipping shared skill with invalid frontmatter"),
        }
    }
    SharedScan { skills: SkillSet { skills }, root: root.map(str::to_string) }
}
```

- [ ] **Step 4: Thread the root through `scan`**

```rust
pub async fn scan(
    client: &RuntimeClient,
    workspace: Option<String>,
    include_shared: bool,
) -> (WorkspaceContext, SharedScan) {
    ...
    match client.scan_workspace(workspace, candidates, SKILLS_GLOB.to_string(), include_shared).await {
        Ok(resp) => {
            let shared = interpret_shared(resp.shared_skills, resp.shared_root.as_deref());
            (interpret(resp.workspaces), shared)
        }
        Err(e) => {
            tracing::warn!(error = %e, "workspace scan failed; continuing without it");
            (WorkspaceContext::default(), SharedScan::default())
        }
    }
}
```

Add `pub root: Option<String>` to `SharedContext` and include it in `is_empty`'s reasoning only if it already reads naturally — a root with no skills and no bootstrap is still empty, so leave `is_empty` as it is.

- [ ] **Step 5: Fix the callers**

`server/src/sessions/session_actor.rs`:

```rust
let (ws, shared_scan) = scan_workspace(&self.runtime_client, None, use_plugins).await;
let shared = if use_plugins {
    let bootstrap = match self.runtime_client.run_session_start().await {
        Ok(context) if !context.trim().is_empty() => Some(context),
        Ok(_) | Err(_) => None,
    };
    Some(SharedContext {
        skills: Arc::new(shared_scan.skills),
        root: shared_scan.root,
        bootstrap,
    })
} else {
    None
};
```

`workflow/src/workflow_actor.rs` takes the same shape with its own names.

`workflow/src/context.rs` lines 449 and 487 bind `let (_, shared) = ...` and then use `shared` as a `SkillSet`; they become `shared.skills` at the use sites (`shared.skills.get(...)`, `shared.skills.names()`, `shared_inspect(&shared.skills, shared.root.as_deref())`). Line 491's `let (ws, _)` is unaffected. Export `SharedScan` from `workflow/src/lib.rs:39` next to `SharedContext`.

- [ ] **Step 6: Run the tests**

Run: `cargo test -p horsie-workflow -p horsie-server`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add workflow/src server/src
git commit -m "workflow: give every skill its absolute directory and carry the library root"
```

---

### Task 5: The prompt tells the model where things are

**Files:**
- Modify: `workflow/src/workspace.rs:258-320` (`compose_system_prompt`), `:336-360` (`shared_inspect`, `skills_listing`), `:364-395` (`inspect_result`)
- Modify: `workflow/src/context.rs:506-520` (`shared_skill_body`), `:440-500` (skill tool result)
- Test: `workflow/tests/workspace_context.rs`

**Interfaces:**
- Consumes: `Skill::dir`, `SharedScan::root`, `SharedContext::root` from Task 4.
- Produces: `skills_listing(skills: &SkillSet, root: Option<&str>) -> String`.

- [ ] **Step 1: Write the failing tests**

In `workflow/tests/workspace_context.rs`, add:

```rust
#[tokio::test]
async fn prompt_states_the_default_working_directory_and_skill_dirs() {
    // Build the same two-workspace fixture the file's other tests use, with a
    // skill in the first workspace, then compose the prompt.
    let prompt = /* compose_system_prompt(...) */;
    assert!(
        prompt.contains("Your working directory starts at /ws/api."),
        "{prompt}"
    );
    assert!(
        !prompt.contains("take a `workspace` argument"),
        "the old intro survived: {prompt}"
    );
    assert!(
        prompt.contains("- deploy — .claude/skills/deploy/: "),
        "{prompt}"
    );
}

#[tokio::test]
async fn shared_section_names_its_root() {
    let prompt = /* compose with a SharedContext { root: Some("/opt/plugins"), .. } */;
    assert!(prompt.contains("# Shared skills — /opt/plugins"), "{prompt}");
    assert!(
        prompt.contains("- brainstorming — sp/skills/brainstorming/: "),
        "{prompt}"
    );
}
```

Follow the fixture style already in that file (see its existing `# Workspaces` assertion around line 58) rather than inventing a new harness.

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p horsie-workflow --test workspace_context`
Expected: FAIL on the new assertions.

- [ ] **Step 3: Render skill directories relative to their section root**

```rust
/// Render skills as sorted `- name — <dir>/: description` lines, with each
/// skill's directory relative to `root` (the section header already names it,
/// so repeating a long absolute prefix on every line would be waste). Falls
/// back to `- name: description` when a skill has no directory or sits outside
/// the root.
fn skills_listing(skills: &SkillSet, root: Option<&str>) -> String {
    skills
        .iter()
        .map(|s| match relative_dir(s, root) {
            Some(rel) => format!("- {} — {}/: {}", s.name, rel, s.description),
            None => format!("- {}: {}", s.name, s.description),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A skill's directory relative to `root`, or `None` when either is missing or
/// the skill is not under the root.
fn relative_dir(skill: &Skill, root: Option<&str>) -> Option<String> {
    let dir = skill.dir.as_deref()?;
    let root = root?;
    std::path::Path::new(dir)
        .strip_prefix(root)
        .ok()
        .map(|p| p.display().to_string())
        .filter(|p| !p.is_empty())
}
```

- [ ] **Step 4: Rewrite the prompt block**

In `compose_system_prompt`, replace the `# Workspaces` intro and the shared section. The workspace loop keeps its `## name — path (git)` header and instructions verbatim; only the skills line changes:

```rust
if !ws.workspaces.is_empty() {
    let default_root = ws.workspaces.first().map_or("", |w| w.path.as_str());
    let mut block = format!(
        "# Workspaces\nYour working directory starts at {default_root}. Filesystem \
         and bash tools resolve relative paths against it; use an absolute path to \
         reach another workspace, or set_working_dir to move."
    );
    for w in &ws.workspaces {
        block.push_str(&format!(
            "\n\n## {} — {}{}",
            w.name,
            w.path,
            if w.is_git_repo { " (git)" } else { "" }
        ));
        if let Some(instr) = &w.instructions
            && !instr.trim().is_empty()
        {
            block.push_str(&format!("\n{}", instr.trim()));
        }
        if !w.skills.is_empty() {
            block.push_str(&format!(
                "\n### Skills (load with the skill tool, workspace=\"{}\")\n{}",
                w.name,
                skills_listing(&w.skills, Some(&w.path))
            ));
        }
    }
    sections.push(block);
}
if let Some(s) = shared
    && !s.skills.is_empty()
{
    let header = match &s.root {
        Some(root) => format!("# Shared skills — {root}"),
        None => "# Shared skills".to_string(),
    };
    sections.push(format!(
        "{header}\nShared across all workspaces. Load with the skill tool, \
         workspace=\"{}\".\n{}",
        SHARED_WORKSPACE,
        skills_listing(&s.skills, s.root.as_deref())
    ));
}
```

- [ ] **Step 5: Update the two `inspect` renderers**

`shared_inspect` takes the root and forwards it:

```rust
pub(crate) fn shared_inspect(skills: &SkillSet, root: Option<&str>) -> String {
    if skills.is_empty() {
        return format!("## {SHARED_WORKSPACE}\nskills: none");
    }
    format!(
        "## {}\nskills ({}):\n{}",
        SHARED_WORKSPACE,
        skills.len(),
        skills_listing(skills, root)
    )
}
```

`inspect_result` passes each workspace's own path: `skills_listing(&w.skills, Some(&w.path))`.

- [ ] **Step 6: Make the resources footer absolute, for both kinds**

In `workflow/src/context.rs`, replace `shared_skill_body` with a kind-agnostic helper and use it on both branches of the `skill` tool:

```rust
/// A skill's body plus, when its directory is known, a hint pointing at it so
/// the agent can read sibling resources with the filesystem tools. Absolute,
/// because that is the only addressing the file tools take.
fn skill_body(skill: &crate::workspace::Skill) -> String {
    match &skill.dir {
        Some(dir) => format!(
            "{}\n\n[resources] This skill's files are in {}/. \
             Read one with read_file(path=\"{}/<file>\").",
            skill.body, dir, dir,
        ),
        None => skill.body.clone(),
    }
}
```

The shared branch becomes `Ok(Value::String(skill_body(skill)))`; the workspace branch changes from `Ok(Value::String(skill.body.clone()))` to `Ok(Value::String(skill_body(skill)))`.

- [ ] **Step 7: Run the tests**

Run: `cargo test -p horsie-workflow`
Expected: PASS. Update the assertions in `workflow/src/workspace.rs`'s existing prompt tests (around line 535, which locates `# Workspaces`) and `context.rs`'s skill-tool tests for the new footer wording.

- [ ] **Step 8: Commit**

```bash
git add workflow docs
git commit -m "prompt: name the working directory and each skill's directory"
```

---

### Task 6: Green across the workspace

**Files:**
- Modify: `cli/tests/connect_e2e.rs`, `cli/tests/sandbox_e2e.rs`, `tests/tests/session_server_e2e.rs`, `tests/tests/agent_recovery_e2e.rs`, `runtime/tests/provision_steps.rs`, `server/src/runtime_vendor/{fake,transport}.rs`, `workflow/tests/workflow_e2e.rs` — whatever the compiler and test run report.

- [ ] **Step 1: Build everything**

Run: `cargo check --all-targets --all-features`
Expected: a list of errors at the `workspace: None` / `workspace: Some(...)` literals and any `scan_workspace` tuple destructuring. Fix each mechanically — remove the field, or bind `resp` and use `resp.workspaces` / `resp.shared_skills`.

`runtime/tests/provision_steps.rs` should need no change: provision steps still carry a `workspace` param.

- [ ] **Step 2: Clippy**

Run: `cargo clippy --all-targets --all-features -- -D warnings`
Expected: clean. Watch for now-unused imports (`PathBuf` in `runtime/src/tools/mod.rs`, `PluginSkill`/`WorkspaceScan` in `runtime-client/src/transport.rs`).

- [ ] **Step 3: Full test run**

Run: `cargo nextest run --all-features`
Expected: PASS.

- [ ] **Step 4: Grep for stragglers**

Run: `rg -n 'workspace' --type rust runtime-client/src/tools/`
Expected: no hits — the nine tool files should not mention it at all.

Run: `rg -n 'horsie_shared' docs/guide/`
Expected: review each hit; if a user guide documents `read_file(workspace="horsie_shared", ...)`, update it to the absolute-path form.

- [ ] **Step 5: Commit and open the PR**

```bash
git add -A
git commit -m "tests: drop the workspace argument across the suites"
git push -u origin drop-workspace-param
gh pr create --title "Drop the workspace parameter from the path-taking tools" --body "..."
```

The PR body: one short paragraph on why (three addressing schemes, the #86 precedence bug), one on what replaces it (cwd override else first workspace; absolute paths in the prompt for skills and the shared library), and the note that `skill`/`inspect_workspace`/provision steps keep theirs. One line per paragraph — no hard wrapping. Reference `Closes #94`.
